//! Helper bodies for the consolidated `admin` domain tool — one `run_<mode>` per [`AdminMode`],
//! plus the [`run_admin`] dispatcher the `#[tool]` shim and the CLI both call. Kept out of
//! `helpers.rs` so that file stays under the 1000-line cap.
//!
//! The four text-compression modes (`compress` / `delta` / `checkpoint` / `waste`) keep their
//! bodies in `helpers_compress.rs`, alongside `expand`'s, which shares their machinery.
//!
//! ## GC is a non-destructive report while the blob store is machine-global
//!
//! The blob store is a single machine-global directory shared by every workspace, so neither the
//! MCP `cache_gc` tool (this file) nor the offline CLI `cache gc`
//! ([`crate::store_gc::run_gc`]) can safely mark-and-sweep: a single session enumerates only ONE
//! workspace's references and would reap blobs other workspaces still need. Both paths therefore
//! return a non-destructive report ([`crate::store_gc::gc_report_only`], `removed == 0`).
//! Reference-counted GC that spans every workspace is the daemon's job (Track E).

use std::collections::BTreeMap;
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{elapsed_us, json_result, require_git_repo, scan_and_refresh};
use super::mode::{AdminMode, reject_unsupported};
use super::tools::{blob_divergence_note, count_fm_blobs};
use super::types::{
    CacheClearParams, CacheClearResponse, CacheGcParams, CacheGcResponse, CacheStatsParams, CacheStatsResponse,
    RepoInfoResponse, StatusResponse,
};
use super::types_admin::AdminParams;
use super::types_compress::{CheckpointParams, CompressParams, DeltaParams, DetectWasteParams};
use crate::store_gc::{self, CacheComponent};

/// The progress channel `rescan` reports on: the calling peer and the progress token it supplied.
/// `None` on the CLI path, which has no MCP peer.
type Progress<'a> = Option<(&'a rmcp::Peer<rmcp::RoleServer>, Option<rmcp::model::ProgressToken>)>;

/// Fail a mode that was given a field belonging to some other mode.
///
/// Inverted against `allowed` rather than listing every rejected field per mode: with eleven modes
/// and fifteen sibling fields, an explicit per-mode reject list is where a newly added field
/// silently becomes accept-everywhere.
fn reject_foreign_fields(mode: AdminMode, present: &[(&str, bool)], allowed: &[&str]) -> Result<(), McpError> {
    let foreign: Vec<(&str, bool)> = present
        .iter()
        .filter(|(field, _)| !allowed.contains(field))
        .copied()
        .collect();
    reject_unsupported(AdminMode::DOMAIN, mode.as_str(), &foreign)
}

/// Unwrap a field this mode cannot run without, naming the exact `mode`/field pair.
fn require_field<T>(mode: AdminMode, field: &str, value: Option<T>) -> Result<T, McpError> {
    value
        .ok_or_else(|| McpError::invalid_params(format!("`admin` mode=\"{}\" requires `{field}`", mode.as_str()), None))
}

