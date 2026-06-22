//! Helper bodies for the cache admin MCP tools (`cache_stats`, `cache_gc`,
//! `cache_clear`). Kept out of `helpers.rs` so that file stays under the
//! 1000-line cap.
//!
//! ## In-process GC vs the offline CLI path
//!
//! `serve` holds the store's `.basemind/.lock` advisory flock for its entire
//! lifetime (acquired by `Store::open`). [`crate::store_gc::run_gc`] re-acquires
//! that flock, so calling it in-process would deadlock against ourselves. Instead
//! these helpers call the *unlocked* primitives
//! [`crate::store_gc::collect_referenced_hashes`] + [`crate::store_gc::gc_blobs`]
//! while holding a `state.store` `RwLock` guard as the mutual-exclusion mechanism:
//! a held read guard blocks the only in-process writer (`scan_and_refresh`, which
//! takes `write()`), and cross-process scans are already impossible because serve
//! holds the flock. `run_gc` remains the correct primitive for the offline CLI.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{json_result, scan_and_refresh};
use super::types::{
    CacheClearParams, CacheClearResponse, CacheGcParams, CacheGcResponse, CacheStatsParams,
    CacheStatsResponse,
};
use crate::store_gc::{self, CacheComponent};

/// Body for the `cache_stats` MCP tool. Read-only: takes a `blocking_read()` store
/// guard inside `spawn_blocking` and gathers per-component sizes + blob accounting.
pub(super) async fn run_cache_stats(
    state: Arc<ServerState>,
    _params: CacheStatsParams,
) -> Result<CallToolResult, McpError> {
    let state_for_stats = Arc::clone(&state);
    let stats = tokio::task::spawn_blocking(move || {
        // Read guard: blocks `scan_and_refresh` for the (cheap) stat walk; cross-process
        // scans can't run because serve holds the flock.
        let store = state_for_stats.store.blocking_read();
        store_gc::cache_stats(&store.basemind_dir)
    })
    .await
    .map_err(|e| McpError::internal_error(format!("cache_stats join: {e}"), None))?
    .map_err(|e| McpError::internal_error(format!("cache_stats: {e}"), None))?;

    json_result(&CacheStatsResponse::from(stats))
}

/// Body for the `cache_gc` MCP tool. In-process mark-and-sweep over orphaned blobs.
/// Uses the unlocked `collect_referenced_hashes` + `gc_blobs` primitives under a
/// `blocking_read()` guard — NEVER `run_gc` (which would deadlock on serve's flock).
pub(super) async fn run_cache_gc(
    state: Arc<ServerState>,
    _params: CacheGcParams,
) -> Result<CallToolResult, McpError> {
    let state_for_gc = Arc::clone(&state);
    let report = tokio::task::spawn_blocking(move || {
        // The read guard blocks the only in-process writer (`scan_and_refresh`) for the
        // mark+sweep, so a concurrent rescan can't write a blob we then orphan-reap.
        let store = state_for_gc.store.blocking_read();
        let referenced = store_gc::collect_referenced_hashes(&store.basemind_dir)?;
        store_gc::gc_blobs(&store.basemind_dir, &referenced)
    })
    .await
    .map_err(|e| McpError::internal_error(format!("cache_gc join: {e}"), None))?
    .map_err(|e| McpError::internal_error(format!("cache_gc: {e}"), None))?;

    json_result(&CacheGcResponse::from(report))
}

