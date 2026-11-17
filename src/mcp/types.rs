//! Parameter shapes (deserialized from MCP tool-call arguments) and JSON response shapes
//! (serialized into tool-call results). Kept separate from `tools.rs` so the impl block
//! itself stays readable and within the per-file size budget.

use std::collections::BTreeMap;

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::cursor::Cursor;
use crate::path::RelPath;

pub(crate) use crate::engine::types::LifecycleNotice;

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
    /// only the identifier's start byte. `None` takes the first whole-identifier occurrence of
    /// `name` on the line.
    #[serde(default)]
    pub column: Option<u32>,
    /// The referenced name, looked for on the line when `column` is absent.
    #[serde(default)]
    pub name: String,
}

pub(super) use crate::engine::types::default_true;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct DependentsResponse {
    /// Importing files, across every page.
    pub total: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub paths: Vec<RelPath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
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

#[derive(Debug, schemars::JsonSchema)]
pub(crate) struct ReferenceHit {
    /// The file of the call site: the key the hit is listed under.
    pub path: RelPath,
    /// 1-based line of the call site; `0` when the index holds no position for it.
    pub line: u32,
    /// Byte column of the call site: 0-based here, written 1-based, and only when
    /// [`shared_line`](Self::shared_line) is set.
    pub column: u32,
    /// The name called. Written only when it is not the name asked for (a `substring` match, or an
    /// alias the definition is called by); [`ReferenceHit::named`] clears it otherwise.
    pub callee: String,
    /// With a pinned definition: `true` when scope / import resolution proved this site binds to
    /// it, `false` when resolution checked the site and did not. Absent when nothing was checked:
    /// no definition was pinned, resolution has no facts about the definition (`resolution` on the
    /// response says why), or the site's file lies past the probe cap.
    pub resolved: Option<bool>,
    /// Another call site of the same callee is on this line of this file, in the whole result and
    /// not only this page, so the line alone does not tell them apart and `column` is written. Set
    /// by [`super::helpers_calls_scan::mark_shared_lines`].
    #[schemars(skip)]
    pub shared_line: bool,
    /// The file changed since it was indexed, so `line` is where the call was when it was scanned.
    pub stale: bool,
}

impl ReferenceHit {
    /// This hit as listed for a query of `name`: the callee is left out when it is `name`.
    pub(super) fn named(mut self, name: &str) -> Self {
        if self.callee == name {
            self.callee.clear();
        }
        self
    }
}

/// Written by hand so a call site with no recorded position omits `line` instead of reporting line
/// 0, and so a callee that is the name asked for is not repeated on every row.
impl super::by_path::Located for ReferenceHit {
    fn located(&self) -> &RelPath {
        &self.path
    }

    fn writes_column(&self) -> Option<bool> {
        Some(self.shared_line)
    }

    fn line(&self) -> u32 {
        self.line
    }

    fn serialize_row<S: serde::Serializer>(&self, serializer: S, column: bool) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        if self.line != 0 {
            map.serialize_entry("line", &self.line)?;
            if column {
                map.serialize_entry("column", &self.column.saturating_add(1))?;
            }
        }
        if !self.callee.is_empty() {
            map.serialize_entry("callee", &self.callee)?;
        }
        if let Some(resolved) = self.resolved {
            map.serialize_entry("resolved", &resolved)?;
        }
        if self.stale {
            map.serialize_entry("stale", &true)?;
        }
        map.end()
    }
}

/// A call site on its own, as the token budget costs it.
impl Serialize for ReferenceHit {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        super::by_path::Located::serialize_row(self, serializer, false)
    }
}

