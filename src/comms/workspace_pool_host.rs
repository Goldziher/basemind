//! The in-process host seam of the [`WorkspacePool`]. Split out of `workspace_pool.rs` to keep
//! that file under the module size cap.

use std::path::{Path, PathBuf};

use super::WorkspacePool;
use crate::scanner::ScanStats;

/// The in-process host seam: a daemon-hosted read stack routes its writes / rescans / precise
/// resolved-reference reads straight through the pool (the machine's sole fjall writer) instead of
/// forwarding them over the daemon's own socket — the daemon would otherwise dial itself. Mirrors
/// the forwarded-op handlers in [`daemon_forward_handlers`](crate::comms::daemon_forward_handlers) exactly;
/// the pool's per-workspace store lock supplies the same serialization the socket path relied on.
///
/// Methods are synchronous (the pool's API is blocking); call sites run them under `spawn_blocking`.
impl crate::mcp::HostBackend for WorkspacePool {
    fn host_rescan(
        &self,
        root: &Path,
        paths: Option<Vec<PathBuf>>,
        full: bool,
        embed: bool,
    ) -> Result<ScanStats, String> {
        // Use the broker's shared drain token (not a throwaway `default()`), so a hosted mid-scan — ~keep
        // e.g. a full-corpus Inline embed pass — honors `comms stop` / SIGTERM within one file ~keep
        // instead of pinning the runtime until the 15s teardown backstop. The socket path already ~keep
        // threads this token; the host seam must too. ~keep
        self.rescan(root, paths, full, embed, &self.scan_cancel)
            .map(|(stats, _cancelled)| stats)
            .map_err(|error| error.to_string())
    }

    fn host_rescan_named(&self, root: &Path, paths: Vec<PathBuf>, embed: bool) -> Result<ScanStats, String> {
        self.rescan_as(root, Some(paths), false, embed, true, &self.scan_cancel)
            .map(|(stats, _cancelled)| stats)
            .map_err(|error| error.to_string())
    }

    #[cfg(feature = "code-search")]
    fn host_code_search_lanes(
        &self,
        root: &Path,
        query: crate::comms::code_search_proto::CodeSearchLaneQuery,
    ) -> Result<crate::comms::code_search_proto::CodeSearchLaneResult, String> {
        self.with_workspace(root, |store| {
            crate::comms::daemon_forward_handlers::code_search_lanes_against(store, &query)
        })
        .map_err(|error| error.to_string())?
    }

    fn host_index_read(
        &self,
        root: &Path,
        query: crate::comms::index_read_proto::IndexReadQuery,
    ) -> Result<crate::comms::index_read_proto::IndexReadResult, String> {
        crate::comms::daemon_forward_handlers::index_read_via_pool(self, root, &query)
    }

    fn host_resolved_refs(
        &self,
        root: &Path,
        query: crate::comms::resolved_proto::ResolvedRefQuery,
    ) -> Result<crate::comms::resolved_proto::ResolvedRefResult, String> {
        self.with_workspace(root, |store| {
            crate::comms::daemon_forward_handlers::resolve_refs_against(store, &query)
        })
        .map_err(|error| error.to_string())
    }

    #[cfg(feature = "memory")]
    fn host_memory(
        &self,
        root: &Path,
        scope: &str,
        op: crate::comms::memory_proto::MemoryOp,
    ) -> Result<crate::comms::memory_proto::MemoryOutcome, String> {
        self.with_workspace_mut(root, |store| {
            let idx = store
                .index_db
                .as_ref()
                .ok_or(crate::mcp::memory_ops::MemoryOpError::IndexUnavailable)?;
            crate::mcp::memory_ops::run_memory_op(idx, scope, &op)
        })
        .map_err(|error| error.to_string())
        .and_then(|result| result.map_err(|error| error.to_string()))
    }
}
