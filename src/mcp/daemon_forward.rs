//! Scans for a daemon-hosted workspace.
//!
//! The daemon's pool holds the workspace's writer store and the hosted store is a read-only one
//! borrowing its index handle per call, so a hosted scan runs through the pool ([`writer_rescan_and_refresh`]),
//! then reloads the file map the pool wrote and refreshes the in-RAM [`MapCache`](super::MapCache) from it, so the
//! caller sees fresh results without waiting on the passive view watcher.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ErrorData as McpError;

use super::ServerState;
use crate::comms::client::RescanReport;
use crate::comms::workspace_pool::WorkspacePool;

/// Run a hosted workspace's scan through the daemon's pool, in process on a blocking thread, then
/// refresh the in-RAM map from the index it wrote.
///
/// `paths` (with `full == false`) drives an incremental rescan of just those files; `None`/empty or
/// `full` scans the whole working tree. `embed` asks for an
/// [`EmbedMode::Inline`](crate::scanner::EmbedMode::Inline) vector-fill pass so documents and code
/// chunks land in LanceDB; `false` is the fast code-map-only pass. `verify` says the paths were named
/// by a person or an agent (`index_rescan paths`), not seen changing by the watcher: the writer
/// validates the blob of a named file it would skip.
pub(super) async fn writer_rescan_and_refresh(
    state: &Arc<ServerState>,
    pool: &Arc<WorkspacePool>,
    paths: Option<Vec<PathBuf>>,
    full: bool,
    embed: bool,
    verify: bool,
) -> Result<RescanReport, McpError> {
    let scan_pool = Arc::clone(pool);
    let root = state.shared.root.clone();
    let (stats, cancelled) =
        tokio::task::spawn_blocking(move || scan_pool.hosted_rescan(&root, paths, full, embed, verify))
            .await
            .map_err(|error| McpError::internal_error(format!("host rescan task panicked: {error}"), None))?
            .map_err(|error| McpError::internal_error(format!("host rescan: {error}"), None))?;
    // A cancelled pass may have flushed a file map whose resolve pass never ran. The session keeps
    // its map until a completed pass, and the caller is told, as on the socket path. ~keep
    if cancelled {
        return Err(McpError::internal_error(
            "host rescan cancelled: the daemon is draining; retry against the next daemon",
            None,
        ));
    }
    refresh_cache_after_scan(state, pool).await?;
    Ok(RescanReport {
        scanned: stats.scanned,
        updated: stats.updated,
        docs_indexed: stats.docs_indexed,
        docs_degraded: stats.docs_degraded,
        docs_text_fallback: stats.docs_text_fallback,
        doc_timeouts: stats.doc_timeouts,
        removed: stats.removed,
        elapsed_ms: 0,
    })
}

/// Refresh the hosted read-only view from the current (pool-written) `index.msgpack`: reopen the
/// read store (borrowing the writer's index handle per call) and derive the in-RAM
/// [`MapCache`](super::MapCache) from the current one
/// ([`MapCache::refreshed`](super::MapCache::refreshed): only the paths the scan changed are
/// re-projected, and unchanged outlines stay decoded). Runs on a blocking thread so the reactor is
/// never stalled.
///
/// Swaps BOTH the store and the cache. The daemon just rewrote `index.msgpack`, so serve's in-memory
/// [`crate::store::Index`] is stale. Cache-reading tools (`search_symbols`, `outline`) pick up the
/// new cache, but store-reading tools (`status`'s `file_count`, corpus bytes) read `store.index`
/// directly — without replacing the store they would report the pre-scan (often empty) index
/// forever. This is the hosted counterpart to a local scan mutating the store in place.
async fn refresh_cache_after_scan(state: &Arc<ServerState>, pool: &Arc<WorkspacePool>) -> Result<(), McpError> {
    let root = state.shared.root.clone();
    let current = state.shared.cache.load_full();
    let config = Arc::clone(&state.shared.config);
    let pool = Arc::clone(pool);
    let (store, cache) = tokio::task::spawn_blocking(move || {
        let store = pool.read_store(&root)?;
        let cache = current.refreshed(&store, super::l1_cache::budget_bytes_from(&config.resources));
        Ok::<_, crate::comms::workspace_pool::WorkspacePoolError>((store, cache))
    })
    .await
    .map_err(|error| McpError::internal_error(format!("refresh map task panicked: {error}"), None))?
    .map_err(|error| McpError::internal_error(format!("reopen read-only store: {error}"), None))?;
    *state.store_write().await = store;
    if let Some(cache) = cache {
        let cache = Arc::new(cache);
        state.shared.cache.store(Arc::clone(&cache));
        super::background::warm_terms(&cache);
    }
    state.shared.note_index_changed();
    Ok(())
}
