//! MCP server per-connection state: [`ServerState`] (identity + a shared read stack), its
//! [`Lifecycle`] classifier, and the in-RAM [`MapCache`] over every indexed file's L1 blob.
//!
//! The heavy, workspace-level read fields live on [`SharedReadStack`](super::SharedReadStack)
//! (in `shared_state.rs`) so one stack can be shared — via an [`Arc`] — across many connections.
//! `ServerState` keeps only what is specific to a single connection: the resolved agent identity,
//! that identity's lazily-connected broker clients, and the client's requested log verbosity.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) const DELIVERED_NOTIFICATION_CAP: usize = 4_096;

#[cfg(feature = "documents")]
use super::codegraph;
use super::{SharedReadStack, helpers_calls, helpers_impls, map_fingerprint, types};
use crate::extract::{FileMapL1, Import};
use crate::store::Store;

/// Per-connection MCP server state.
///
/// Holds one [`Arc<SharedReadStack>`](super::SharedReadStack) (the heavy read state, shared across
/// connections) plus this connection's own identity. Field accesses to the shared read stack go
/// through [`shared`](Self::shared); the identity fields (`agent_id`, `comms_clients`, `log_level`)
/// are read directly.
pub(crate) struct ServerState {
    /// The workspace-level read stack shared across every connection to this `(root, view)`.
    pub(crate) shared: Arc<SharedReadStack>,
    /// Owner segment for the individual-memory tier. Resolved once at boot by
    /// [`crate::comms::identity`] (validated through [`crate::comms::ids::AgentId`] so it is
    /// NUL-free), which never yields a shared constant — so two sessions cannot land on one
    /// memory owner. Group-tier writes ignore it. Per-connection identity.
    #[allow(dead_code)]
    pub(crate) agent_id: String,
    /// Per-identity registry of lazily-connected comms-broker clients, keyed by `AgentId`. The
    /// server's own identity (`agent_id`) connects directly; a sub-identity (driven via a tool's
    /// `as_agent` param) gets its own broker connection, so one `serve` process can act as many
    /// named agents. Entries are created on first use; a connect failure surfaces as an MCP error
    /// on the triggering call, never at server boot. Per-connection identity.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) comms_clients: tokio::sync::Mutex<
        ahash::AHashMap<
            crate::comms::ids::AgentId,
            std::sync::Arc<tokio::sync::Mutex<crate::comms::client::CommsClient>>,
        >,
    >,
    /// Bounded per-session high-water cache for mailbox notices piggybacked onto tool responses.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) delivered_notifications: tokio::sync::Mutex<lru::LruCache<String, ()>>,
    /// Minimum logging severity the client asked for via `logging/setLevel`, as an ordinal
    /// (see [`super::notifications::level_ordinal`]). Defaults to `Info`. Checked before every log emit so
    /// the server honors the client's verbosity preference. Per-connection.
    pub(crate) log_level: std::sync::atomic::AtomicU8,
    /// Whether this server advertises the lean three-tool surface (see [`super::lean`]). Resolved
    /// ONCE at construction from `BASEMIND_MCP_LEAN` (the value does not change mid-session), so the
    /// decision is stable and per-server rather than a per-request global-env read — which lets
    /// several servers with different surfaces coexist in one process (integration tests). The
    /// in-memory test serve overrides it directly.
    pub(crate) lean: std::sync::atomic::AtomicBool,
}

impl ServerState {
    /// Build a fresh per-connection state that shares the given [`SharedReadStack`].
    ///
    /// Each connection gets its own identity: a fresh (empty) `comms_clients` registry and the
    /// default log verbosity. Not yet wired to a caller — the seam a future daemon-hosted transport
    /// uses to hand every accepted connection the one shared read stack.
    #[allow(dead_code)]
    pub(crate) fn for_connection(shared: Arc<SharedReadStack>, agent_id: String) -> Self {
        Self {
            shared,
            agent_id,
            #[cfg(all(feature = "comms", any(unix, windows)))]
            comms_clients: tokio::sync::Mutex::new(ahash::AHashMap::new()),
            #[cfg(all(feature = "comms", any(unix, windows)))]
            delivered_notifications: tokio::sync::Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(DELIVERED_NOTIFICATION_CAP)
                    .expect("notification cache capacity is non-zero"),
            )),
            log_level: std::sync::atomic::AtomicU8::new(super::notifications::DEFAULT_LOG_ORDINAL),
            lean: std::sync::atomic::AtomicBool::new(super::lean::lean_mode_enabled()),
        }
    }
}

