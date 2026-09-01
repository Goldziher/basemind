//! MCP server per-connection state: [`ServerState`] (identity + a shared read stack), its
//! [`Lifecycle`] classifier, and the [`MapCache`] read stack over the indexed corpus.
//!
//! The heavy, workspace-level read fields live on [`SharedReadStack`](super::SharedReadStack)
//! (in `shared_state.rs`) so one stack can be shared — via an [`Arc`] — across many connections.
//! `ServerState` keeps only what is specific to a single connection: the resolved agent identity,
//! that identity's lazily-connected broker clients, and the client's requested log verbosity.

use std::sync::Arc;

#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) const DELIVERED_NOTIFICATION_CAP: usize = 4_096;

#[cfg(feature = "documents")]
use super::codegraph;
use super::{SharedReadStack, helpers_calls, helpers_impls, l1_cache, map_fingerprint, types};
use crate::extract::FileMapL1;
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
    /// [`MapCache::build`] is IO-bound (a rayon `par_iter` probing every indexed file's blob), so it
    /// must not run on the async reactor. It is handed to [`tokio::task::block_in_place`], which requires
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
                let budget = l1_cache::budget_bytes_from(&self.shared.config.resources);
                let mut cache = if multi_thread {
                    tokio::task::block_in_place(|| MapCache::build(&store, budget))
                } else {
                    MapCache::build(&store, budget)
                };
                // Attach persisted doc↔code links (ADR-0008) before publish; `attach_async` offloads
                // the LanceStore read to a blocking thread so its block_on never nests on the reactor.
                super::doc_links_cache::attach_async(&mut cache, &store, &self.shared.config, &self.shared.scope).await;
                let files = cache.len();
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
        types::LifecycleNotice::for_state(self.lifecycle()).or_else(|| {
            // A read-only session whose projected call/impl indexes were capped answers reference
            // queries from a truncated set. Reported through the same channel as the warming/
            // rescanning states so a caller never reads an incomplete result as "no matches".
            self.shared
                .cache
                .load()
                .projections_capped
                .then(types::LifecycleNotice::projections_capped)
        })
    }
}

pub(crate) struct MapCache {
    /// Symbol-free view of every indexed file. Permanent and O(files) — never O(symbols).
    files: l1_cache::FileIndexView,
    /// The only place a decoded [`FileMapL1`] lives: a byte-charged, read-through LRU keyed by
    /// content hash. Shared with the snapshot this cache was derived from, so an incremental
    /// rescan is a refcount bump rather than a rebuild.
    l1: Arc<l1_cache::L1Cache>,
    /// In-RAM callee index, populated ONLY when the Fjall index is unavailable —
    /// i.e. a read-only `serve` session that lost the single-holder lock to another
    /// process, or a `daemon_writer` front-end whose store is index-less. Lets
    /// `find_references` / `find_callers` / `call_graph` answer from the shared L2
    /// blobs so multiple sessions can use one repo at once. `None` on a writer
    /// session, which uses the live Fjall index (no extra RAM/build cost).
    pub(crate) calls: Option<helpers_calls::InRamCallIndex>,
    /// In-RAM trait→impl index, same read-only-only gating as `calls`. Backs
    /// `find_implementations` from the L1 blobs when Fjall is held elsewhere.
    pub(crate) impls: Option<helpers_impls::InRamImplIndex>,
    /// True when a read-only session's projected call/impl indexes hit their byte cap and were
    /// truncated, so the answers they back are incomplete. Surfaced through
    /// [`ServerState::lifecycle_notice`] rather than silently degrading a result to "no matches".
    pub(crate) projections_capped: bool,
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
    /// Build the read stack's map over `store`, charging decoded L1s against `budget_bytes`
    /// (`0` = unbounded).
    ///
    /// Unlike the whole-corpus map this replaces, `build` decodes NO L1 blobs: it projects the
    /// index into [`l1_cache::FileIndexView`] and leaves the outlines to be faulted in on demand.
    /// The read-only projections (`calls` / `impls`) are the exception — a session with no Fjall
    /// index has nothing else to answer reference queries from — and they are charged against the
    /// same budget.
    pub(crate) fn build(store: &Store, budget_bytes: u64) -> Self {
        let files = l1_cache::FileIndexView::build(store);
        let l1 = Arc::new(l1_cache::L1Cache::new(store.blobs_dir.clone(), budget_bytes));
        let mut projections_capped = false;
        let (calls, impls) = if store.index_db.is_none() {
            let calls = helpers_calls::InRamCallIndex::build(store, budget_bytes);
            let impls = helpers_impls::InRamImplIndex::build(&files, &l1, budget_bytes);
            projections_capped = calls.capped() || impls.capped();
            (Some(calls), Some(impls))
        } else {
            (None, None)
        };
        Self {
            fingerprint: map_fingerprint::index_fingerprint(store),
            files,
            l1,
            calls,
            impls,
            projections_capped,
            #[cfg(feature = "documents")]
            doc_links: Default::default(),
        }
    }

    /// An empty map cache: the placeholder a `serve` boots with while the real [`build`](Self::build)
    /// runs in the background (see [`super::background::spawn_cache_warm`]). Deferring the index
    /// projection off the startup path is what lets the MCP `initialize`/`tools/list` handshake
    /// answer immediately. Cache-reading tools await [`ServerState::await_cache_ready`] before
    /// reading, so they observe the built map, never this placeholder.
    pub(crate) fn empty() -> Self {
        Self {
            fingerprint: 0,
            files: l1_cache::FileIndexView::default(),
            l1: Arc::new(l1_cache::L1Cache::placeholder()),
            calls: None,
            impls: None,
            projections_capped: false,
            #[cfg(feature = "documents")]
            doc_links: Default::default(),
        }
    }

