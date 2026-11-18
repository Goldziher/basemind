//! Pure helper functions used by the tool methods. Kept out of `mod.rs` so the tool impl
//! block stays focused on dispatch logic. Everything here is `pub(super)`.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde::Serialize;

use super::ServerState;

pub(super) use super::helpers_calls::{run_find_callers, run_find_references};
#[cfg(all(feature = "documents", feature = "code-search"))]
pub(super) use super::helpers_documents::format_response;
pub(super) use super::helpers_grep::run_workspace_grep;
pub(super) use super::helpers_impls::run_find_implementations;
pub(super) use super::helpers_telemetry::record_call;

pub(super) use crate::engine::ops::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX};

/// Wrap a tool-shim body with telemetry instrumentation.
///
/// Captures `Instant::now()` before the body runs, serializes the params for a deterministic
/// hash, awaits the body, then records the resulting `CallToolResult` (or skips on `Err`) via
/// `record_call`. Each tool's shim becomes a one-liner.
///
/// Usage from `tools.rs` / `tools_memory.rs` / `tools_admin.rs`:
/// ```ignore
/// async fn outline(...) -> Result<CallToolResult, McpError> {
///     instrument_tool!(&self.state, "outline", params, run_outline(&self.state, params).await)
/// }
/// ```
#[macro_export]
macro_rules! instrument_tool {
    ($state:expr, $tool:literal, $params:expr, $body:expr) => {{
        let __started = ::std::time::Instant::now();
        let __params_json = ::serde_json::to_value(&$params).unwrap_or(::serde_json::Value::Null);
        let __result = $body;
        $crate::mcp::helpers::record_call($state, $tool, &__params_json, __started, &__result);
        __result
    }};
}

pub(super) use crate::engine::types::code::{kind_to_str, parse_kind};

/// Build a successful tool result carrying both a text mirror and SEP-2106 `structured_content`.
///
/// Every tool routes its response through here, so populating `structured_content` in one place
/// gives the whole MCP surface typed output: `CallToolResult::structured` sets the parsed JSON on
/// `structured_content` (for clients that consume typed results) and keeps a `to_string()` text
/// block in `content` (for clients that only read text). No tool advertises an `output_schema`:
/// one tool answers in several shapes, and `tests/mcp_smoke.rs` holds every verb tool to that.
pub(super) fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let value =
        serde_json::to_value(value).map_err(|e| McpError::internal_error(format!("serialize response: {e}"), None))?;
    Ok(CallToolResult::structured(value))
}

pub(super) use timing::elapsed_us;

/// The `elapsed_us` contract ([`crate::engine::timing`]), named here for the docs that link it.
pub(super) use crate::engine::timing;

/// The only thing the MCP rescan wants out of a scan's per-file outcomes: which paths to fold into
/// the cache delta, and which to drop from it.
///
/// A [`ScanObserver`](crate::scanner::ScanObserver) rather than a filter over a collected `Vec`,
/// because the two projections here are all that ever needed to survive a file's result — the
/// status, the buffered index entry and the pending batches never leave the drive chunk.
#[derive(Default)]
struct RescanPaths {
    updated: Vec<crate::path::RelPath>,
    removed: Vec<crate::path::RelPath>,
}

impl crate::scanner::ScanObserver for RescanPaths {
    fn on_file(&mut self, result: crate::scanner::FileResult) {
        match result.status {
            crate::scanner::FileStatus::Updated { .. } => {
                self.updated.push(crate::path::RelPath::from(result.path.as_str()));
            }
            crate::scanner::FileStatus::Removed => {
                self.removed.push(crate::path::RelPath::from(result.path.as_str()));
            }
            _ => {}
        }
    }
}

