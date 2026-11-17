//! Scans for a daemon-hosted workspace.
//!
//! The daemon's pool is the workspace's sole fjall writer and the hosted store is opened read-only,
//! so a hosted scan runs through the pool ([`writer_rescan_and_refresh`]), then rebuilds the
//! read-only in-RAM [`MapCache`] from the `index.msgpack` the pool wrote, so the caller sees fresh
//! results without waiting on the passive view watcher.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ErrorData as McpError;

use super::{MapCache, ServerState};
use crate::comms::client::RescanReport;
use crate::store::Store;

/// Run a hosted workspace's scan through the daemon's pool (`host`), in process on a blocking
/// thread, then refresh the in-RAM map from the index it wrote.
///
/// `paths` (with `full == false`) drives an incremental rescan of just those files; `None`/empty or
/// `full` scans the whole working tree. `embed` asks for an
/// [`EmbedMode::Inline`](crate::scanner::EmbedMode::Inline) vector-fill pass so documents and code
/// chunks land in LanceDB; `false` is the fast code-map-only pass. `verify` says the paths were named
/// by a person or an agent (`index_rescan paths`), not seen changing by the watcher: the writer
/// validates the blob of a named file it would skip.
pub(super) async fn writer_rescan_and_refresh(
    state: &Arc<ServerState>,
    host: &Arc<dyn super::HostBackend>,
    paths: Option<Vec<PathBuf>>,
    full: bool,
    embed: bool,
    verify: bool,
) -> Result<RescanReport, McpError> {
    let host = Arc::clone(host);
    let root = state.shared.root.clone();
    let stats = tokio::task::spawn_blocking(move || match paths {
        Some(paths) if verify && !full && !paths.is_empty() => host.host_rescan_named(&root, paths, embed),
        paths => host.host_rescan(&root, paths, full, embed),
    })
    .await
    .map_err(|error| McpError::internal_error(format!("host rescan task panicked: {error}"), None))?
    .map_err(|error| McpError::internal_error(format!("host rescan: {error}"), None))?;
    refresh_cache_after_scan(state).await?;
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
/// store and derive the in-RAM [`MapCache`] from the current one ([`MapCache::refreshed`]: only
/// the paths the scan changed are re-projected, and unchanged outlines stay decoded). Runs on a
/// blocking thread so the reactor is never stalled.
///
/// Swaps BOTH the store and the cache. The daemon just rewrote `index.msgpack`, so serve's in-memory
/// [`crate::store::Index`] is stale. Cache-reading tools (`search_symbols`, `outline`) pick up the
/// new cache, but store-reading tools (`status`'s `file_count`, corpus bytes) read `store.index`
/// directly — without replacing the store they would report the pre-scan (often empty) index
/// forever. This is the hosted counterpart to a local scan mutating the store in place.
async fn refresh_cache_after_scan(state: &Arc<ServerState>) -> Result<(), McpError> {
    let view = state.store_read().await.view.clone();
    let root = state.shared.root.clone();
    let current = state.shared.cache.load_full();
    let config = Arc::clone(&state.shared.config);
    let (store, cache) = tokio::task::spawn_blocking(move || {
        let store = Store::open_read_only_no_index(&root, &view)?;
        let cache = current.refreshed(&store, super::l1_cache::budget_bytes_from(&config.resources));
        Ok::<(Store, Option<MapCache>), crate::store::StoreError>((store, cache))
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