/// Dispatch the single `admin` tool onto the per-operation helper its `mode` selects.
///
/// Fields belonging to another mode are rejected rather than dropped: a silently ignored
/// `component` on a `gc` call reads to an agent as a successful targeted clear.
pub(super) async fn run_admin(
    state: Arc<ServerState>,
    params: AdminParams,
    progress: Progress<'_>,
) -> Result<CallToolResult, McpError> {
    let AdminParams {
        mode,
        paths,
        full,
        window,
        tool,
        component,
        confirm,
        text,
        path,
        level,
        preserve_code,
        target_tokens,
        old,
        new,
        log,
    } = params;
    let present = [
        ("paths", paths.is_some()),
        ("full", full.is_some()),
        ("window", window.is_some()),
        ("tool", tool.is_some()),
        ("component", component.is_some()),
        ("confirm", confirm.is_some()),
        ("text", text.is_some()),
        ("path", path.is_some()),
        ("level", level.is_some()),
        ("preserve_code", preserve_code.is_some()),
        ("target_tokens", target_tokens.is_some()),
        ("old", old.is_some()),
        ("new", new.is_some()),
        ("log", log.is_some()),
    ];
    let reject = |allowed: &[&str]| reject_foreign_fields(mode, &present, allowed);

    match mode {
        AdminMode::Status => {
            reject(&[])?;
            run_status(&state).await
        }
        AdminMode::Repo => {
            reject(&[])?;
            run_repo(&state)
        }
        AdminMode::Rescan => {
            reject(&["paths", "full"])?;
            let (peer, token) = progress.map_or((None, None), |(peer, token)| (Some(peer), token));
            run_rescan(
                state,
                super::types::RescanParams {
                    paths,
                    full: full.unwrap_or(false),
                },
                peer,
                token,
            )
            .await
        }
        AdminMode::CacheStats => {
            reject(&[])?;
            run_cache_stats(state, CacheStatsParams {}).await
        }
        AdminMode::Gc => {
            reject(&[])?;
            run_cache_gc(state, CacheGcParams {}).await
        }
        AdminMode::CacheClear => {
            reject(&["component", "confirm"])?;
            run_cache_clear(
                state,
                CacheClearParams {
                    component: require_field(mode, "component", component)?,
                    confirm: confirm.unwrap_or(false),
                },
            )
            .await
        }
        AdminMode::Telemetry => {
            reject(&["window", "tool"])?;
            super::helpers::run_telemetry_summary(&state, super::types::TelemetrySummaryParams { window, tool }).await
        }
        AdminMode::Compress => {
            reject(&["text", "path", "level", "preserve_code", "target_tokens"])?;
            super::helpers_compress::run_compress(
                &state,
                CompressParams {
                    text,
                    path,
                    level,
                    preserve_code: preserve_code.unwrap_or(true),
                    target_tokens,
                },
            )
            .await
        }
        AdminMode::Delta => {
            reject(&["old", "new"])?;
            super::helpers_compress::run_delta(
                &state,
                DeltaParams {
                    old: require_field(mode, "old", old)?,
                    new: require_field(mode, "new", new)?,
                },
            )
            .await
        }
        AdminMode::Checkpoint => {
            reject(&["text"])?;
            super::helpers_compress::run_checkpoint(
                &state,
                CheckpointParams {
                    text: require_field(mode, "text", text)?,
                },
            )
            .await
        }
        AdminMode::Waste => {
            reject(&["log"])?;
            super::helpers_compress::run_detect_waste(
                &state,
                DetectWasteParams {
                    log: require_field(mode, "log", log)?,
                },
            )
            .await
        }
    }
}

/// Body for `admin` mode `status`: index health for the served view — file counts, per-language
/// breakdown, on-disk blob count, and the lifecycle flags a client needs to tell "still indexing"
/// apart from "nothing indexed".
///
/// A `try_read` miss is a report, not an error: another basemind process holding the store lock
/// means a rebuild is in flight, which is exactly what `rebuild_in_progress` tells the caller.
async fn run_status(state: &ServerState) -> Result<CallToolResult, McpError> {
    let body = std::time::Instant::now();
    let indexing = state
        .shared
        .initial_scan_active
        .load(std::sync::atomic::Ordering::Relaxed);
    let index_build_ms = {
        let ms = state.shared.initial_scan_ms.load(std::sync::atomic::Ordering::Relaxed);
        (ms > 0).then_some(ms)
    };
    let warming = state.shared.cache_warming.load(std::sync::atomic::Ordering::Relaxed);
    let warm_ms = {
        let ms = state.shared.cache_warm_ms.load(std::sync::atomic::Ordering::Relaxed);
        (ms > 0).then_some(ms)
    };
    let notice = state.lifecycle_notice();
    let store = match state.shared.store.try_read() {
        Ok(store) => store,
        Err(_) => {
            return json_result(&StatusResponse {
                file_count: 0,
                blob_count: count_fm_blobs(),
                note: Some(
                    "a rebuild is in progress (another basemind process holds the store \
                     lock); index counts are unavailable until it completes"
                        .to_string(),
                ),
                rebuild_in_progress: true,
                indexing,
                index_build_ms,
                warming,
                warm_ms,
                notice,
                total_size_bytes: 0,
                languages: BTreeMap::new(),
                cache_dir: crate::lang::grammar_cache_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(unresolved)".to_string()),
                schema_version: crate::extract::SCHEMA_VER,
                root: state.shared.root.display().to_string(),
                submodules: state
                    .shared
                    .repo
                    .as_ref()
                    .map(|r| r.submodule_paths())
                    .unwrap_or_default(),
                elapsed_us: elapsed_us(body),
            });
        }
    };
    let mut by_lang_ref: BTreeMap<&str, usize> = BTreeMap::new();
    let mut total_size: u64 = 0;
    for entry in store.index.files.values() {
        *by_lang_ref.entry(entry.language.as_str()).or_insert(0) += 1;
        total_size = total_size.saturating_add(entry.size_bytes);
    }
    let by_lang: BTreeMap<String, usize> = by_lang_ref.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    let cache_dir = crate::lang::grammar_cache_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(unresolved)".to_string());
    let submodules = state
        .shared
        .repo
        .as_ref()
        .map(|r| r.submodule_paths())
        .unwrap_or_default();
    let file_count = store.index.files.len();
    let blob_count = count_fm_blobs();
    let note = blob_divergence_note(file_count, blob_count);
    json_result(&StatusResponse {
        file_count,
        blob_count,
        note,
        rebuild_in_progress: false,
        indexing,
        index_build_ms,
        warming,
        warm_ms,
        notice,
        total_size_bytes: total_size,
        languages: by_lang,
        cache_dir,
        schema_version: crate::extract::SCHEMA_VER,
        root: state.shared.root.display().to_string(),
        submodules,
        elapsed_us: elapsed_us(body),
    })
}

