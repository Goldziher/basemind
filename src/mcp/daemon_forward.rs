//! Scans for a daemon-hosted workspace.
//!
//! The daemon's pool holds the workspace's writer store and the hosted store is a read-only one
//! borrowing its index handle per call, so a hosted scan runs through the pool ([`writer_rescan`]),
//! then serves the file map the pool wrote ([`Workspace::publish_store`]), so the caller sees fresh
//! results without waiting on the passive view watcher. The pool is reached as a concrete type from
//! the session, not through an engine seam (ADR-0042).

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::path::PathBuf;
use std::sync::Arc;

use crate::comms::workspace_pool::WorkspacePool;
use crate::engine::EngineError;
use crate::engine::Workspace;
use crate::engine::ops::index::RescanStats;

/// Run a hosted workspace's scan through the daemon's pool on the calling thread, then serve the
/// index it wrote.
///
/// `paths` (with `full == false`) drives an incremental rescan of just those files; `None`/empty or
/// `full` scans the whole working tree. `embed` asks for an
/// [`EmbedMode::Inline`](crate::scanner::EmbedMode::Inline) vector-fill pass so documents and code
/// chunks land in LanceDB; `false` is the fast code-map-only pass. `verify` says the paths were named
/// by a person or an agent (`index_rescan paths`), not seen changing by the watcher: the writer
/// validates the blob of a named file it would skip. The pool reports no per-file skip breakdown, so
/// those counts read 0.
pub(super) fn writer_rescan(
    ws: &Workspace,
    pool: &Arc<WorkspacePool>,
    paths: Option<Vec<PathBuf>>,
    full: bool,
    embed: bool,
    verify: bool,
) -> Result<RescanStats, EngineError> {
    let (stats, cancelled) = pool
        .hosted_rescan(&ws.root, paths, full, embed, verify)
        .map_err(|error| EngineError::internal(format!("host rescan: {error}")))?;
    // A cancelled pass may have flushed a file map whose resolve pass never ran. The session keeps
    // its map until a completed pass, and the caller is told, as on the socket path. ~keep
    if cancelled {
        return Err(EngineError::internal(
            "host rescan cancelled: the daemon is draining; retry against the next daemon",
        ));
    }
    let store = pool
        .read_store(&ws.root)
        .map_err(|error| EngineError::internal(format!("reopen read-only store: {error}")))?;
    if let Some(cache) = ws.publish_store(store) {
        super::background::warm_terms(&cache);
    }
    Ok(RescanStats {
        scanned: stats.scanned,
        updated: stats.updated,
        docs_indexed: stats.docs_indexed,
        docs_degraded: stats.docs_degraded,
        docs_text_fallback: stats.docs_text_fallback,
        doc_timeouts: stats.doc_timeouts,
        removed: stats.removed,
        ..RescanStats::default()
    })
}
