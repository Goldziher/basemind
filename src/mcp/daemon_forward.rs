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
    // Dedicated, un-cached connection: a rescan/embed can run for minutes, so it must NOT hold the
    // shared per-identity comms client mutex — doing so head-of-line-blocks every other comms tool
    // and `resolved_refs` read for this identity until the scan finishes (#36).
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
        // Blobs-only: never open the fjall index (the daemon holds its exclusive lock as the sole
        // writer). The rebuilt store + map read from the shared blobs the daemon just wrote.
        let store = Store::open_read_only_no_index(&root, &view)?;
        // A scan that changed nothing (`updated: 0, removed: 0` — the common case under editor or
        // gitignored churn) still rewrites `index.msgpack`. Rebuilding the whole map for it means
        // re-reading every L1/L2 blob while the OLD map is still resident in `state.cache`, which
        // transiently doubles serve's RSS. An unchanged fingerprint proves the content-addressed
        // blobs behind the map are identical, so the map already in hand is still exactly correct.
        let cache =
            (super::map_fingerprint::index_fingerprint(&store) != current_fingerprint).then(|| MapCache::build(&store));
        Ok::<(Store, Option<MapCache>), crate::store::StoreError>((store, cache))
    })
    .await
    .map_err(|error| McpError::internal_error(format!("refresh map task panicked: {error}"), None))?
    .map_err(|error| McpError::internal_error(format!("reopen read-only store: {error}"), None))?;
    // The store is swapped either way: the daemon rewrote `index.msgpack`, and store-reading tools
    // (`status`'s `file_count`, corpus bytes) read `store.index` directly rather than the map.
    *state.store.write().await = store;
    if let Some(cache) = cache {
        state.cache.store(Arc::new(cache));
    }
    // Bump the generation on every forwarded rescan — even when the fingerprint was unchanged and the
    // map above was reused. `cache_generation` is the cursor-validity token, and the documented
    // contract (matched by the local scan path in `background.rs`) is that a rescan invalidates
    // in-flight cursors. The bump is a lone atomic add, independent of the expensive `MapCache::build`
    // the fingerprint check still guards, so cursor invalidation stays consistent across the
    // daemon-writer and local paths without giving up the no-op-rescan RSS optimization.
    state.cache_generation.fetch_add(1, Ordering::Relaxed);
    Ok(())
}
