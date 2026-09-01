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
        .rescan(state.shared.root.clone(), paths, full, embed)
        .await
        .map_err(comms_err)?;
    refresh_cache_after_scan(state).await?;
    Ok(report)
}

/// Route a read-only serve's scan to the machine's sole fjall writer, then refresh the in-RAM map.
///
/// On the DAEMON-HOSTED path ([`state.shared.host`](ServerState) is `Some`) the writer pool is
/// in-process, so the scan runs directly through it on a blocking thread — no socket loopback, which
/// on the daemon would mean the daemon dialing itself. Every other `daemon_writer` serve has no host
/// and [`forward_rescan_and_refresh`] ships the scan over the socket. Both then rebuild the read-only
/// map from the index the writer just wrote via the shared [`refresh_cache_after_scan`] tail — so the
/// refresh half is written once.
pub(super) async fn writer_rescan_and_refresh(
    state: &Arc<ServerState>,
    paths: Option<Vec<PathBuf>>,
    full: bool,
    embed: bool,
) -> Result<RescanReport, McpError> {
    if let Some(host) = &state.shared.host {
        let host = Arc::clone(host);
        let root = state.shared.root.clone();
        let stats = tokio::task::spawn_blocking(move || host.host_rescan(&root, paths, full, embed))
            .await
            .map_err(|error| McpError::internal_error(format!("host rescan task panicked: {error}"), None))?
            .map_err(|error| McpError::internal_error(format!("host rescan: {error}"), None))?;
        refresh_cache_after_scan(state).await?;
        return Ok(RescanReport {
            scanned: stats.scanned,
            updated: stats.updated,
            docs_indexed: stats.docs_indexed,
            removed: stats.removed,
            elapsed_ms: 0,
        });
    }
    forward_rescan_and_refresh(state, paths, full, embed).await
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
async fn refresh_cache_after_scan(state: &Arc<ServerState>) -> Result<(), McpError> {
    let view = state.shared.store.read().await.view.clone();
    let root = state.shared.root.clone();
    let current_fingerprint = state.shared.cache.load().fingerprint;
    let scope = state.shared.scope.clone();
    let config = Arc::clone(&state.shared.config);
    let (store, cache) = tokio::task::spawn_blocking(move || {
        let store = Store::open_read_only_no_index(&root, &view)?;
        // Attach persisted doc↔code links (ADR-0008) on this blocking thread, before publish — the
        // LanceStore's own block_on must not nest inside the async reactor.
        let cache = (super::map_fingerprint::index_fingerprint(&store) != current_fingerprint).then(|| {
            let mut cache = MapCache::build(&store, super::l1_cache::budget_bytes_from(&config.resources));
            super::doc_links_cache::attach(&mut cache, &store, &config, &scope);
            cache
        });
        Ok::<(Store, Option<MapCache>), crate::store::StoreError>((store, cache))
    })
    .await
    .map_err(|error| McpError::internal_error(format!("refresh map task panicked: {error}"), None))?
    .map_err(|error| McpError::internal_error(format!("reopen read-only store: {error}"), None))?;
    *state.shared.store.write().await = store;
    if let Some(cache) = cache {
        state.shared.cache.store(Arc::new(cache));
    }
    state.shared.cache_generation.fetch_add(1, Ordering::Relaxed);
    Ok(())
}
