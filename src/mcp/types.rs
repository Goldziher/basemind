//! Parameter shapes (deserialized from MCP tool-call arguments) and JSON response shapes
//! (serialized into tool-call results). Kept separate from `tools.rs` so the impl block
//! itself stays readable and within the per-file size budget.

use std::collections::BTreeMap;

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::cursor::Cursor;
use crate::path::RelPath;

/// Lifecycle label attached to a read-tool response when the server is not fully [`Ready`]. Lets an
/// agent distinguish "index still loading — retry" from a genuine empty result, so a query issued during
/// startup warmup or a from-scratch index build is never misread as "no matches".
///
/// [`Ready`]: super::Lifecycle::Ready
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub(crate) struct LifecycleNotice {
    /// Machine-readable state tag: `"warming_up"`, `"building_index"`, `"rescanning"`, or `"index_rebuilding"`.
    pub state: &'static str,
    /// Human/agent-readable explanation of what the server is doing and what it means for this result.
    pub message: &'static str,
    /// `true` when the caller should retry shortly for complete results (warming / building), `false`
    /// when the current result is usable but may be a moment stale (rescanning).
    pub retry: bool,
}

impl LifecycleNotice {
    /// Build the notice for `state`, or `None` when [`Ready`](super::Lifecycle::Ready) (the common path,
    /// so no notice field is serialized on a healthy response).
    pub(crate) fn for_state(state: super::Lifecycle) -> Option<Self> {
        use super::Lifecycle;
        match state {
            Lifecycle::Ready => None,
            Lifecycle::WarmingUp => Some(Self {
                state: "warming_up",
                message: "Index is warming up (loading the code map into memory). Results may be \
                          incomplete — retry in a moment for the full set.",
                retry: true,
            }),
            Lifecycle::BuildingIndex => Some(Self {
                state: "building_index",
                message: "Index is building for the first time. Results are incomplete — poll `status` \
                          until `indexing` is false, then retry.",
                retry: true,
            }),
            Lifecycle::Rescanning => Some(Self {
                state: "rescanning",
                message: "An incremental rescan is in progress after a file change. Results are usable \
                          but may be a moment stale.",
                retry: false,
            }),
        }
    }

    /// The notice for a read-only session whose projected call / implementation indexes hit their
    /// byte cap. Such a session has no Fjall index to query, so those projections ARE the answer
    /// source; once capped, reference-shaped results are truncated rather than merely stale.
    /// `retry: false` because rerunning changes nothing — the fix is a larger
    /// `[resources] max_map_cache_mb`, or a session that owns the index.
    pub(crate) fn projections_capped() -> Self {
        Self {
            state: "projections_capped",
            message: "No daemon was reachable, so this session answered from in-memory reference \
                      projections that hit the `[resources] max_map_cache_mb` budget; reference / \
                      implementation results are truncated. Start the daemon or raise the budget \
                      for complete results.",
            retry: false,
        }
    }

    /// [`index_rebuilding`](Self::index_rebuilding) when `status` records a degraded scan, else `None`
    /// (healthy, or no sidecar yet: the never-scanned case belongs to the building-index lifecycle).
    pub(crate) fn for_scan_status(status: Option<&crate::store::StatusSidecar>) -> Option<Self> {
        status
            .is_some_and(crate::store::StatusSidecar::is_degraded)
            .then(Self::index_rebuilding)
    }

    /// The notice attached when nothing has been scanned: every list is empty for that reason, not
    /// because the query matched nothing.
    pub(crate) fn empty_index() -> Self {
        Self {
            state: "empty_index",
            message: "The index is empty: nothing has been scanned for this workspace, so this result \
                      says nothing about the code. Run `index_rescan {\"full\":true}` (or `basemind scan` \
                      in a shell) and retry.",
            retry: true,
        }
    }

