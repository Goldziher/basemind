//! MCP server per-connection state: [`ServerState`] (identity + a shared read stack), its
//! [`Lifecycle`](crate::engine::Lifecycle) classifier, and the [`MapCache`] read stack over the indexed corpus.
//!
//! The heavy, workspace-level read fields live on [`Workspace`](super::Workspace)
//! (in `src/engine/workspace.rs`) so one stack can be shared — via an [`Arc`] — across many connections.
//! `ServerState` keeps only what is specific to a single connection: the resolved agent identity,
//! its lazily-connected link to the daemon, and the client's requested log verbosity.

use std::sync::Arc;

use super::{Workspace, types};
use crate::store::Store;

/// An owned read guard on a workspace's store, from [`ServerState::store_read`].
pub(crate) type StoreReadGuard = parking_lot::ArcRwLockReadGuard<parking_lot::RawRwLock, Store>;
/// An owned write guard on a workspace's store, from [`ServerState::store_write`].
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) type StoreWriteGuard = parking_lot::ArcRwLockWriteGuard<parking_lot::RawRwLock, Store>;

/// A [`Workspace`] together with what every connection served from it shares: the telemetry writer
/// and how the workspace's writes reach the index. The engine knows none of these; they belong to
/// the process serving it. Cloned into each connection's [`ServerState`].
#[derive(Clone)]
pub(crate) struct ServedWorkspace {
    /// The workspace-level read stack.
    pub(crate) workspace: Arc<Workspace>,
    /// Per-tool-call telemetry writer; appends to the workspace's `telemetry.jsonl`. One writer per
    /// served workspace, so connections sharing it share its rotation.
    pub(crate) telemetry: Arc<super::telemetry::Telemetry>,
    /// The daemon's own workspace pool, for a daemon-hosted workspace (see [`ServerState::pool`]).
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) pool: Option<Arc<crate::comms::workspace_pool::WorkspacePool>>,
}

impl ServedWorkspace {
    /// Serve `workspace` from this process, with its own telemetry writer under `basemind_dir`.
    pub(crate) fn new(workspace: Arc<Workspace>, basemind_dir: &std::path::Path) -> Self {
        Self {
            workspace,
            telemetry: Arc::new(super::telemetry::Telemetry::new(basemind_dir)),
            #[cfg(all(feature = "comms", any(unix, windows)))]
            pool: None,
        }
    }
}

/// Per-connection MCP server state.
///
/// Holds one [`Arc<Workspace>`](super::Workspace) (the heavy read state, shared across
/// connections) plus this connection's own identity. Field accesses to the shared read stack go
/// through [`shared`](Self::shared); the identity fields (`agent_id`, `agents`)
/// are read directly.
pub(crate) struct ServerState {
    /// The workspace-level read stack shared across every connection to this `(root, view)`.
    pub(crate) shared: Arc<Workspace>,
    /// Per-tool-call telemetry writer, shared by every connection to this workspace. Always present
    /// (best-effort writes); the dashboard surfaces / statusline read from the same file.
    pub(crate) telemetry: Arc<super::telemetry::Telemetry>,
    /// The daemon's workspace pool, for a DAEMON-HOSTED workspace: the pool holds the writer store,
    /// so this session's scans run through it. Reads need no seam: the hosted store shares the
    /// writer's index handle. `None` on every non-hosted session (in-process serve / CLI), which
    /// writes its own store.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) pool: Option<Arc<crate::comms::workspace_pool::WorkspacePool>>,
    /// Owner segment for the individual-memory tier: the session's principal, resolved by the one
    /// identity resolver of agent comms (ADR-0021). Group-tier writes ignore it.
    #[allow(dead_code)]
    pub(crate) agent_id: String,
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
    /// Build a fresh per-connection state that shares the given served [`Workspace`].
    ///
    /// Each connection gets its own identity: no daemon link yet, no agent context, and the default
    /// log verbosity. The daemon-hosted transports use it to hand every accepted connection the one
    /// shared read stack.
    pub(crate) fn for_connection(served: ServedWorkspace, agent_id: String) -> Self {
        Self {
            shared: served.workspace,
            telemetry: served.telemetry,
            #[cfg(all(feature = "comms", any(unix, windows)))]
            pool: served.pool,
            agent_id,
            #[cfg(all(feature = "comms", any(unix, windows)))]
            agents: None,
            lean: std::sync::atomic::AtomicBool::new(super::lean::lean_mode_enabled()),
            task_threshold_ms: std::sync::atomic::AtomicU64::new(super::tasks::TASK_THRESHOLD.as_millis() as u64),
        }
    }
}

