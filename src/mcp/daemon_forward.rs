//! Serve-side write forwarding for a `daemon_writer` session.
//!
//! On a `comms` build the real `serve` binary opens its store read-only and delegates every scan to
//! the machine daemon (the sole fjall writer). This module is that seam: [`forward_rescan_and_refresh`]
//! sends the scan over the socket, then rebuilds the read-only in-RAM [`MapCache`] from the
//! daemon-written `index.msgpack` so the caller sees fresh results without waiting on the passive
//! view watcher.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;

use super::helpers_comms::{comms_err, connect_ephemeral_client};
use super::{MapCache, ServerState};
use crate::comms::client::RescanReport;
use crate::store::Store;

/// Forward a scan to the daemon (the sole fjall writer) and rebuild the read-only map from the
/// index it writes.
///
/// `paths` (with `full == false`) drives an incremental rescan of just those files; `None`/empty or
/// `full` scans the whole working tree. `embed` asks the daemon (the sole writer) to run an
/// [`EmbedMode::Inline`](crate::scanner::EmbedMode::Inline) vector-fill pass so documents + code
/// chunks land in LanceDB; `false` is the fast code-map-only pass. Returns the daemon's scan counts.
/// Errors — no daemon reachable, a scan failure, or a store reopen failure — surface as an
/// [`McpError`] the caller maps to its own response.
pub(super) async fn forward_rescan_and_refresh(
    state: &Arc<ServerState>,
    paths: Option<Vec<PathBuf>>,
    full: bool,
    embed: bool,
) -> Result<RescanReport, McpError> {
    let mut client = connect_ephemeral_client(state).await?;
    let report = client
        .rescan(state.root.clone(), paths, full, embed)
        .await
        .map_err(comms_err)?;
    refresh_readonly_map(state).await?;
    Ok(report)
}

/// Refresh serve's read-only view from the current (daemon-written) `index.msgpack`: reopen the
/// store and rebuild the in-RAM [`MapCache`]. Runs the reopen + `MapCache::build` (a rayon
/// `par_iter`) on a blocking thread so the reactor is never stalled.
///
/// Swaps BOTH the store and the cache. The daemon just rewrote `index.msgpack`, so serve's in-memory
/// [`crate::store::Index`] is stale. Cache-reading tools (`search_symbols`, `outline`) pick up the
/// new cache, but store-reading tools (`status`'s `file_count`, corpus bytes) read `store.index`
/// directly — without replacing the store they would report the pre-scan (often empty) index
/// forever. This is the forward-path counterpart to a local scan mutating the store in place.
async fn refresh_readonly_map(state: &Arc<ServerState>) -> Result<(), McpError> {
    let view = state.store.read().await.view.clone();
    let root = state.root.clone();
    let current_fingerprint = state.cache.load().fingerprint;
    let (store, cache) = tokio::task::spawn_blocking(move || {
        let store = Store::open_read_only_no_index(&root, &view)?;
        let cache =
            (super::map_fingerprint::index_fingerprint(&store) != current_fingerprint).then(|| MapCache::build(&store));
        Ok::<(Store, Option<MapCache>), crate::store::StoreError>((store, cache))
    })
    .await
    .map_err(|error| McpError::internal_error(format!("refresh map task panicked: {error}"), None))?
    .map_err(|error| McpError::internal_error(format!("reopen read-only store: {error}"), None))?;
    *state.store.write().await = store;
    if let Some(cache) = cache {
        state.cache.store(Arc::new(cache));
    }
    state.cache_generation.fetch_add(1, Ordering::Relaxed);
    Ok(())
}