    /// The notice for a workspace whose last scan is partial or whose resolve pass has not
    /// committed, notably an index wiped by a schema revision bump and rebuilding. Reference-shaped
    /// reads (`references` / `callers` / `implementations`) answer empty or short until it finishes.
    pub(crate) fn index_rebuilding() -> Self {
        Self {
            state: "index_rebuilding",
            message: "index rebuilding — results incomplete. A full scan is rebuilding the symbol / \
                      reference index; an empty `references` / `callers` / `implementations` answer \
                      does not mean there are none. Poll `status` until `degraded` clears, then retry.",
            retry: true,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchSymbolsParams {
    /// Substring matched against symbol name (case-sensitive).
    pub needle: String,
    /// Optional kind filter: function, method, struct, enum, class, interface,
    /// trait, type, const, module, macro.
    #[serde(default)]
    pub kind: Option<String>,
    /// Also match names the needle is a fuzzy subsequence of, ranked below every literal match.
    #[serde(default)]
    pub fuzzy: bool,
    /// Cap the number of results returned. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned result list (not the whole envelope).
    /// Items are kept best-first until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Wire format for the response: `"json"` (default) or `"toon"`. TOON is a compact
    /// tabular encoding of the `results` list — far fewer tokens than JSON for large hit sets.
    #[serde(default)]
    pub format: Option<String>,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the in-RAM index snapshot and invalidate on rescan.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListFilesParams {
    /// Optional substring matched against the path. Cheaper than reading a glob crate.
    #[serde(default)]
    pub path_contains: Option<String>,
    /// Optional prefix the path must start with.
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Filter by language (e.g. "rust", "python").
    #[serde(default)]
    pub language: Option<String>,
    /// Cap. Default 200, max 5000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned file list (not the whole envelope).
    /// Entries are kept in order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Wire format for the response: `"json"` (default) or `"toon"`. TOON is a compact
    /// tabular encoding of the `files` list — far fewer tokens than JSON for large listings.
    #[serde(default)]
    pub format: Option<String>,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the in-RAM index snapshot and invalidate on rescan.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindFilesParams {
    /// Fuzzy query matched as a subsequence against each indexed path (fzf/fd-style — letters
    /// of `query` must appear in order in the path, not necessarily contiguous). Case-insensitive.
    pub query: String,
    /// Optional prefix filter applied before scoring (e.g. "src/mcp/").
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Filter by language (e.g. "rust", "python"), applied before scoring.
    #[serde(default)]
    pub language: Option<String>,
    /// Cap. Default 200, max 5000 (same convention as `list_files`).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned file list (not the whole envelope).
    /// Entries are kept in score order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Wire format for the response: `"json"` (default) or `"toon"`. TOON is a compact
    /// tabular encoding of the `files` list — far fewer tokens than JSON for large listings.
    #[serde(default)]
    pub format: Option<String>,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the in-RAM index snapshot (and the score ordering computed for `query`); a rescan
    /// invalidates them same as `list_files`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindReferencesParams {
    /// The callee identifier to look up. Substring match — case-sensitive, no scope
    /// resolution; both `Foo::bar()` and `bar()` register as callee `"bar"`. Use with
    /// caution on common names like `new` or `get`.
    pub name: String,
    /// Match callee identifiers containing `name` instead of only those equal to it.
    #[serde(default)]
    pub substring: bool,
    /// Cap on results returned. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned `hits` list (not the whole envelope).
    /// Hits are kept in scan order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Wire format for the response: `"json"` (default) or `"toon"`. TOON is a compact
    /// tabular encoding of the `hits` list — far fewer tokens than JSON for large hit sets.
    #[serde(default)]
    pub format: Option<String>,
    /// Resume token returned by the previous call's `next_cursor`. Stable across rescans
    /// because the underlying Fjall keys are content-addressed.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindCallersParams {
    /// Repository-relative path of the definition file.
    pub path: RelPath,
    /// Name of the definition.
    pub name: String,
    /// Match callee identifiers containing `name` instead of only those equal to it.
    #[serde(default)]
    pub substring: bool,
    /// Optional kind filter for resolving the definition (function/method/class/...).
    #[serde(default)]
    pub kind: Option<String>,
    /// 1-based line inside the definition meant, when several in the file share the name.
    #[serde(default)]
    pub line: Option<u32>,
    /// Cap on results returned. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned `hits` list (not the whole envelope).
    /// Hits are kept in scan order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Stable across rescans
    /// because the underlying Fjall keys are content-addressed.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GotoDefinitionParams {
    /// Repository-relative path of the file holding the reference.
    pub path: RelPath,
    /// 1-based line of the reference identifier.
    pub line: u32,
    /// 0-based byte column of the reference within the line. Any byte inside the identifier
    /// resolves for span-aware engines (oxc JS/TS); the tree-sitter `locals` fallback matches
    /// only the identifier's start byte. Defaults to 0 (line start).
    #[serde(default)]
    pub column: u32,
}

pub(super) fn default_true() -> bool {
    true
}

/// Response of `code_outline`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct OutlineResponse {
    pub path: RelPath,
    pub language: String,
    pub size_bytes: u64,
    /// The parser reported syntax errors; the outline may be missing symbols.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub had_errors: bool,
    /// Symbols in the file, across every page.
    pub total: usize,
    /// True when the token budget dropped trailing symbols; page them with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// The working-tree file no longer matches the indexed content (or is gone), so line spans may
    /// be off until `index_rescan`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    pub symbols: Vec<OutlineSymbol>,
    /// Imported modules, on the first page only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub imports_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    #[serde(default)]
    pub elapsed_us: u64,
}

/// One symbol of a `code_outline` page. `line` and `end_line` are 1-based and inclusive, the span
/// `code_read` and `git_blame` take.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct OutlineSymbol {
    pub name: String,
    pub kind: &'static str,
    pub line: u32,
    pub end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct SearchHitView {
    pub path: RelPath,
    pub name: String,
    pub kind: &'static str,
    /// 1-based line of the definition.
    pub line: u32,
    /// 1-based column of the definition.
    pub column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// How the name matched: `exact`, `prefix`, `substring` or `fuzzy`. Rows are ranked in that
    /// order.
    #[serde(rename = "match")]
    pub matched: &'static str,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct SearchResponse {
    /// Matching symbols in the corpus, across every page.
    pub total: usize,
    /// True when a fuzzy sweep stopped at its candidate cap, so `total` is a lower bound.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub total_is_partial: bool,
    pub truncated: bool,
    /// True when a `max_tokens` budget dropped trailing results. The kept prefix is
    /// best-first; page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub results: Vec<SearchHitView>,
    /// Opaque cursor to pass back on the next call when more results are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different in-RAM snapshot
    /// (a rescan happened between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / store
    /// lookup + response construction), excluding MCP transport, argument deserialization, and
    /// response serialization. A first call against a cold server also includes index warm-up;
    /// such responses carry a `notice`. See [`crate::mcp::helpers::timing`] for the full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ListFilesEntry {
    pub path: RelPath,
    pub language: String,
    pub size_bytes: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ListFilesResponse {
    pub total: usize,
    pub returned: usize,
    pub truncated: bool,
    /// True when the caller's requested `limit` exceeded the hard cap (`LIST_LIMIT_MAX`) and
    /// was clamped down to it. Surfaced so callers know the page size was reduced (bug #17).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub limit_clamped: bool,
    /// True when a `max_tokens` budget dropped trailing files. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub files: Vec<ListFilesEntry>,
    /// Opaque cursor to pass back on the next call when more results are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different in-RAM snapshot
    /// (a rescan happened between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / store
    /// lookup + response construction), excluding MCP transport, argument deserialization, and
    /// response serialization. A first call against a cold server also includes index warm-up;
    /// such responses carry a `notice`. See [`crate::mcp::helpers::timing`] for the full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct FindFilesEntry {
    pub path: RelPath,
    pub language: String,
    pub size_bytes: u64,
    /// Fuzzy match score from `nucleo-matcher` (higher is a better match). Not comparable
    /// across queries — only meaningful to rank entries within one response.
    pub score: u32,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct FindFilesResponse {
    pub total: usize,
    pub returned: usize,
    pub truncated: bool,
    /// True when the caller's requested `limit` exceeded the hard cap (`LIST_LIMIT_MAX`) and
    /// was clamped down to it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub limit_clamped: bool,
    /// True when a `max_tokens` budget dropped trailing files. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// Matches sorted by descending `score` (ties broken by path, ascending).
    pub files: Vec<FindFilesEntry>,
    /// Opaque cursor to pass back on the next call when more results are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different in-RAM snapshot
    /// (a rescan happened between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / store
    /// lookup + response construction), excluding MCP transport, argument deserialization, and
    /// response serialization. A first call against a cold server also includes index warm-up;
    /// such responses carry a `notice`. See [`crate::mcp::helpers::timing`] for the full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct DependentsResponse {
    pub module: String,
    /// Importing files, across every page.
    pub total: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub paths: Vec<RelPath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    #[serde(default)]
    pub elapsed_us: u64,
}

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

#[derive(Debug, schemars::JsonSchema)]
pub(crate) struct ReferenceHit {
    pub path: RelPath,
    /// 1-based line of the call site; `0` when the index holds no position for it.
    pub line: u32,
    /// Byte column of the call site: 0-based here, written 1-based.
    pub column: u32,
    pub callee: String,
    /// With a pinned definition: whether scope / import resolution proved this site binds to it.
    /// Absent when no definition was pinned or the language has no resolution.
    pub resolved: Option<bool>,
}

/// Written by hand so a call site with no recorded position omits `line` and `column` together. A
/// derived form would emit `"line": 0`, which is not a line, beside a `column` of 1.
impl Serialize for ReferenceHit {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("path", &self.path)?;
        if self.line != 0 {
            map.serialize_entry("line", &self.line)?;
            map.serialize_entry("column", &self.column.saturating_add(1))?;
        }
        map.serialize_entry("callee", &self.callee)?;
        if let Some(resolved) = self.resolved {
            map.serialize_entry("resolved", &resolved)?;
        }
        map.end()
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct FindReferencesResponse {
    pub name: String,
    /// How `name` was matched against callee identifiers: `exact` or `substring`.
    #[serde(rename = "match")]
    pub matching: &'static str,
    pub total: u32,
    /// True when `total` was capped at `limit` and more matches exist on disk.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub total_is_partial: bool,
    /// True when a `max_tokens` budget dropped trailing `hits`. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub hits: Vec<ReferenceHit>,
    /// Opaque cursor to pass back on the next call when more results are available.
    /// Stable across rescans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / store
    /// lookup + response construction), excluding MCP transport, argument deserialization, and
    /// response serialization. A first call against a cold server also includes index warm-up;
    /// such responses carry a `notice`. See [`crate::mcp::helpers::timing`] for the full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct FindCallersResponse {
    /// The definition the callers were pinned to.
    pub definition: DefinitionView,
    /// How `name` was matched against callee identifiers: `exact` or `substring`.
    #[serde(rename = "match")]
    pub matching: &'static str,
    /// Why no hit is marked `resolved`, when scope / import resolution could not run for the
    /// definition's language: `unsupported for <language>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// Hits proven to bind to `definition`. Absent, not 0, when resolution did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_total: Option<u32>,
    pub total: u32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub total_is_partial: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub hits: Vec<ReferenceHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct DefinitionView {
    pub path: RelPath,
    pub name: String,
    pub kind: &'static str,
    /// 1-based line of the definition.
    pub line: u32,
    /// 1-based column of the definition.
    pub column: u32,
}

/// A resolved definition site returned by `goto_definition`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct DefinitionLocation {
    pub path: RelPath,
    /// 1-based.
    pub line: u32,
    /// 0-based byte column.
    pub column: u32,
    /// The definition identifier text, when the engine recorded its span (empty otherwise).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct GotoDefinitionResponse {
    /// Echo of the queried file.
    pub path: RelPath,
    /// Normalized 1-based line / 0-based byte column of the queried position.
    pub line: u32,
    pub column: u32,
    /// The resolved definition, or absent when the position holds no resolved binding
    /// (module-global, unresolved name, or a language without resolution coverage).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<DefinitionLocation>,
    /// Server-side handler latency in microseconds — the tool body's own execution (scope
    /// resolution + index lookup + response construction), excluding MCP transport, argument
    /// deserialization, and response serialization. A first call against a cold server also
    /// includes index warm-up. See [`crate::mcp::helpers::timing`] for the full contract.
    /// Why there is no `definition`, when the file's language has no scope resolution:
    /// `unsupported for <language>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WorkspaceGrepParams {
    /// Rust regex syntax (`regex` crate). Required.
    pub pattern: String,
    /// Optional language filter (e.g. `"rust"`, `"typescript"`). Same ID convention as
    /// `list_files`.
    #[serde(default)]
    pub language: Option<String>,
    /// Optional substring filter on path. Same convention as `list_files`.
    #[serde(default)]
    pub path_contains: Option<String>,
    /// Max number of HITS returned. Default 100, max 1000. It does not bound the files scanned:
    /// grep always sweeps the whole indexed corpus (after the `language` / `path_contains`
    /// filters), so a rare token is found wherever it lives.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned `hits` list (not the whole envelope).
    /// Hits are kept in scan order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Wire format for the response: `"json"` (default) or `"toon"`. TOON is a compact
    /// tabular encoding of the `hits` list — far fewer tokens than JSON for large hit sets.
    #[serde(default)]
    pub format: Option<String>,
    /// Include 1 line of context before + after each hit. Default true.
    #[serde(default = "default_true")]
    pub include_context: bool,
    /// Match `pattern` as literal text instead of compiling it as a regex.
    #[serde(default)]
    pub literal: bool,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the in-RAM index snapshot and invalidate on rescan.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

/// `skip_serializing_if` for a count that is reported only when something was left out.
pub(super) fn is_zero(count: &usize) -> bool {
    *count == 0
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct GrepHit {
    pub path: RelPath,
    /// 1-based line number of the match.
    pub line: u32,
    /// 1-based byte column of the match within the line.
    pub column: u32,
    /// The matching line, so the hit can be read without opening the file. A line over 400 bytes is
    /// cut to a window around the match, `…` marking each cut side.
    pub text: String,
    /// The innermost symbol whose span holds the match; absent outside every symbol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<GrepSymbol>,
    /// The line immediately before the match, when context was requested and line > 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_before: Option<String>,
    /// The line immediately after the match, when context was requested and line < EOF.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_after: Option<String>,
    /// Byte offset of the match in the file; used to find `symbol`, not part of the response.
    #[serde(skip)]
    pub start_byte: u32,
    /// Size of the file the hit was read from; `symbol` is set only when the index agrees.
    #[serde(skip)]
    pub source_fresh: bool,
}

/// The symbol a grep hit sits in.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct GrepSymbol {
    pub name: String,
    pub kind: &'static str,
}

/// Why a `workspace_grep` result was cut short. Present only when `truncated` is true — a grep that
/// returned every match carries no reason, so an agent can tell a complete zero from a bounded one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum GrepTruncation {
    /// More matches exist than `limit` allowed. Raise `limit` or page with `next_cursor`;
    /// `total_matches` is the exact count that was available.
    Limit,
    /// The corpus was larger than one call may read. Narrow with `path_contains` / `language`, or
    /// page with `next_cursor`. Only reachable on workspaces of multiple gigabytes of source.
    ByteBudget,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct WorkspaceGrepResponse {
    /// Echoed pattern from the request.
    pub pattern: String,
    /// Whether `pattern` was compiled as a regex; false means it was matched as literal text.
    pub regex: bool,
    /// Number of files that had at least one match. Exact: every candidate file is scanned.
    pub total_files_matched: usize,
    /// Exact hit count across the whole scanned corpus — not just the returned page. Exceeds
    /// `hits.len()` when `limit` truncated the result, which is what makes `truncated` actionable.
    pub total_matches: u32,
    /// True when matches exist that this response does not carry. Never true merely because the
    /// corpus is large: grep scans every candidate file.
    pub truncated: bool,
    /// Which bound cut the result, when `truncated` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation_reason: Option<GrepTruncation>,
    /// True when a `max_tokens` budget dropped trailing `hits`. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub hits: Vec<GrepHit>,
    /// Opaque cursor to pass back on the next call when more results are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different in-RAM snapshot
    /// (a rescan happened between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / store
    /// lookup + response construction), excluding MCP transport, argument deserialization, and
    /// response serialization. A first call against a cold server also includes index warm-up;
    /// such responses carry a `notice`. See [`crate::mcp::helpers::timing`] for the full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RescanParams {
    /// Optional list of repo-relative paths to scope the rescan. When omitted
    /// the full repo is walked. Paths are forward-slash with no leading `/`.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// Force a complete working-tree re-index even when `paths` is supplied (full wins).
    /// Use when the index is stale or reports "no indexed files" and a scoped rescan won't
    /// rebuild it.
    #[serde(default)]
    pub full: bool,
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

#[cfg(feature = "documents")]
pub use super::types_documents::SearchDocumentsParams;
#[cfg(feature = "documents")]
pub(super) use super::types_documents::{DocumentSearchHit, SearchDocumentsResponse};
pub use super::types_git::{
    BlameFileParams, BlameSymbolParams, CommitsTouchingParams, DiffFileParams, DiffOutlineParams,
    FindCommitsByPathParams, HotFilesParams, RecentChangesParams, SearchGitHistoryParams, SymbolHistoryParams,
};
pub(super) use super::types_git::{
    BlameHunkView, BlameResponse, BlameSymbolResponse, CommitFileView, CommitView, CommitsTouchingResponse,
    DiffFileResponse, DiffOutlineResponse, DiffSymbolView, FindCommitsByPathResponse, GitCommitHit, GitShowResponse,
    HotFileEntry, HotFilesResponse, HunkView, RecentChangesResponse, SearchGitHistoryResponse, SymbolHistoryEntry,
    SymbolHistoryResponse, WorkingTreeStatusView,
};