/// Body for the `cache_clear` MCP tool. Parses + validates the component token,
/// gates the destructive (live-index-backing) components behind `confirm=true`, and
/// rebuilds the live state after a destructive clear so queries recover.
pub(super) async fn run_cache_clear(
    state: Arc<ServerState>,
    params: CacheClearParams,
) -> Result<CallToolResult, McpError> {
    // `views:<name>` clears a single view (bug #22). Safe in-process for any view EXCEPT the
    // one this server currently has open (deleting its live Fjall handle's dir would break it
    // — same hazard as `views`/`all`). Reclaims disk from stale rev/staged views while serving.
    if let Some(name) = params.component.strip_prefix("views:") {
        let name = name.to_string();
        let active_view = state.store.read().await.view.clone();
        if name == active_view {
            return Err(McpError::invalid_request(
                format!(
                    "view `{name}` is the one this server is serving; clearing it would break \
                     the live index. Stop the server and run `basemind cache clear --component \
                     views:{name}`, or serve a different view."
                ),
                None,
            ));
        }
        let dir = state.store.read().await.basemind_dir.clone();
        tokio::task::spawn_blocking(move || store_gc::clear_single_view(&dir, &name))
            .await
            .map_err(|e| McpError::internal_error(format!("cache_clear join: {e}"), None))?
            .map_err(|e| McpError::invalid_request(format!("cache_clear: {e}"), None))?;
        return json_result(&CacheClearResponse {
            component: params.component.clone(),
            cleared: true,
        });
    }

    let component: CacheComponent = params.component.parse().map_err(|e: String| {
        McpError::invalid_request(
            format!("{e} (valid: blobs|views|lance|git-cache|telemetry|all, or views:<name>)"),
            None,
        )
    })?;

    match component {
        // `all` removes the whole `.basemind/` dir, and `views` removes `index.fjall/` —
        // the directory the live `IndexDb` handle has OPEN. Deleting either out from under
        // the running server breaks the open Fjall tree (and, for `all`, the flock this
        // server holds): a stale handle pointing at a deleted dir. Refuse in-process and
        // point the operator at the offline CLI, which clears with no handles open.
        CacheComponent::All | CacheComponent::Views => Err(McpError::invalid_request(
            format!(
                "clearing `{}` removes the live Fjall index out from under the running \
                 server; stop the server and run `basemind cache clear --component {}`",
                component.as_str(),
                component.as_str()
            ),
            None,
        )),
        // Blobs are content-addressed files, not an open handle: safe to clear under a
        // write guard, then a rescan re-extracts + rewrites them. Require confirm because
        // queries briefly see missing L2 blobs until the rescan completes.
        CacheComponent::Blobs => {
            if !params.confirm {
                return Err(McpError::invalid_request(
                    "clearing `blobs` drops cached extractions; pass confirm=true to proceed \
                     (a rescan runs afterwards to rebuild them)",
                    None,
                ));
            }
            clear_live_component(Arc::clone(&state), component).await?;
            // Rebuild from source so subsequent queries don't hit missing blobs.
            scan_and_refresh(state, None).await?;
            json_result(&CacheClearResponse {
                component: component.as_str().to_string(),
                cleared: true,
            })
        }
        // Non-live caches: clear freely, no confirm, no rebuild needed.
        CacheComponent::Lance | CacheComponent::GitCache | CacheComponent::Telemetry => {
            clear_live_component(Arc::clone(&state), component).await?;
            json_result(&CacheClearResponse {
                component: component.as_str().to_string(),
                cleared: true,
            })
        }
    }
}

/// Clear a single component under a `blocking_write()` store guard. The write guard
/// serializes against `scan_and_refresh` and the stats/GC read guards for the wipe.
async fn clear_live_component(
    state: Arc<ServerState>,
    component: CacheComponent,
) -> Result<(), McpError> {
    tokio::task::spawn_blocking(move || {
        let store = state.store.blocking_write();
        store_gc::clear_component(&store.basemind_dir, component)
    })
    .await
    .map_err(|e| McpError::internal_error(format!("cache_clear join: {e}"), None))?
    .map_err(|e| McpError::internal_error(format!("cache_clear: {e}"), None))
}

/// Body for the `rescan` MCP tool. Re-indexes the working tree (or `paths`) in-process and,
/// because it is one of the few genuinely slow tools, emits MCP progress (when the client
/// supplies a token) and a completion logging notification. Lives here with the other admin-tool
/// bodies so `helpers.rs` stays under the line cap.
pub(super) async fn run_rescan(
    state: Arc<ServerState>,
    params: super::types::RescanParams,
    peer: &rmcp::Peer<rmcp::RoleServer>,
    progress_token: Option<rmcp::model::ProgressToken>,
) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    // `full` forces a complete working-tree scan even when `paths` is supplied (full wins);
    // `None` scoped_paths is the full-scan signal in `scan_and_refresh`. Repo-relative request
    // paths are joined to the absolute root — `scan_paths` strips the root prefix and silently
    // drops anything that is not under it, so a bare relative path would be a no-op scan.
    let scoped_paths: Option<Vec<std::path::PathBuf>> = params
        .paths
        .filter(|_| !params.full)
        .map(|v| v.into_iter().map(|p| state.root.join(p)).collect());

    let root = state.root.display().to_string();

    // Tell the client the rescan has started (progress is indeterminate up front — the scanner
    // discovers the file count as it walks).
    if let Some(token) = progress_token.clone() {
        super::notifications::emit_progress(
            peer,
            token,
            0.0,
            None,
            "rescan: scanning working tree",
        )
        .await;
    }

    let report = scan_and_refresh(Arc::clone(&state), scoped_paths).await?;

    // Surface the outcome as a logging notification (gated on the client's level) and close out
    // progress with the discovered file count as both value and total.
    super::notifications::emit_log(
        peer,
        &state.log_level,
        rmcp::model::LoggingLevel::Info,
        "basemind.rescan",
        serde_json::json!({
            "event": "rescan_complete",
            "scanned": report.stats.scanned,
            "updated": report.stats.updated,
            "removed": report.stats.removed,
            "extract_failed": report.stats.extract_failed,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        }),
    )
    .await;
    if let Some(token) = progress_token {
        let scanned = report.stats.scanned as f64;
        super::notifications::emit_progress(
            peer,
            token,
            scanned,
            Some(scanned),
            format!("rescan: done, {} files", report.stats.scanned),
        )
        .await;
    }

    json_result(&super::types::RescanResponse {
        scanned: report.stats.scanned,
        updated: report.stats.updated,
        removed: report.stats.removed,
        skipped_unchanged: report.stats.skipped_unchanged,
        skipped_no_lang: report.stats.skipped_no_lang,
        extract_failed: report.stats.extract_failed,
        elapsed_ms: started.elapsed().as_millis(),
        root,
    })
}
