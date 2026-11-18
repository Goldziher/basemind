//! Parameter shapes (deserialized from MCP tool-call arguments) and JSON response shapes
//! (serialized into tool-call results). Kept separate from `tools.rs` so the impl block
//! itself stays readable and within the per-file size budget.

use std::collections::BTreeMap;

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::path::RelPath;

pub(crate) use crate::engine::types::LifecycleNotice;

#[cfg(feature = "memory")]
pub(super) use crate::engine::types::default_true;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct StatusResponse {
    pub file_count: usize,
    /// Count of content-addressed blob files in `.basemind/blobs/` (one `.fm.msgpack` per
    /// indexed content hash). Reported alongside `file_count` so a lost/empty view index over
    /// live blobs is visible rather than silently reading `file_count: 0` (bug #10).
    pub blob_count: usize,
    /// One-line advisory, present only when the view index is empty but blobs exist on disk
    /// (index lost/wiped) — suggests a rescan. Absent for a populated or legitimately
    /// unscanned (no-blobs) view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// `true` when the code index cannot answer until a scan rebuilds it: an older basemind wrote
    /// it, or a rebuild was interrupted. `note` says how to fix it. Absent (false) otherwise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rescan_needed: bool,
    /// `true` when a writer (a running `scan`/`rescan`/`watch`) currently holds the store
    /// lock, so this report was served *without* blocking on the in-progress rebuild. The
    /// index counts (`file_count`, `languages`) reflect the pre-rebuild state or are omitted;
    /// `blob_count` is still read fresh from disk. Absent (false) on the common uncontended
    /// path — status then reflects the fully-committed index.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rebuild_in_progress: bool,
    /// `true` when the workspace's last full scan was cancelled or aborted before covering it (daemon
    /// shutdown, idle reap, a full disk), so `file_count` and the queries reflect only the files
    /// completed so far, not the repository. A full rescan clears it. Absent (false) otherwise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// One-line reason the index is degraded: a partial scan (with its reason and progress) or a
    /// resolve pass that never completed, leaving resolved references and callers stale. Absent when
    /// the last scan finished cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<String>,
    /// `true` while this serve's boot-time initial scan (auto-scan of an empty index) is still
    /// running. A client seeing this should treat empty query results as "index not ready yet"
    /// and poll again, rather than "no matches". Absent (false) once the index is built.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub indexing: bool,
    /// Wall-clock duration of this serve's boot-time initial scan, in milliseconds, once complete.
    /// Reports index-build time separately from query time so the first query's latency is not
    /// conflated with the one-time indexing cost. Absent when no initial scan ran this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_build_ms: Option<u64>,
    /// `true` while this serve's boot-time in-RAM code-map preload is still running (the index exists
    /// on disk but is loading into memory). Like `indexing`, a client should treat empty/partial query
    /// results as "not ready yet" and retry shortly rather than "no matches". Absent once warm.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub warming: bool,
    /// Wall-clock duration of the boot-time preload in milliseconds, once complete. Absent while still
    /// warming or when the preload wasn't deferred this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_ms: Option<u64>,
    /// `true` while this serve's boot-time git-history sync runs. A `git_log` / `git_blame` cursor
    /// issued before it ends may be refused as stale. Absent once the sync is done.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub history_syncing: bool,
    /// Current lifecycle notice (warming / building / rescanning) with an actionable message, or absent
    /// when the server is fully ready. Mirrors the `notice` on every read-tool response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    pub total_size_bytes: u64,
    pub languages: BTreeMap<String, usize>,
    /// basemind's cache directory for this workspace: its index views, git cache and telemetry.
    pub cache_dir: String,
    /// The tree-sitter grammar cache, which the language pack owns and other tools share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grammar_dir: Option<String>,
    pub schema_version: u16,
    pub root: String,
    /// Forward-slash worktree roots of every submodule declared in `.gitmodules`. Always
    /// reported regardless of `scan.skip_submodules` — lets clients see the boundary the
    /// scanner respects (or didn't, when the knob is disabled). Empty for repos with no
    /// submodules and for non-repo serves.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub submodules: Vec<RelPath>,
    /// Size and modification time (`<bytes>B@<unix secs>`, or `absent`) of the workspace's
    /// `basemind.toml` on disk right now. A changed value between two calls means the file was
    /// edited; a long-lived daemon reloads it on the next request for that workspace.
    pub config_stamp: String,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / store
    /// lookup + response construction), excluding MCP transport, argument deserialization, and
    /// response serialization. Distinct from `index_build_ms` / `warm_ms`, which report the
    /// one-time boot-scan and preload costs. See [`crate::mcp::helpers::timing`] for the contract.
    /// Current branch; absent outside a git repository or on a detached HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Full HEAD sha; absent outside a git repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_short_sha: Option<String>,
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RescanParams {
    /// Optional list of repo-relative paths to scope the rescan. When omitted
    /// the full repo is walked. Paths are forward-slash with no leading `/`.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct RescanResponse {
    pub scanned: usize,
    pub updated: usize,
    /// Documents (PDF / Office / HTML / MDX — anything with no tree-sitter grammar) re-extracted and
    /// re-embedded this rescan.
    ///
    /// Reported separately from `updated`, which counts CODE-map updates only. Without this a
    /// document-only change reconciled to nothing at all: the file was scanned and genuinely
    /// refreshed, but landed in no reported bucket, so `scanned` exceeded the sum of the counts and
    /// `updated: 0` read as "rescan ignored my document" when the content had in fact been updated.
    pub docs_indexed: usize,
    /// Documents chunked by the linear fallback instead of xberg's markdown splitter because they
    /// exceeded `[documents] markdown_chunk_max_bytes` / `plain_text_chunk_max_bytes`.
    pub docs_degraded: usize,
    /// Subset of `docs_degraded`: structured extraction failed (parse error or security limit, e.g. a
    /// Helm template as YAML or a CSV past the cell limit) and the file was re-read as `text/plain`.
    pub docs_text_fallback: usize,
    /// Documents abandoned because extraction overran `[documents] extraction_timeout_secs`; retried
    /// on the next scan.
    pub doc_timeouts: usize,
    pub removed: usize,
    pub skipped_unchanged: usize,
    pub skipped_no_lang: usize,
    pub extract_failed: usize,
    pub elapsed_ms: u128,
    pub root: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TelemetrySummaryParams {
    /// Time window for aggregation. `"today"` (default — since 00:00 local),
    /// `"1h"` (last hour), `"all"` (no window).
    #[serde(default)]
    pub window: Option<String>,
    /// Optional exact key filter, matched against the recorded `domain:mode` key
    /// (e.g. `"code:outline"`).
    #[serde(default)]
    pub tool: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct TelemetrySummaryResponse {
    pub window: String,
    pub total_calls: usize,
    pub total_resp_bytes: u64,
    pub total_est_tokens_saved: u64,
    pub per_tool: Vec<ToolCallCount>,
    pub per_baseline: Vec<BaselineCount>,
    pub recent: Vec<RecentCallView>,
    /// True when the JSONL grew past the in-memory read cap and the dashboard
    /// only inspected the tail. Aggregates are still over the inspected slice.
    pub truncated: bool,
    /// Disclosure of the estimator model — read by `/basemind-stats --explain`
    /// to remind the user that savings numbers are heuristic.
    pub savings_note: &'static str,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ToolCallCount {
    pub tool: String,
    pub calls: usize,
    pub est_tokens_saved: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct BaselineCount {
    pub baseline: String,
    pub calls: usize,
    pub est_tokens_saved: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct RecentCallView {
    pub ts_micros: i64,
    pub tool: String,
    pub resp_bytes: u64,
    /// Wall-clock microseconds the call took. Matches the `elapsed_us` the tool itself returned.
    pub elapsed_us: u64,
    pub est_tokens_saved: u64,
}

pub use crate::engine::types::git::{FindCommitsByPathParams, HotFilesParams};