/// Body for `admin` mode `repo`: repository identity — workdir, branch, full + short HEAD sha.
fn run_repo(state: &ServerState) -> Result<CallToolResult, McpError> {
    let body = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    let info = repo
        .info()
        .map_err(|e| McpError::internal_error(format!("repo info: {e}"), None))?;
    json_result(&RepoInfoResponse {
        workdir: info.workdir.display().to_string(),
        head_sha: info.head_sha,
        head_short_sha: info.head_short_sha,
        branch: info.branch,
        elapsed_us: elapsed_us(body),
    })
}

/// Body for `admin` mode `cache_stats`. Read-only: takes a `blocking_read()` store
/// guard inside `spawn_blocking` and gathers per-component sizes + blob accounting.
async fn run_cache_stats(state: Arc<ServerState>, _params: CacheStatsParams) -> Result<CallToolResult, McpError> {
    let state_for_stats = Arc::clone(&state);
    let stats = tokio::task::spawn_blocking(move || {
        let store = state_for_stats.shared.store.blocking_read();
        store_gc::cache_stats(&store.basemind_dir)
    })
    .await
    .map_err(|e| McpError::internal_error(format!("cache_stats join: {e}"), None))?
    .map_err(|e| McpError::internal_error(format!("cache_stats: {e}"), None))?;

    json_result(&CacheStatsResponse::from(stats))
}

/// Body for `admin` mode `gc`. Reports blob-store accounting without deleting.
///
/// The blob store is machine-global now (shared by every workspace), so a single serve session can
/// only enumerate ONE workspace's references — never the full live set across all workspaces. A
/// mark-and-sweep from here would reap blobs other workspaces still need, so the in-process GC is a
/// non-destructive report (`removed == 0`); reference-counted GC that spans every workspace is the
/// daemon's job (Track E). `scanned` still reflects the blobs inspected so callers see the store
/// size.
async fn run_cache_gc(state: Arc<ServerState>, _params: CacheGcParams) -> Result<CallToolResult, McpError> {
    let _ = &state;
    let report = tokio::task::spawn_blocking(store_gc::gc_report_only)
        .await
        .map_err(|e| McpError::internal_error(format!("cache_gc join: {e}"), None))?
        .map_err(|e| McpError::internal_error(format!("cache_gc: {e}"), None))?;

    json_result(&CacheGcResponse::from(report))
}