/// Run the scanner in-process and refresh the in-RAM caches, returning the raw
/// [`crate::scanner::ScanReport`]. Shared by the `rescan` MCP tool and the startup
/// auto-scan in [`super::BasemindServer::new`] so both go through the exact same scan +
/// cache-swap path against the server's own `Store` — never releasing or colliding with
/// the Fjall lock the `serve` process already holds.
///
/// Holds the `state.shared.store` write lock for the duration of the scan; MCP query tools block
/// while it runs (acceptable for small/medium repos — rescan is agent-triggered, not
/// per-request). `scoped_paths` limits the scan to those repo-relative paths; `None` runs a
/// full working-tree scan. `named` says the paths came from a person or an agent, not from the
/// watcher: a named file that reads as unchanged has its blob validated.
pub(super) async fn scan_and_refresh(
    state: Arc<ServerState>,
    scoped_paths: Option<Vec<std::path::PathBuf>>,
    embed: crate::scanner::EmbedMode,
    named: bool,
) -> Result<crate::scanner::ScanReport, McpError> {
    if state.shared.read_only {
        return Err(McpError::invalid_request(
            "this basemind serve is read-only: another serve process holds the write lock for \
             this repo, so it owns index refresh. Reads are served from the shared index; run \
             rescans from the lock-holding serve (or close it and retry).",
            None,
        ));
    }
    // The last in-process full-tree walk that the CLI's own guard does not cover (issue #62). The
    // tool-subcommand dispatcher never calls `guard_workspace_root`, so `basemind rescan`
    // reaches `crate::scanner::scan` here — and so does the `index_rescan` MCP tool, plus
    // the boot/watcher scans in `mcp::background`. Guarding at this single funnel covers all of
    // them; it does NOT cover the read-only store open that `cli::context::build_server` already
    // performed, so a refused root can still have had its cache directory created by that point.
    //
    // Used as a pure admission check: the scan keeps running against `state.shared.root`, not the
    // resolved path. This server's store, `MapCache` and any scoped paths were all built from that
    // exact spelling, so swapping in a differently-spelled-but-equal root here would make
    // `strip_prefix` miss every scoped path. Resolution belongs where the root is first chosen.
    crate::config::root_guard::workspace_root_verdict(&state.shared.root).map_err(|refusal| {
        McpError::invalid_request(
            crate::config::root_guard::refusal_message(&state.shared.root, refusal),
            None,
        )
    })?;
    let root = state.shared.root.clone();
    let config = Arc::clone(&state.shared.config);

    let was_scoped = scoped_paths.is_some();
    let state_for_scan = Arc::clone(&state);
    let (report, paths) = tokio::task::spawn_blocking(move || {
        let mut store = state_for_scan.shared.store_write();
        // The store may have been opened via `Store::open_read_only` (the CLI / MCP query path),
        // which never creates `view_dir` on disk. On a workspace's first-ever scan that directory
        // is still missing here, and `flush()` would fail with ENOENT writing `index.msgpack.tmp`.
        // ~keep
        store.ensure_dirs()?;
        let mut observer = RescanPaths::default();
        let cancel = crate::scanner::ScanCancel::new();
        // A full Inline pass loads the embedding engine; release it once the pass is over (incremental
        // watcher batches keep it warm for the searches that follow an edit).
        #[cfg(feature = "intelligence")]
        let _embed_pass = (matches!(embed, crate::scanner::EmbedMode::Inline) && scoped_paths.is_none())
            .then(crate::embeddings::begin_embed_pass);
        let report = if let Some(paths) = scoped_paths {
            let scan = if named {
                crate::scanner::scan_named_paths_with_observer
            } else {
                crate::scanner::scan_paths_with_observer
            };
            scan(&root, &mut store, &config, &paths, embed, &cancel, &mut observer)
        } else {
            crate::scanner::scan_with_observer(
                &root,
                &mut store,
                &config,
                crate::scanner::ScanSource::WorkingTree,
                embed,
                &cancel,
                &mut observer,
            )
        };
        report.map(|report| (report, observer))
    })
    .await
    .map_err(|e| McpError::internal_error(format!("scan join: {e}"), None))?
    .map_err(|e| rescan_error(&e))?;

    if was_scoped && report.stats.updated == 0 && report.stats.removed == 0 {
        return Ok(report);
    }

    let RescanPaths { updated, removed } = paths;

    let new_cache = {
        let store = state.store_read().await;
        let corpus_bytes: u64 = store.index.files.values().map(|e| e.size_bytes).sum();
        state
            .shared
            .corpus_bytes
            .store(corpus_bytes, std::sync::atomic::Ordering::Relaxed);
        let current = state.shared.cache.load();
        if was_scoped {
            Some(current.with_delta(&store, &updated, &removed))
        } else {
            current.refreshed(
                &store,
                super::l1_cache::budget_bytes_from(&state.shared.config.resources),
            )
        }
    };
    if let Some(cache) = new_cache {
        state.shared.cache.store(std::sync::Arc::new(cache));
    }
    state.shared.note_index_changed();

    Ok(report)
}

/// Body for the `telemetry_summary` MCP tool. Thin wrapper — the aggregation logic
/// lives in [`super::telemetry::summarize`] so this module stays under the line cap.
pub(super) async fn run_telemetry_summary(
    state: &ServerState,
    params: super::types::TelemetrySummaryParams,
) -> Result<CallToolResult, McpError> {
    let response = super::telemetry::summarize(state.telemetry.path(), params).await?;
    json_result(&response)
}

/// The error a refused or failed rescan answers with. Another process writing this workspace is the
/// caller's to retry or rescan through, and is marked [`BUSY_DATA`] so the CLI exits `3` for it as
/// it does for every other lock contention.
pub(crate) fn rescan_error(error: &crate::scanner::ScanError) -> McpError {
    match error {
        crate::scanner::ScanError::Store(store) if store.is_lock_contention() => {
            McpError::invalid_request(format!("rescan: {error}"), Some(serde_json::json!({ BUSY_DATA: true })))
        }
        _ => McpError::internal_error(format!("rescan: {error}"), None),
    }
}

/// Key of the `data` field that marks a tool error as contention for the workspace writer lock.
pub(crate) const BUSY_DATA: &str = "busy";