    /// Build a cache over a synthetic, blob-less corpus. Test-only seam: the graph lanes are
    /// exercised against hand-built outlines that no scan produced, so there is no store to read
    /// them back from and they are seeded straight into the L1 cache.
    #[cfg(test)]
    pub(crate) fn from_synthetic(files: Vec<(crate::path::RelPath, FileMapL1)>) -> Self {
        let l1 = Arc::new(l1_cache::L1Cache::new(std::path::PathBuf::new(), 0));
        let mut metas: Vec<(crate::path::RelPath, l1_cache::FileMeta)> = Vec::with_capacity(files.len());
        for (path, map) in files {
            let hash_hex = format!("synthetic-{}", path.to_str_lossy());
            let meta = l1_cache::FileMeta {
                hash_hex: hash_hex.as_str().into(),
                language: map.language.as_str().into(),
                size_bytes: map.size_bytes,
            };
            l1.seed(&hash_hex, Arc::new(map));
            metas.push((path, meta));
        }
        Self {
            fingerprint: 0,
            files: l1_cache::FileIndexView::from_pairs(metas),
            l1,
            calls: None,
            impls: None,
            projections_capped: false,
            #[cfg(feature = "documents")]
            doc_links: Default::default(),
        }
    }

    /// Number of indexed files this map covers.
    pub(crate) fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether `path` is one of the indexed files. Answered from the symbol-free view, so it costs
    /// no blob read. Gated on the features whose lanes ask the question (see
    /// [`l1_cache::FileIndexView::contains`]).
    #[cfg(any(test, feature = "documents", feature = "memory"))]
    pub(crate) fn contains(&self, path: &crate::path::RelPath) -> bool {
        self.files.contains(path)
    }

    /// The language recorded for `path` at scan time, without faulting in its outline.
    pub(crate) fn language_of(&self, path: &crate::path::RelPath) -> Option<&str> {
        self.files.get(path).map(|meta| &*meta.language)
    }

    /// Every indexed path, in sorted order.
    pub(crate) fn paths(&self) -> impl Iterator<Item = &crate::path::RelPath> {
        self.files.keys()
    }

    /// Every indexed path with its symbol-free metadata, in sorted order. The whole surface a
    /// path/language/size filter needs, with no L1 decode anywhere.
    pub(crate) fn file_metas(&self) -> impl Iterator<Item = (&crate::path::RelPath, &l1_cache::FileMeta)> {
        self.files.iter()
    }

    /// The decoded outline for one file — the point-lookup path. A hit is a pointer clone; a miss
    /// is one blob read, which is exactly what the old whole-corpus build paid per file up front.
    pub(crate) fn get(&self, path: &crate::path::RelPath) -> Option<Arc<FileMapL1>> {
        let meta = self.files.get(path)?;
        self.l1.load(&meta.hash_hex)
    }

    /// Stream every `(path, outline)` in path order, stopping early when `f` returns `false`.
    ///
    /// The whole-corpus read primitive. Consumers project into a compact structure and drop the
    /// outline, so the corpus is never simultaneously materialised — the invariant the old
    /// `for (path, l1) in &cache.by_path` loop violated by construction.
    pub(crate) fn for_each_while<F>(&self, f: F)
    where
        F: FnMut(&crate::path::RelPath, &FileMapL1) -> bool,
    {
        l1_cache::stream_while(&self.files, &self.l1, f);
    }

    /// [`for_each_while`](Self::for_each_while) for a consumer that always runs to completion.
    pub(crate) fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(&crate::path::RelPath, &FileMapL1),
    {
        self.for_each_while(|path, l1| {
            f(path, l1);
            true
        });
    }

    /// The L1 cache backing this map. The budget tests read its counters.
    #[cfg(test)]
    pub(crate) fn l1_cache(&self) -> &l1_cache::L1Cache {
        &self.l1
    }

    /// Incrementally derive a fresh cache from `self` for a **scoped** (watcher) rescan: patch the
    /// file view for the changed paths only and carry the L1 cache forward by refcount. This avoids
    /// `build`'s whole-corpus index projection on every debounced batch, which is what pegged
    /// multi-core CPU on gitignored / nested-`.basemind` churn (issue #33).
    ///
    /// Sharing the L1 cache across the two snapshots is safe because it is keyed by CONTENT HASH:
    /// an updated path's new view entry names a new hash, so a reader still holding `self` keeps
    /// resolving the old hash and cannot observe the newer content.
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
        if self.calls.is_some() || self.impls.is_some() {
            // Carry doc↔code links (ADR-0008) forward from the previous cache rather than dropping
            // them: a degraded full rebuild has no off-reactor context to reload the LanceStore.
            #[cfg(feature = "documents")]
            {
                let mut rebuilt = Self::build(store, self.l1.budget_bytes());
                rebuilt.doc_links = std::sync::Arc::clone(&self.doc_links);
                return rebuilt;
            }
            #[cfg(not(feature = "documents"))]
            return Self::build(store, self.l1.budget_bytes());
        }
        Self {
            fingerprint: map_fingerprint::index_fingerprint(store),
            files: self.files.with_delta(store, updated, removed),
            l1: Arc::clone(&self.l1),
            calls: None,
            impls: None,
            projections_capped: self.projections_capped,
            #[cfg(feature = "documents")]
            doc_links: std::sync::Arc::clone(&self.doc_links),
        }
    }
}

#[cfg(test)]
#[path = "map_cache_budget_tests.rs"]
mod map_cache_budget_tests;
