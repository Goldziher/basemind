//! The broker's streamable-HTTP per-request seam, split out of `daemon.rs` to keep it under the
//! `rust-max-lines` cap. This is a second `impl Broker` block plus the RAII activity guard: it
//! covers what one HTTP request needs from the broker — pin the daemon while the request runs, stamp
//! HTTP recency, resolve the request's workspace read stack, and register the request against that
//! workspace's eviction sweep. The idle/reaper logic that *reads* `http_inflight` / `last_http_ms`
//! stays in `daemon.rs`; only the per-request write path lives here.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::daemon::Broker;

/// RAII marker that one streamable-HTTP request is being served; see [`Broker::begin_http_request`].
///
/// The HTTP front-end is stateless — no persistent connection survives between requests — so the
/// link refcount that pins a UDS session cannot pin an HTTP one. This guard fills that gap: while it
/// lives the request counts as in-flight (the idle reaper skips the daemon), and both its
/// construction and its drop stamp `last_http_ms` so a burst of short requests keeps the daemon
/// warm across the gaps between them, exactly as `last_activity_ms` does for UDS links.
pub struct HttpActivityGuard {
    broker: Arc<Broker>,
}

impl Drop for HttpActivityGuard {
    fn drop(&mut self) {
        self.broker.http_inflight.fetch_sub(1, Ordering::SeqCst);
        self.broker.stamp_http_activity();
    }
}

impl Broker {
    /// Stamp "now" as the last streamable-HTTP request time. Called at the start and end of every
    /// HTTP request via [`HttpActivityGuard`].
    pub fn stamp_http_activity(&self) {
        self.last_http_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::SeqCst);
    }

    /// Mark one streamable-HTTP request as in flight for as long as the returned guard lives, and
    /// stamp HTTP activity now. See [`HttpActivityGuard`] and [`Broker::is_idle_for`].
    pub fn begin_http_request(self: &Arc<Self>) -> HttpActivityGuard {
        self.http_inflight.fetch_add(1, Ordering::SeqCst);
        self.stamp_http_activity();
        HttpActivityGuard { broker: self.clone() }
    }

    /// Resolve a repo `root` to its daemon-hosted [`SharedReadStack`](crate::mcp::SharedReadStack),
    /// building it on first touch. This is the HTTP front-end's per-request seam: it mirrors exactly
    /// what [`serve_relay_connection`](Broker::serve_relay_connection) does for a UDS relay client —
    /// same host backend (the workspace pool) and git-history host (this broker) — so an HTTP request
    /// and a relay connection to the same workspace share one resident read stack.
    pub(crate) async fn host_read_stack(
        self: &Arc<Self>,
        root: &std::path::Path,
    ) -> anyhow::Result<Arc<crate::mcp::SharedReadStack>> {
        let host = Arc::clone(&self.workspaces) as Arc<dyn crate::mcp::HostBackend>;
        let git_history_host = Arc::clone(self) as Arc<dyn crate::git_history::remote::HistoryHost>;
        self.workspaces
            .get_or_build_serve_state(root, host, git_history_host)
            .await
    }

    /// Register one HTTP request against a hosted workspace, returning a guard that keeps the
    /// eviction sweep from dropping the workspace's shared read stack while the request runs.
    pub(crate) fn begin_workspace_conn(
        &self,
        root: &std::path::Path,
    ) -> Result<super::workspace_pool::ServeConnGuard, String> {
        self.workspaces.begin_conn(root).map_err(|error| error.to_string())
    }
}