/// The `resolution` of a `code_refs` name scan: hits are matched by name and nothing binds them to
/// one definition.
pub(super) const NAME_ONLY: &str = "name only: pass `path` to check each hit against that file's definition";

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct FindReferencesResponse {
    /// Always [`NAME_ONLY`]: no hit was checked against a definition.
    pub resolution: &'static str,
    /// Matches across every page. A lower bound when `truncated`; an upper bound when a daemon that
    /// predates exact matching answered, whose substring hits are dropped page by page.
    pub total: u32,
    /// True when the scan stopped at its cap before the end of the matches: `total` is then a
    /// lower bound, and `next_cursor` resumes the scan.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `scan_cap` whenever `truncated` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// True when a `max_tokens` budget dropped trailing `hits`. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// The call sites under the file each is in.
    #[serde(serialize_with = "super::by_path::serialize")]
    pub hits: Vec<ReferenceHit>,
    /// Opaque cursor to pass back on the next call when more results are available.
    /// Stable across rescans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct FindCallersResponse {
    /// The definition the callers were pinned to.
    pub definition: DefinitionView,
    /// Why no hit carries `resolved`: `unsupported for <language>`, `no resolution facts for this
    /// file`, `no resolution facts for this definition` (the file has facts and none names it), or
    /// that the index's resolve pass has not finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// Hits proven to bind to `definition`. Absent, not 0, when resolution did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_total: Option<u32>,
    /// Matches across every page. A lower bound when `truncated`; an upper bound when a daemon that
    /// predates exact matching answered, whose substring hits are dropped page by page.
    pub total: u32,
    /// As on a `code_refs` name scan: the scan stopped at its cap, so `total` is a lower bound.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// The call sites under the file each is in.
    #[serde(serialize_with = "super::by_path::serialize")]
    pub hits: Vec<ReferenceHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
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
    /// Why there is no `definition`, when the file's language has no scope resolution:
    /// `unsupported for <language>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
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

pub(super) use crate::engine::types::is_zero;

/// One `code_grep` hit, listed under its file ([`super::by_path`] writes it).
#[derive(Debug, schemars::JsonSchema)]
pub(super) struct GrepHit {
    /// The file the hit is in: the key the hit is listed under, not a field of its own.
    pub path: RelPath,
    /// 1-based line number of the match.
    pub line: u32,
    /// 1-based byte column of the match, written only when [`write_column`](Self::write_column)
    /// is set.
    pub column: u32,
    /// The file holds another match on this line, or `text` is a clipped window of the line: the
    /// line alone does not place the match, so `column` is written. Decided over every match in
    /// the file, so a page boundary does not change it.
    #[schemars(skip)]
    pub write_column: bool,
    /// The matching line, so the hit can be read without opening the file. A line over 400 bytes is
    /// cut to a window around the match, `…` marking each cut side.
    pub text: String,
    /// Name of the innermost symbol whose span holds the match; absent outside every symbol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// The line immediately before the match, when context was requested and line > 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_before: Option<String>,
    /// The line immediately after the match, when context was requested and line < EOF.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_after: Option<String>,
    /// Byte offset of the match in the file; used to find `symbol`, not part of the response.
    pub start_byte: u32,
    /// The file changed since it was indexed. The hit is from the file as it is now, and `symbol`
    /// is left out: the indexed outline describes other bytes until `index_rescan`.
    pub stale: bool,
}

impl super::by_path::Located for GrepHit {
    fn located(&self) -> &RelPath {
        &self.path
    }

    fn writes_column(&self) -> Option<bool> {
        Some(self.write_column)
    }

    fn line(&self) -> u32 {
        self.line
    }

    fn serialize_row<S: serde::Serializer>(&self, serializer: S, column: bool) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("line", &self.line)?;
        if column {
            map.serialize_entry("column", &self.column)?;
        }
        map.serialize_entry("text", &self.text)?;
        if let Some(symbol) = &self.symbol {
            map.serialize_entry("symbol", symbol)?;
        }
        if let Some(before) = &self.context_before {
            map.serialize_entry("context_before", before)?;
        }
        if let Some(after) = &self.context_after {
            map.serialize_entry("context_after", after)?;
        }
        if self.stale {
            map.serialize_entry("stale", &true)?;
        }
        map.end()
    }
}

/// A hit on its own, as the token budget costs it.
impl Serialize for GrepHit {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        super::by_path::Located::serialize_row(self, serializer, false)
    }
}

pub(super) use crate::engine::types::SCAN_CAP;

/// Why a `workspace_grep` result was cut short. Present only when `truncated` is true — a grep that
/// returned every match carries no reason, so an agent can tell a complete zero from a bounded one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum GrepTruncation {
    /// The corpus was larger than one call may read. Narrow with `path_contains` / `language`, or
    /// page with `next_cursor`. Only reachable on workspaces of multiple gigabytes of source.
    ByteBudget,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct WorkspaceGrepResponse {
    /// Exact hit count across the whole scanned corpus — not just the returned page. Exceeds the
    /// hits listed when `limit` or the token budget cut the page; `next_cursor` continues.
    pub total: u32,
    /// True when files were left unread, so `total` does not cover them. Never true because
    /// `limit` cut the page or because the corpus is large: grep scans every candidate file it may
    /// read. Absent when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `byte_budget` whenever `truncated` is true. There is no `omitted`: the matches in the
    /// unread files were never counted. `next_cursor` continues with those files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<GrepTruncation>,
    /// True when a `max_tokens` budget dropped trailing hits. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// The hits under the file each is in, files in the order searched.
    #[serde(serialize_with = "super::by_path::serialize")]
    pub hits: Vec<GrepHit>,
    /// Opaque cursor to pass back on the next call when more results are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// Files this call opened and could not search: unreadable, or not UTF-8 text.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skipped_files: usize,
    /// True when the caller passed a `cursor` minted against a different in-RAM snapshot
    /// (a rescan happened between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
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
