//! MCP server runtime state: the shared [`ServerState`], its [`Lifecycle`] classifier, and
//! the in-RAM [`MapCache`] over every indexed file's L1 blob.
//!
//! Extracted from `mod.rs` to keep that file within the per-file size budget.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::RwLock;

use super::{OutlineCache, helpers_calls, helpers_impls, map_fingerprint, telemetry, types};
use crate::extract::{FileMapL1, Import};
use crate::store::Store;

pub(crate) struct ServerState {
    pub(crate) store: RwLock<Store>,
    pub(crate) root: PathBuf,
    /// In-RAM mirror of every indexed file's L1 blob.
    ///
    /// Cross-file queries (`search_symbols`, `dependents`) otherwise re-read 1 blob per file
    /// per call — for a 39k-file repo that's seconds. With the preload they're pure-RAM scans.
    /// Wrapped in `ArcSwap` so the filesystem watcher can publish a new snapshot without
    /// blocking readers. Read-path tools do `.load_full()` once at the top to take a stable
    /// `Arc<MapCache>` for the duration of the call.
    pub(crate) cache: ArcSwap<MapCache>,
    /// Discovered git repository, or `None` when serving against a non-git directory.
    /// All git-aware tools (`working_tree_status`, `recent_changes`, …) check this and
    /// return an MCP error if `None`.
    pub(crate) repo: Option<Arc<crate::git::Repo>>,
    /// Sha-keyed cache for commit-files diffs, log walks, and blame results.
    pub(crate) git_cache: Arc<crate::git_cache::GitCache>,
    /// Precomputed git-history index (posting lists `path → [commit]`). `Some` only on a writable
    /// serve in a git repo with the index enabled; a read-only serve or a Fjall-lock collision
    /// leaves it `None`, and the git tools fall back to the live walk. Used by the history tools
    /// only when `last_indexed_head == HEAD` (the freshness gate), so it never serves stale results.
    pub(crate) git_history: Option<Arc<crate::git_history::GitHistoryIndex>>,
    /// `(blob_oid, lang) -> Arc<OutlineEntry>` cache that keeps `symbol_history` fast on
    /// hot files even when the symbol's source blob shows up in many adjacent commits.
    pub(crate) outline_cache: Arc<OutlineCache>,
    /// Scanner config (include / exclude globs, eager_l2, document tier knobs, …).
    /// Held on the server so the `rescan` MCP tool can re-run a scan in-process
    /// without re-reading `.basemind/basemind.toml`.
    pub(crate) config: Arc<crate::config::Config>,
    /// Per-tool-call telemetry writer; appends to `.basemind/telemetry.jsonl`.
    /// Always present (best-effort writes); the dashboard surfaces / statusline
    /// read from the same file.
    pub(crate) telemetry: Arc<telemetry::Telemetry>,
    /// Sum of `size_bytes` across every indexed file. Captured at boot and
    /// after each `rescan`. Feeds the corpus-baseline cost in
    /// [`super::savings::estimate_from_text`].
    pub(crate) corpus_bytes: std::sync::atomic::AtomicU64,
    /// Monotonic counter bumped every time `cache` is swapped (boot, rescan, view watcher).
    /// In-memory pagination cursors embed this value as a snapshot id so a resume call
    /// against a stale generation can be detected and reported back as
    /// `cursor_invalidated = true`.
    pub(crate) cache_generation: std::sync::atomic::AtomicU32,
    /// Per-repo scope key for LanceDB tables and `memory_by_key` Fjall keyspace.
    /// Computed once at boot. Do NOT recompute per-call.
    #[allow(dead_code)]
    pub(crate) scope: String,
    /// Owner segment for the individual-memory tier. Resolved once at boot by
    /// [`crate::comms::identity`] (validated through [`crate::comms::ids::AgentId`] so it is
    /// NUL-free), which never yields a shared constant — so two sessions cannot land on one
    /// memory owner. Group-tier writes ignore it.
    #[allow(dead_code)]
    pub(crate) agent_id: String,
    /// LanceDB vector store. Lazy-init on first memory/document/code-search call.
    #[cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]
    pub(crate) lance: tokio::sync::OnceCell<Arc<crate::lance::LanceStore>>,
    /// Shared embedding engine. Lazy-init on first embed call.
    #[cfg(feature = "intelligence")]
    pub(crate) embedder: tokio::sync::OnceCell<Arc<crate::embeddings::SharedEmbedder>>,
    /// Shared crawlberg engine. Initialised at server boot from the `[crawl]`
    /// config section; `None` if engine construction failed (the web_* tools
    /// will return an MCP error rather than crash).
    #[cfg(feature = "crawl")]
    pub(crate) crawl_engine: Option<crawlberg::CrawlEngineHandle>,
    /// Per-identity registry of lazily-connected comms-broker clients, keyed by `AgentId`. The
    /// server's own identity (`agent_id`) connects directly; a sub-identity (driven via a tool's
    /// `as_agent` param) gets its own broker connection, so one `serve` process can act as many
    /// named agents. Entries are created on first use; a connect failure surfaces as an MCP error
    /// on the triggering call, never at server boot.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) comms_clients: tokio::sync::Mutex<
        ahash::AHashMap<
            crate::comms::ids::AgentId,
            std::sync::Arc<tokio::sync::Mutex<crate::comms::client::CommsClient>>,
        >,
    >,
    /// Embedded rmux-backed headless shell runtime. Lazily connects to (or
    /// starts) the embedded daemon on the first `shell_*` tool call; cheap to
    /// hold otherwise (no daemon spawn until first use).
    #[cfg(all(feature = "shells", any(unix, windows)))]
    pub(crate) shell_runtime: crate::shells::ShellRuntime,
    /// Minimum logging severity the client asked for via `logging/setLevel`, as an ordinal
    /// (see [`super::notifications::level_ordinal`]). Defaults to `Info`. Checked before every log emit so
    /// the server honors the client's verbosity preference.
    pub(crate) log_level: std::sync::atomic::AtomicU8,
    /// True while the boot-time initial scan (auto-scan of an empty index) is running. Lets a
    /// client polling `status` distinguish "index still building" from "index empty / no matches"
    /// so the build cost is not silently folded into the first query's latency.
    pub(crate) initial_scan_active: std::sync::atomic::AtomicBool,
    /// Wall-clock duration of the boot-time initial scan, in milliseconds, once it completes
    /// (`0` = no initial scan happened this session, or it is still running). Surfaced on `status`
    /// as `index_build_ms` to report indexing time separately from query time.
    pub(crate) initial_scan_ms: std::sync::atomic::AtomicU64,
    /// True while the boot-time in-RAM code-map preload (`MapCache::build` over the existing blobs)
    /// is still running. Deferring that build off the startup path is what lets `serve` answer the
    /// MCP `initialize`/`tools/list` handshake immediately instead of blocking on a rayon `par_iter`
    /// that can be starved for minutes by other sessions' scans. Cache-reading tools await
    /// [`cache_ready`](Self::cache_ready) while this is set (see [`ServerState::await_cache_ready`]).
    pub(crate) cache_warming: std::sync::atomic::AtomicBool,
    /// Wall-clock duration of the boot-time cache preload, in milliseconds, once it completes
    /// (`0` = still warming or no deferred preload this session). Surfaced on `status` as `warm_ms`.
    pub(crate) cache_warm_ms: std::sync::atomic::AtomicU64,
    /// Fired once when the deferred preload finishes and the full map is swapped in. Tools that read
    /// the cache `notified().await` on this (bounded by [`CACHE_WARM_WAIT_CAP`]) so a query issued
    /// during the warmup window returns COMPLETE data rather than an empty snapshot.
    pub(crate) cache_ready: tokio::sync::Notify,
    /// True while a watcher-driven incremental rescan (`scan_and_refresh` from the active filesystem
    /// watcher) is in flight. Surfaced as the `Rescanning` lifecycle so a client sees "results may be
    /// a moment stale" rather than treating a mid-rescan snapshot as final.
    pub(crate) rescan_active: std::sync::atomic::AtomicBool,
    /// True when the in-RAM code map is built ON DEMAND — at the first
    /// [`await_cache_ready`](Self::await_cache_ready) barrier — instead of at construction.
    ///
    /// Set only for the one-shot CLI. A CLI process answers exactly one tool call and exits, and
    /// most tools (`repo_info`, `status`, every git tool) never read the map at all — yet
    /// [`MapCache::build`] deserializes EVERY indexed file's L1 blob, so those tools were paying
    /// seconds of whole-corpus startup for data they never touch. `serve` keeps eager/background
    /// warming: it is long-lived, so the build amortizes over the session.
    pub(crate) lazy_cache: bool,
    /// Gates the one-time on-demand build under [`lazy_cache`](Self::lazy_cache). Concurrent
    /// callers of the barrier all await the single build rather than racing to rebuild the map.
    pub(crate) lazy_cache_built: tokio::sync::OnceCell<()>,
    /// True when this serve fell back to a read-only store because another serve owns the
    /// write lock for this repo (issue #27). The single in-process writer (`scan_and_refresh`,
    /// behind the `rescan` tool) checks this and returns a clean error rather than writing
    /// without the lock.
    pub(crate) read_only: bool,
    /// True when this serve delegates every write to the machine daemon (the sole fjall writer)
    /// instead of writing locally. The store is opened read-only, but — unlike a plain
    /// [`read_only`](Self::read_only) fallback — the empty-index auto-scan, the filesystem
    /// watcher, and the `rescan` tool FORWARD their scans to the daemon over the socket and then
    /// rebuild the in-RAM map from the daemon-written `index.msgpack`. Only ever true on a
    /// `comms`-enabled build; always false otherwise, so the local-writer paths are unchanged.
    ///
    /// Only exists on a `comms` build: every read of this field lives behind the same `cfg`, so on
    /// a non-`comms` build there is no daemon to forward to and the field would be dead.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) daemon_writer: bool,
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
            self.initial_scan_active.load(Relaxed),
            self.cache_warming.load(Relaxed),
            self.rescan_active.load(Relaxed),
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
        if self.lazy_cache {
            self.build_cache_on_demand().await;
            return;
        }
        if !self.cache_warming.load(Relaxed) {
            return;
        }
        let notified = self.cache_ready.notified();
        if !self.cache_warming.load(Relaxed) {
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
        self.lazy_cache_built
            .get_or_init(|| async {
                let started = std::time::Instant::now();
                let store = self.store.read().await;
                let multi_thread = tokio::runtime::Handle::try_current()
                    .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
                    .unwrap_or(false);
                let cache = if multi_thread {
                    tokio::task::block_in_place(|| MapCache::build(&store))
                } else {
                    MapCache::build(&store)
                };
                let files = cache.by_path.len();
                self.cache.store(Arc::new(cache));
                self.cache_generation.fetch_add(1, Relaxed);
                let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                self.cache_warm_ms.store(elapsed_ms, Relaxed);
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
            // Never matches a populated index, so a serve still warming its map can never mistake
            // this placeholder for a current one and skip the build.
            fingerprint: 0,
            by_path: BTreeMap::new(),
            imports_index: Vec::new(),
            calls: None,
            impls: None,
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
        }
    }
}
