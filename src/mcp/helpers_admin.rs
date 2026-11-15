//! Bodies of the index verb tools: `index_status` and `index_rescan`. Kept out of `helpers.rs` so
//! that file stays under the 1000-line cap.
//!
//! The cache operations (`stats`, `gc`, `clear`) have no MCP tool: `basemind cache …` runs them
//! offline against `store_gc`, which is the only safe place to sweep a blob store shared by every
//! workspace on the machine.

use std::collections::BTreeMap;
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{elapsed_us, json_result, scan_and_refresh};
use super::tools::{blob_divergence_note, count_fm_blobs};
use super::types::StatusResponse;
use super::types_tools::IndexRescanParams;

/// Where `index_rescan` reports progress: the calling peer and the progress token it sent, if any.
pub(super) type Progress<'a> = Option<(&'a rmcp::Peer<rmcp::RoleServer>, Option<rmcp::model::ProgressToken>)>;

/// `index_status`: index health plus repository identity.
pub(super) async fn index_status(state: &ServerState) -> Result<CallToolResult, McpError> {
    run_status(state).await
}

/// `index_rescan`: re-index changed files, the given paths, or the whole workspace.
pub(super) async fn index_rescan(
    state: Arc<ServerState>,
    p: IndexRescanParams,
    progress: Progress<'_>,
) -> Result<CallToolResult, McpError> {
    let (peer, token) = progress.map_or((None, None), |(peer, token)| (Some(peer), token));
    run_rescan(
        state,
        super::types::RescanParams {
            paths: p.paths,
            full: p.full.unwrap_or(false),
        },
        peer,
        token,
    )
    .await
}

/// Body of `index_status`: index health for the served view — file counts, per-language
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
    let identity = repo_identity(state);
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
                partial: false,
                degraded: None,
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
                config_stamp: config_stamp(state),
                branch: identity.branch.clone(),
                head_sha: identity.head_sha.clone(),
                head_short_sha: identity.head_short_sha.clone(),
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
    let last_scan = crate::store::read_status_sidecar(&store.basemind_dir);
    json_result(&StatusResponse {
        file_count,
        blob_count,
        note,
        rebuild_in_progress: false,
        partial: last_scan.as_ref().is_some_and(|status| status.partial),
        degraded: last_scan
            .as_ref()
            .and_then(crate::store::StatusSidecar::degraded_summary),
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
        config_stamp: config_stamp(state),
        branch: identity.branch,
        head_sha: identity.head_sha,
        head_short_sha: identity.head_short_sha,
        elapsed_us: elapsed_us(body),
    })
}

/// Branch and HEAD of the served repository; all `None` outside a git repository.
struct RepoIdentity {
    branch: Option<String>,
    head_sha: Option<String>,
    head_short_sha: Option<String>,
}

fn repo_identity(state: &ServerState) -> RepoIdentity {
    let info = state.shared.repo.as_ref().and_then(|repo| repo.info().ok());
    RepoIdentity {
        branch: info.as_ref().and_then(|info| info.branch.clone()),
        head_sha: info.as_ref().and_then(|info| info.head_sha.clone()),
        head_short_sha: info.and_then(|info| info.head_short_sha),
    }
}

fn config_stamp(state: &ServerState) -> String {
    crate::config::daemon::ConfigStamp::of(&state.shared.root).to_string()
}

/// Body of `index_rescan`. Re-indexes the working tree (or `paths`) in-process and,
/// because it is one of the few genuinely slow operations, emits MCP progress (when the client
/// supplies a token); the summary counts are in the tool result.
///
/// `peer` is `None` on the CLI path: a one-shot `basemind admin rescan` has no MCP peer to notify,
/// so the progress notifications are skipped while the scan itself is identical.
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
                    McpError::invalid_params(format!("`index_rescan`: path {p:?} escapes the repository root"), None)
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
        docs_indexed: stats.docs_indexed,
        docs_degraded: stats.docs_degraded,
        docs_text_fallback: stats.docs_text_fallback,
        doc_timeouts: stats.doc_timeouts,
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
    docs_indexed: usize,
    docs_degraded: usize,
    docs_text_fallback: usize,
    doc_timeouts: usize,
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
            docs_indexed: report.docs_indexed,
            docs_degraded: report.docs_degraded,
            docs_text_fallback: report.docs_text_fallback,
            doc_timeouts: report.doc_timeouts,
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
        docs_indexed: report.stats.docs_indexed,
        docs_degraded: report.stats.docs_degraded,
        docs_text_fallback: report.stats.docs_text_fallback,
        doc_timeouts: report.stats.doc_timeouts,
        removed: report.stats.removed,
        skipped_unchanged: report.stats.skipped_unchanged,
        skipped_no_lang: report.stats.skipped_no_lang,
        extract_failed: report.stats.extract_failed,
    })
}