/// Upper bound a cache-reading tool waits for the deferred boot preload to finish before serving from
/// whatever is loaded so far. Sized so a normal repo's preload (seconds) completes within the wait — a
/// query issued right after the handshake returns COMPLETE data — while a pathologically large tree
/// still can't hang a call indefinitely (it returns partial results labelled with a warming notice).
pub(crate) const CACHE_WARM_WAIT_CAP: std::time::Duration = std::time::Duration::from_secs(15);

/// Coarse server lifecycle state surfaced to clients so an empty/partial result is never mistaken for
/// "no matches". Precedence (highest first): [`BuildingIndex`](Lifecycle::BuildingIndex) (a from-scratch
/// scan is populating the index) > [`WarmingUp`](Lifecycle::WarmingUp) (blobs are loading into RAM) >
/// [`Rescanning`](Lifecycle::Rescanning) (a watcher-driven incremental refresh is in flight) >
/// [`Ready`](Lifecycle::Ready).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lifecycle {
    Ready,
    WarmingUp,
    BuildingIndex,
    Rescanning,
}

impl Lifecycle {
    /// Pure precedence classifier over the three lifecycle flags. Highest first: `building`
    /// (from-scratch index scan), then `warming` (blobs loading into RAM), then `rescanning`
    /// (watcher refresh), then `Ready`. Split out from [`ServerState::lifecycle`] so the precedence
    /// is unit-testable without constructing a server.
    pub(crate) fn from_flags(building: bool, warming: bool, rescanning: bool) -> Self {
        if building {
            Lifecycle::BuildingIndex
        } else if warming {
            Lifecycle::WarmingUp
        } else if rescanning {
            Lifecycle::Rescanning
        } else {
            Lifecycle::Ready
        }
    }
}

impl ServerState {
    /// Current [`Lifecycle`] derived from the boot/rescan atomics, applying the documented precedence.
    pub(crate) fn lifecycle(&self) -> Lifecycle {
        use std::sync::atomic::Ordering::Relaxed;
        Lifecycle::from_flags(
            self.shared.initial_scan_active.load(Relaxed),
            self.shared.cache_warming.load(Relaxed),
            self.shared.rescan_active.load(Relaxed),
        )
    }

    /// Barrier every cache-reading tool crosses before it touches [`ServerState::cache`].
    ///
    /// Two regimes:
    ///
    /// * **One-shot ([`lazy_cache`](Self::lazy_cache))** — build the map HERE, on demand, once. The
    ///   cost then falls only on tools that actually read the map; `repo_info` / `status` / the git
    ///   tools never reach this barrier and so never pay it.
    /// * **`serve`** — wait for the background preload to publish the full map, bounded by
    ///   [`CACHE_WARM_WAIT_CAP`]. No-op once warm (the common path).
    ///
    /// Neither regime waits on [`Lifecycle::BuildingIndex`] — a from-scratch scan can run for
    /// minutes, so those tools return the partial index plus a
    /// [`lifecycle_notice`](Self::lifecycle_notice) telling the client to poll.
    pub(crate) async fn await_cache_ready(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        if self.shared.lazy_cache {
            self.build_cache_on_demand().await;
            return;
        }
        if !self.shared.cache_warming.load(Relaxed) {
            return;
        }
        let notified = self.shared.cache_ready.notified();
        if !self.shared.cache_warming.load(Relaxed) {
            return;
        }
        let _ = tokio::time::timeout(CACHE_WARM_WAIT_CAP, notified).await;
    }