/// Body for `admin` mode `cache_clear`. Parses + validates the component token,
/// gates the destructive (live-index-backing) components behind `confirm=true`, and
/// rebuilds the live state after a destructive clear so queries recover.
async fn run_cache_clear(state: Arc<ServerState>, params: CacheClearParams) -> Result<CallToolResult, McpError> {
    if let Some(name) = params.component.strip_prefix("views:") {
        let name = name.to_string();
        let active_view = state.shared.store.read().await.view.clone();
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
        let dir = state.shared.store.read().await.basemind_dir.clone();
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
        CacheComponent::All | CacheComponent::Views => Err(McpError::invalid_request(
            format!(
                "clearing `{}` removes the live Fjall index out from under the running \
                 server; stop the server and run `basemind cache clear --component {}`",
                component.as_str(),
                component.as_str()
            ),
            None,
        )),
        CacheComponent::Blobs => {
            if !params.confirm {
                return Err(McpError::invalid_request(
                    "clearing `blobs` drops cached extractions; pass confirm=true to proceed \
                     (a rescan runs afterwards to rebuild them)",
                    None,
                ));
            }
            clear_live_component(Arc::clone(&state), component).await?;
            scan_and_refresh(state, None, crate::scanner::EmbedMode::Inline).await?;
            json_result(&CacheClearResponse {
                component: component.as_str().to_string(),
                cleared: true,
            })
        }
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
async fn clear_live_component(state: Arc<ServerState>, component: CacheComponent) -> Result<(), McpError> {
    tokio::task::spawn_blocking(move || {
        let store = state.shared.store.blocking_write();
        store_gc::clear_component(&store.basemind_dir, component)
    })
    .await
    .map_err(|e| McpError::internal_error(format!("cache_clear join: {e}"), None))?
    .map_err(|e| McpError::internal_error(format!("cache_clear: {e}"), None))
}

/// Body for `admin` mode `rescan`. Re-indexes the working tree (or `paths`) in-process and,
/// because it is one of the few genuinely slow operations, emits MCP progress (when the client
/// supplies a token) and a completion logging notification.
///
/// `peer` is `None` on the CLI path: a one-shot `basemind admin rescan` has no MCP peer to notify,
/// so the notifications are skipped while the scan itself is identical.
async fn run_rescan(
    state: Arc<ServerState>,
    params: super::types::RescanParams,
    peer: Option<&rmcp::Peer<rmcp::RoleServer>>,
    progress_token: Option<rmcp::model::ProgressToken>,
) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let scoped_paths: Option<Vec<std::path::PathBuf>> = match params.paths.filter(|_| !params.full) {
        None => None,
        Some(requested) => {
            let mut out = Vec::with_capacity(requested.len());
            for p in requested {
                let normalized = crate::path::normalize_query_path(&p, &state.shared.root).ok_or_else(|| {
                    McpError::invalid_params(format!("rescan: path {p:?} escapes the repository root"), None)
                })?;
                out.push(state.shared.root.join(normalized));
            }
            Some(out)
        }
    };

    let root = state.shared.root.display().to_string();

    if let (Some(peer), Some(token)) = (peer, progress_token.clone()) {
        super::notifications::emit_progress(peer, token, 0.0, None, "rescan: scanning working tree").await;
    }

    let stats = fetch_rescan_stats(&state, scoped_paths).await?;

    if let Some(peer) = peer {
        #[allow(deprecated)]
        super::notifications::emit_log(
            peer,
            &state.log_level,
            rmcp::model::LoggingLevel::Info,
            "basemind.rescan",
            serde_json::json!({
                "event": "rescan_complete",
                "scanned": stats.scanned,
                "updated": stats.updated,
                "removed": stats.removed,
                "extract_failed": stats.extract_failed,
                "elapsed_ms": started.elapsed().as_millis() as u64,
            }),
        )
        .await;
    }
    if let (Some(peer), Some(token)) = (peer, progress_token) {
        let scanned = stats.scanned as f64;
        super::notifications::emit_progress(
            peer,
            token,
            scanned,
            Some(scanned),
            format!("rescan: done, {} files", stats.scanned),
        )
        .await;
    }

    json_result(&super::types::RescanResponse {
        scanned: stats.scanned,
        updated: stats.updated,
        removed: stats.removed,
        skipped_unchanged: stats.skipped_unchanged,
        skipped_no_lang: stats.skipped_no_lang,
        extract_failed: stats.extract_failed,
        elapsed_ms: started.elapsed().as_millis(),
        root,
    })
}

/// The `rescan` counts a [`RescanResponse`](super::types::RescanResponse) reports, sourced either
/// from a local in-process scan or a scan forwarded to the daemon.
struct RescanStats {
    scanned: usize,
    updated: usize,
    removed: usize,
    skipped_unchanged: usize,
    skipped_no_lang: usize,
    extract_failed: usize,
}

/// Run the rescan and return its counts. A `daemon_writer` serve forwards the scan to the daemon
/// and rebuilds its read-only map (the daemon RPC carries no per-file skip breakdown, so those read
/// 0); every other serve scans locally under its own write lock via [`scan_and_refresh`].
///
/// `scoped_paths` already encodes the caller's `full` flag — it is `None` for a full working-tree
/// scan (which the daemon likewise treats as "scan everything") and `Some` only for an incremental
/// rescan — so no separate `full` argument is threaded here.
async fn fetch_rescan_stats(
    state: &Arc<ServerState>,
    scoped_paths: Option<Vec<std::path::PathBuf>>,
) -> Result<RescanStats, McpError> {
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.shared.daemon_writer {
        let report = super::daemon_forward::writer_rescan_and_refresh(state, scoped_paths, false, true).await?;
        return Ok(RescanStats {
            scanned: report.scanned,
            updated: report.updated,
            removed: report.removed,
            skipped_unchanged: 0,
            skipped_no_lang: 0,
            extract_failed: 0,
        });
    }
    let report = scan_and_refresh(Arc::clone(state), scoped_paths, crate::scanner::EmbedMode::Inline).await?;
    Ok(RescanStats {
        scanned: report.stats.scanned,
        updated: report.stats.updated,
        removed: report.stats.removed,
        skipped_unchanged: report.stats.skipped_unchanged,
        skipped_no_lang: report.stats.skipped_no_lang,
        extract_failed: report.stats.extract_failed,
    })
}