impl ServerState {
    /// Barrier every cache-reading tool crosses before it touches [`ServerState::cache`].
    ///
    /// Two regimes:
    ///
    /// * **One-shot ([`lazy_cache`](Self::lazy_cache))** — build the map HERE, on demand, once. The
    ///   cost then falls only on tools that actually read the map; `repo_info` / `status` / the git
    ///   tools never reach this barrier and so never pay it.
    /// * **`serve`** — wait for the background preload to publish the full map, bounded by
    ///   [`CACHE_WARM_WAIT_CAP`](crate::engine::workspace::CACHE_WARM_WAIT_CAP). No-op once warm (the common path).
    ///
    /// Neither regime waits on
    /// [`Lifecycle::BuildingIndex`](crate::engine::Lifecycle::BuildingIndex) — a from-scratch scan
    /// can run for minutes, so those tools return the partial index plus a
    /// [`lifecycle_notice`](Self::lifecycle_notice) telling the client to poll.
    pub(crate) async fn await_cache_ready(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        if self.shared.lazy_cache {
            if self.shared.lazy_cache_built.get().is_none() {
                super::index_route::blocking(|| self.build_cache_on_demand());
            }
            return;
        }
        if !self.shared.cache_warming.load(Relaxed) {
            return;
        }
        super::index_route::blocking(|| self.shared.wait_for_warm());
    }

    /// Build the whole-corpus in-RAM map and publish it — exactly once, however many callers race
    /// the barrier.
    ///
    /// [`MapCache::build`] is IO-bound (a rayon `par_iter` probing every indexed file's blob), so it
    /// must not run on the async reactor: [`await_cache_ready`](Self::await_cache_ready) runs it
    /// inside [`index_route::blocking`](super::index_route::blocking), which hands the worker over
    /// on the multi-thread runtime the CLI builds and runs it in place on a `current_thread` test
    /// runtime, which has no other task to starve.
    fn build_cache_on_demand(&self) {
        if let Some(cache) = self.shared.build_map_on_demand() {
            super::background::warm_terms(&cache);
        }
    }

    /// Read the workspace's store from async code. A free lock is taken at once; a held one (a scan
    /// is writing) is waited for on a blocking thread, so the wait never parks a runtime worker. The
    /// guard is owned and `Send`, so it may be held across an `.await`, as the async lock this
    /// replaces allowed.
    pub(crate) async fn store_read(&self) -> StoreReadGuard {
        let lock = self.shared.store_lock();
        if let Some(store) = lock.try_read_arc() {
            return store;
        }
        let lock = Arc::clone(lock);
        match tokio::task::spawn_blocking(move || lock.read_arc()).await {
            Ok(store) => store,
            Err(join) => std::panic::resume_unwind(join.into_panic()),
        }
    }

    /// Write the workspace's store from async code, waiting as [`store_read`](Self::store_read)
    /// does. Only the daemon-forward refresh writes from async code.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) async fn store_write(&self) -> StoreWriteGuard {
        let lock = self.shared.store_lock();
        if let Some(store) = lock.try_write_arc() {
            return store;
        }
        let lock = Arc::clone(lock);
        match tokio::task::spawn_blocking(move || lock.write_arc()).await {
            Ok(store) => store,
            Err(join) => std::panic::resume_unwind(join.into_panic()),
        }
    }

    /// The notice a read-tool response carries ([`Workspace::lifecycle_notice`]).
    pub(crate) fn lifecycle_notice(&self) -> Option<types::LifecycleNotice> {
        self.shared.lifecycle_notice()
    }
}
