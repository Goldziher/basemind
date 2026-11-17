//! MCP server per-connection state: [`ServerState`] (identity + a shared read stack), its
//! [`Lifecycle`] classifier, and the [`MapCache`] read stack over the indexed corpus.
//!
//! The heavy, workspace-level read fields live on [`SharedReadStack`](super::SharedReadStack)
//! (in `shared_state.rs`) so one stack can be shared — via an [`Arc`] — across many connections.
//! `ServerState` keeps only what is specific to a single connection: the resolved agent identity,
//! its lazily-connected link to the daemon, and the client's requested log verbosity.

use std::sync::Arc;

use super::{SharedReadStack, l1_cache, types};
use crate::engine::{Lifecycle, MapCache, workspace::CACHE_WARM_WAIT_CAP};

/// Per-connection MCP server state.
///
/// Holds one [`Arc<SharedReadStack>`](super::SharedReadStack) (the heavy read state, shared across
/// connections) plus this connection's own identity. Field accesses to the shared read stack go
/// through [`shared`](Self::shared); the identity fields (`agent_id`, `daemon_client`)
/// are read directly.
pub(crate) struct ServerState {
    /// The workspace-level read stack shared across every connection to this `(root, view)`.
    pub(crate) shared: Arc<SharedReadStack>,
    /// Owner segment for the individual-memory tier: the session's principal, resolved by the one
    /// identity resolver of agent comms (ADR-0021). Group-tier writes ignore it.
    #[allow(dead_code)]
    pub(crate) agent_id: String,
    /// This session's link to the daemon for forwarded work, connected on first use (see
    /// `helpers_daemon`). It names no agent.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) daemon_client:
        tokio::sync::OnceCell<std::sync::Arc<tokio::sync::Mutex<crate::comms::client::CommsClient>>>,
    /// Who this session is to the agent tools: set for a daemon-hosted relay session only.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) agents: Option<std::sync::Arc<super::AgentContext>>,
    /// Whether this server advertises the lean three-tool surface (see [`super::lean`]). Resolved
    /// ONCE at construction from `BASEMIND_MCP_LEAN` (the value does not change mid-session), so the
    /// decision is stable and per-server rather than a per-request global-env read — which lets
    /// several servers with different surfaces coexist in one process (integration tests). The
    /// in-memory test serve overrides it directly.
    pub(crate) lean: std::sync::atomic::AtomicBool,
    /// Milliseconds an offload-eligible call may run before it becomes a task
    /// ([`super::tasks::TASK_THRESHOLD`]). Per-server so the in-memory test serve can lower it to
    /// exercise the task path without a genuinely slow call.
    pub(crate) task_threshold_ms: std::sync::atomic::AtomicU64,
}

impl ServerState {
    /// Build a fresh per-connection state that shares the given [`SharedReadStack`].
    ///
    /// Each connection gets its own identity: no daemon link yet, no agent context, and the default
    /// log verbosity. The daemon-hosted transports use it to hand every accepted connection the one
    /// shared read stack.
    #[allow(dead_code)]
    pub(crate) fn for_connection(shared: Arc<SharedReadStack>, agent_id: String) -> Self {
        Self {
            shared,
            agent_id,
            #[cfg(all(feature = "comms", any(unix, windows)))]
            daemon_client: tokio::sync::OnceCell::new(),
            #[cfg(all(feature = "comms", any(unix, windows)))]
            agents: None,
            lean: std::sync::atomic::AtomicBool::new(super::lean::lean_mode_enabled()),
            task_threshold_ms: std::sync::atomic::AtomicU64::new(super::tasks::TASK_THRESHOLD.as_millis() as u64),
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
                let cache = if multi_thread {
                    tokio::task::block_in_place(|| MapCache::build(&store, budget))
                } else {
                    MapCache::build(&store, budget)
                };
                let files = cache.len();
                let cache = Arc::new(cache);
                self.shared.cache.store(Arc::clone(&cache));
                cache.warm_terms();
                self.shared.cache_generation.fetch_add(1, Relaxed);
                let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                self.shared.cache_warm_ms.store(elapsed_ms, Relaxed);
                tracing::debug!(files, elapsed_ms, "in-RAM code map built on demand");
            })
            .await;
    }

    /// The `index_rebuilding` notice while the workspace's `status.json` records a partial scan or an
    /// uncommitted resolve pass. The daemon marks it when it opens a workspace whose fjall index was
    /// wiped or is empty over a populated file map, and a session that does not hold the index
    /// (`daemon_writer`, daemon-hosted) cannot see that any other way.
    fn index_rebuilding_notice(&self) -> Option<types::LifecycleNotice> {
        let status = crate::store::read_status_sidecar(&crate::store::workspace_cache_dir(&self.shared.root));
        types::LifecycleNotice::for_scan_status(status.as_ref())
    }

    /// A [`LifecycleNotice`](types::LifecycleNotice) to attach to a tool response, or `None` when
    /// [`Ready`](Lifecycle::Ready). Lets every read tool label a possibly-incomplete result with the
    /// server state + an actionable message, so an agent knows to retry rather than concluding "empty".
    /// The notice for a workspace nothing has been scanned into, so an empty answer is never read
    /// as "no matches". Judged on the store's file map, not the in-RAM map, which is also empty
    /// before a lazy build. `None` while another task holds the store lock.
    fn empty_index_notice(&self) -> Option<types::LifecycleNotice> {
        let store = self.shared.store.try_read().ok()?;
        (store.index.files.is_empty() && store.index.doc_files.is_empty()).then(types::LifecycleNotice::empty_index)
    }

    pub(crate) fn lifecycle_notice(&self) -> Option<types::LifecycleNotice> {
        types::LifecycleNotice::for_state(self.lifecycle())
            .or_else(|| self.index_rebuilding_notice())
            .or_else(|| self.empty_index_notice())
            .or_else(|| {
                // A read-only session whose projected call/impl indexes were capped answers reference
                // queries from a truncated set. Reported through the same channel as the warming/
                // rescanning states so a caller never reads an incomplete result as "no matches".
                self.shared
                    .cache
                    .load()
                    .projections_capped()
                    .then(types::LifecycleNotice::projections_capped)
            })
    }
}