    /// Build the whole-corpus in-RAM map and publish it — exactly once, however many callers race
    /// the barrier.
    ///
    /// [`MapCache::build`] is CPU- and IO-bound (a rayon `par_iter` over every L1 blob), so it must
    /// not run on the async reactor. It is handed to [`tokio::task::block_in_place`], which requires
    /// the multi-thread runtime the CLI builds; off one (a `current_thread` test) we call it
    /// directly, which is safe precisely because such a runtime has no other task to starve. Mirrors
    /// the runtime check in [`crate::git_history::remote`].
    async fn build_cache_on_demand(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.shared
            .lazy_cache_built
            .get_or_init(|| async {
                let started = std::time::Instant::now();
                let store = self.shared.store.read().await;
                let multi_thread = tokio::runtime::Handle::try_current()
                    .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
                    .unwrap_or(false);
                let mut cache = if multi_thread {
                    tokio::task::block_in_place(|| MapCache::build(&store))
                } else {
                    MapCache::build(&store)
                };
                // Attach persisted doc↔code links (ADR-0008) before publish; `attach_async` offloads
                // the LanceStore read to a blocking thread so its block_on never nests on the reactor.
                super::doc_links_cache::attach_async(&mut cache, &store, &self.shared.config, &self.shared.scope).await;
                let files = cache.by_path.len();
                self.shared.cache.store(Arc::new(cache));
                self.shared.cache_generation.fetch_add(1, Relaxed);
                let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                self.shared.cache_warm_ms.store(elapsed_ms, Relaxed);
                tracing::debug!(files, elapsed_ms, "in-RAM code map built on demand");
            })
            .await;
    }

    /// A [`LifecycleNotice`](types::LifecycleNotice) to attach to a tool response, or `None` when
    /// [`Ready`](Lifecycle::Ready). Lets every read tool label a possibly-incomplete result with the
    /// server state + an actionable message, so an agent knows to retry rather than concluding "empty".
    pub(crate) fn lifecycle_notice(&self) -> Option<types::LifecycleNotice> {
        types::LifecycleNotice::for_state(self.lifecycle())
    }
}

pub(crate) struct MapCache {
    /// path → L1 (kept sorted by path; iteration order matches `list_files`)
    pub(crate) by_path: BTreeMap<crate::path::RelPath, FileMapL1>,
    /// Pre-flattened `(path, imports)` view used by the `dependents` tool. Without this,
    /// every `dependents` call rebuilds the same `HashMap<PathBuf, Vec<Import>>` from
    /// scratch. Precomputing once at server boot drops that to pure pointer-chase.
    pub(crate) imports_index: Vec<(PathBuf, Vec<Import>)>,
    /// In-RAM callee index, populated ONLY when the Fjall index is unavailable —
    /// i.e. a read-only `serve` session that lost the single-holder lock to another
    /// process. Lets `find_references` / `find_callers` / `call_graph` answer from
    /// the shared L2 blobs so multiple sessions can use one repo at once. `None` on
    /// a writer session, which uses the live Fjall index (no extra RAM/build cost).
    pub(crate) calls: Option<helpers_calls::InRamCallIndex>,
    /// In-RAM trait→impl index, same read-only-only gating as `calls`. Backs
    /// `find_implementations` from the L1 blobs when Fjall is held elsewhere.
    pub(crate) impls: Option<helpers_impls::InRamImplIndex>,
    /// Fingerprint of the indexed file set this map was built from — see
    /// [`map_fingerprint::index_fingerprint`]. The refresh paths compare it against a freshly
    /// reopened store and SKIP the whole-corpus rebuild when it matches, which is what keeps a
    /// no-op daemon scan from transiently doubling serve's resident memory. `0` on the
    /// [`empty`](Self::empty) boot placeholder, which never matches a populated index.
    pub(crate) fingerprint: u64,
    /// Persisted document→code links (ADR-0008), loaded from the LanceDB document store by the async
    /// cache-warm path ([`super::background::spawn_cache_warm`]) and the view watcher. Empty until
    /// loaded; the codegraph `documents` lane reads it. Preserved across incremental
    /// [`with_delta`](Self::with_delta) rescans. `Arc<[_]>` so a scoped rescan carries the links
    /// forward with a refcount bump, not a deep clone of the whole vector.
    #[cfg(feature = "documents")]
    pub(crate) doc_links: std::sync::Arc<[codegraph::DocLink]>,
}

impl MapCache {
    pub(crate) fn build(store: &Store) -> Self {
        use rayon::prelude::*;

        let by_path: BTreeMap<crate::path::RelPath, FileMapL1> = store
            .index
            .files
            .par_iter()
            .filter_map(|(path, entry)| {
                store
                    .read_l1_by_hex(&entry.hash_hex)
                    .ok()
                    .flatten()
                    .map(|l1| (path.clone(), l1))
            })
            .collect();
        let imports_index: Vec<(PathBuf, Vec<Import>)> = by_path
            .par_iter()
            .map(|(p, l1)| (p.to_path_buf(), l1.imports.clone()))
            .collect();
        let (calls, impls) = if store.index_db.is_none() {
            (
                Some(helpers_calls::InRamCallIndex::build(store)),
                Some(helpers_impls::InRamImplIndex::build(&by_path)),
            )
        } else {
            (None, None)
        };
        Self {
            fingerprint: map_fingerprint::index_fingerprint(store),
            by_path,
            imports_index,
            calls,
            impls,
            #[cfg(feature = "documents")]
            doc_links: Default::default(),
        }
    }

    /// An empty map cache: the placeholder a `serve` boots with while the real [`build`](Self::build)
    /// runs in the background (see [`super::background::spawn_cache_warm`]). Deferring the whole-corpus blob
    /// load off the startup path is what lets the MCP `initialize`/`tools/list` handshake answer
    /// immediately instead of blocking on a rayon `par_iter` that a loaded machine can starve for
    /// minutes. Cache-reading tools await [`ServerState::cache_ready`] before reading, so they observe
    /// the fully-built map, never this placeholder.
    pub(crate) fn empty() -> Self {
        Self {
            fingerprint: 0,
            by_path: BTreeMap::new(),
            imports_index: Vec::new(),
            calls: None,
            impls: None,
            #[cfg(feature = "documents")]
            doc_links: Default::default(),
        }
    }

    /// Incrementally derive a fresh cache from `self` for a **scoped** (watcher) rescan: clone the
    /// existing maps and patch only the changed entries — re-read L1 from disk for `updated` paths,
    /// drop `removed` paths. This avoids `build`'s whole-corpus blob I/O (read + msgpack-decode of
    /// every L1, the dominant cost) on every debounced batch, which is what pegged multi-core CPU on
    /// gitignored / nested-`.basemind` churn (issue #33). `imports_index` is rebuilt from the
    /// patched in-RAM `by_path` — a pure clone pass, no I/O.
    ///
    /// Only valid on a writer session, where `calls`/`impls` are `None` (a read-only fallback
    /// session serves those from the blobs and never reaches the rescan path — `scan_and_refresh`
    /// early-returns on `state.read_only`). If they are somehow present, fall back to a full rebuild
    /// rather than let the in-RAM call/impl indexes drift out of sync.
    pub(crate) fn with_delta(
        &self,
        store: &Store,
        updated: &[crate::path::RelPath],
        removed: &[crate::path::RelPath],
    ) -> Self {
        use rayon::prelude::*;
        if self.calls.is_some() || self.impls.is_some() {
            // Carry doc↔code links (ADR-0008) forward from the previous cache rather than dropping
            // them: a degraded full rebuild has no off-reactor context to reload the LanceStore.
            #[cfg(feature = "documents")]
            {
                let mut rebuilt = Self::build(store);
                rebuilt.doc_links = std::sync::Arc::clone(&self.doc_links);
                return rebuilt;
            }
            #[cfg(not(feature = "documents"))]
            return Self::build(store);
        }
        let mut by_path = self.by_path.clone();
        for p in removed {
            by_path.remove(p);
        }
        for p in updated {
            match store.index.files.get(p) {
                Some(entry) => {
                    if let Ok(Some(l1)) = store.read_l1_by_hex(&entry.hash_hex) {
                        by_path.insert(p.clone(), l1);
                    }
                }
                None => {
                    by_path.remove(p);
                }
            }
        }
        let imports_index: Vec<(PathBuf, Vec<Import>)> = by_path
            .par_iter()
            .map(|(p, l1)| (p.to_path_buf(), l1.imports.clone()))
            .collect();
        Self {
            fingerprint: map_fingerprint::index_fingerprint(store),
            by_path,
            imports_index,
            calls: None,
            impls: None,
            #[cfg(feature = "documents")]
            doc_links: std::sync::Arc::clone(&self.doc_links),
        }
    }
}
