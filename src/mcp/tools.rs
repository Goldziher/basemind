//! The code verb tools of `BasemindServer`: `code_outline`, `code_symbols`, `code_read`,
//! `code_grep`, `code_search`, `code_refs` and `find_files` (ADR-0018).
//!
//! Each `#[tool]` is a thin shim: [`BasemindServer::answer`] rejects inexact arguments and reports
//! failures as `isError` results, and the `*_call` method beside it (also what `basemind code …`
//! runs) records telemetry around the body in `helpers_code.rs` / `helpers_expand.rs`.
//!
//! No tool declares an `output_schema`: the response structs nest, schemars emits nested structs
//! as `$ref` into `$defs` (the construct that dropped the whole registry in GH #50), and inlining
//! them would put `tools/list` over its byte ceiling.
//!
//! The pagination / cap helpers the code-map bodies share stay at the bottom of the file.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;

use super::BasemindServer;
use super::helpers::{LIST_LIMIT_DEFAULT, LIST_LIMIT_MAX};
use super::strict::Strict;
use super::types_tools::{
    CodeGrepParams, CodeOutlineParams, CodeReadParams, CodeRefsParams, CodeSearchParams, CodeSymbolsParams,
    FindFilesToolParams,
};
use super::{helpers_code, helpers_expand};

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_core")]
impl BasemindServer {
    #[tool(
        title = "Outline a file",
        description = "Use to see what a file contains before, or instead of, reading it: every \
        symbol with its kind, signature and 1-based `line`/`end_line`, plus the file's imports. \
        Feed a span to `code_read`. Code files only. \
        Example: {\"path\":\"src/store.rs\"}",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn code_outline(
        &self,
        Parameters(args): Parameters<Strict<CodeOutlineParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.code_outline_call(p)).await
    }

    #[tool(
        title = "Find a definition",
        description = "Use for \"where is X defined\": finds functions, types, constants and other \
        symbols by name across the repository and returns path, line and signature. Ranked: the \
        exact name first, then prefix, then substring, then (with `fuzzy`) fuzzy matches. \
        Example: {\"name\":\"parse_config\",\"kind\":\"function\"}",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn code_symbols(
        &self,
        Parameters(args): Parameters<Strict<CodeSymbolsParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.code_symbols_call(p)).await
    }

    #[tool(
        title = "Read a symbol or line span",
        description = "Use to read the source of one function or type, or an exact line span, \
        instead of the whole file. Pass `symbol`, or `line` with `end_line`; not both. Bodies over \
        128 KiB are cut and flagged `truncated`. \
        Example: {\"path\":\"src/store.rs\",\"symbol\":\"open\"}",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn code_read(
        &self,
        Parameters(args): Parameters<Strict<CodeReadParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.code_read_call(p)).await
    }

    #[tool(
        title = "Search code text",
        description = "Use instead of `rg`/`grep` to find a string, identifier or comment in the \
        indexed code. `pattern` is literal text unless `regex` is true. Each hit is the full line \
        plus the symbol it sits in. Code files only: search documents with `docs_search`. \
        Example: {\"pattern\":\"cache_root(\",\"path\":\"src/\"}",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn code_grep(
        &self,
        Parameters(args): Parameters<Strict<CodeGrepParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.code_grep_call(p)).await
    }

    #[tool(
        title = "Search code by meaning",
        description = "Use when you know what the code does but not what it is called: searches \
        code chunks by meaning and returns pointers (path, `line`/`end_line`, symbol), not bodies. \
        `lanes_unavailable` says when a lane could not run. Read a hit with `code_read`. \
        Example: {\"query\":\"retry a failed upload with backoff\"}",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn code_search(
        &self,
        Parameters(args): Parameters<Strict<CodeSearchParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.code_search_call(p)).await
    }

    #[tool(
        title = "Find usages",
        description = "Use for \"what calls X\", \"what implements X\", \"what imports X\" and \
        \"which definition does this reference bind to\". `relation` picks the question; the name \
        is matched exactly; `path` pins the definition in that file and marks each hit `resolved`. \
        Example: {\"name\":\"open_store\",\"path\":\"src/store.rs\"}",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn code_refs(
        &self,
        Parameters(args): Parameters<Strict<CodeRefsParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.code_refs_call(p)).await
    }

    #[tool(
        title = "Find files",
        description = "Use instead of `find`/`fd`/`ls -R` to locate a file by a fuzzy fragment of \
        its name or path, or to list indexed files when `query` is omitted. Covers code and \
        documents (markdown, config, PDFs). \
        Example: {\"query\":\"readme\"}",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn find_files(
        &self,
        Parameters(args): Parameters<Strict<FindFilesToolParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.find_files_call(p)).await
    }
}

impl BasemindServer {
    /// Body of `code_outline`, shared with `basemind code outline`.
    pub(crate) async fn code_outline_call(&self, p: CodeOutlineParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "code_outline",
            p,
            helpers_code::code_outline(&self.state, p).await
        )
    }

    /// Body of `code_symbols`, shared with `basemind code symbols`.
    pub(crate) async fn code_symbols_call(&self, p: CodeSymbolsParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "code_symbols",
            p,
            helpers_code::code_symbols(&self.state, p).await
        )
    }

    /// Body of `code_read`, shared with `basemind code read`.
    pub(crate) async fn code_read_call(&self, p: CodeReadParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "code_read",
            p,
            helpers_expand::code_read(&self.state, p).await
        )
    }

    /// Body of `code_grep`, shared with `basemind code grep`.
    pub(crate) async fn code_grep_call(&self, p: CodeGrepParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "code_grep",
            p,
            helpers_code::code_grep(&self.state, p).await
        )
    }

    /// Body of `code_search`, shared with `basemind code search`.
    pub(crate) async fn code_search_call(&self, p: CodeSearchParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "code_search",
            p,
            helpers_code::code_search(&self.state, p).await
        )
    }

    /// Body of `code_refs`, shared with `basemind code refs`.
    pub(crate) async fn code_refs_call(&self, p: CodeRefsParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "code_refs",
            p,
            helpers_code::code_refs(&self.state, p).await
        )
    }

    /// Body of `find_files`, shared with `basemind code find`.
    pub(crate) async fn find_files_call(&self, p: FindFilesToolParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "find_files",
            p,
            helpers_code::find_files(&self.state, p).await
        )
    }

    /// Fetch one semantic-search chunk body. No MCP tool (`code_read` reads a hit's span):
    /// `basemind code chunk`.
    pub(crate) async fn code_chunk_call(
        &self,
        p: super::types_code::GetChunkParams,
    ) -> Result<CallToolResult, McpError> {
        #[cfg(feature = "code-search")]
        {
            super::helpers_code_search::run_get_chunk(&self.state, p).await
        }
        #[cfg(not(feature = "code-search"))]
        {
            let _ = p;
            Err(super::tool_contract::feature_missing("code chunk", "code-search"))
        }
    }
}

/// Resolve the effective `list_files` page limit and report whether the caller's
/// requested limit was clamped to [`LIST_LIMIT_MAX`].
///
/// Returns `(effective_limit, clamped)` where `clamped` is true iff the caller asked
/// for more than the cap allows — surfaced honestly to the client rather than silently
/// truncating (bug #17).
pub(super) fn effective_list_limit(requested: Option<u32>) -> (usize, bool) {
    let asked = requested.unwrap_or(LIST_LIMIT_DEFAULT);
    let clamped = asked > LIST_LIMIT_MAX;
    (asked.min(LIST_LIMIT_MAX) as usize, clamped)
}

/// Count content-addressed blobs in the GLOBAL blob store by tallying `.fm.msgpack` files
/// (one combined L1 + L2 filemap per indexed content hash; the `.doc` sibling shares the same
/// stem so it is not double-counted). A single directory read — cheaper than
/// [`crate::store_gc::cache_stats`], which also unions every view index — so it is safe to call
/// from the `status` path.
///
/// Since the blob store went machine-global this counts blobs across EVERY workspace, so the
/// `status` divergence heuristic (empty index yet blobs on disk) can over-report on a shared
/// machine — advisory only, as before.
///
/// Returns `0` when the blobs directory is absent or unreadable; the count is advisory.
pub(super) fn count_fm_blobs() -> usize {
    crate::store::count_fm_blobs(&crate::store::global_blobs_dir())
}

/// Build the `status` divergence note (bug #10): when the current view's index is empty
/// but content-addressed blobs are present on disk, the index was lost/wiped over live
/// blobs and a rescan would rebuild it. A legitimately unscanned repo (no blobs either)
/// gets no note.
pub(super) fn blob_divergence_note(file_count: usize, blob_count: usize) -> Option<String> {
    if file_count == 0 && blob_count > 0 {
        Some(format!(
            "index for this view is empty but {blob_count} blob file(s) exist on disk; \
             the view index was lost or wiped — run `basemind scan` to rebuild it"
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::helpers::{LIST_LIMIT_DEFAULT, LIST_LIMIT_MAX};

    #[test]
    fn list_limit_under_cap_is_not_clamped() {
        let (limit, clamped) = effective_list_limit(Some(100));
        assert_eq!(limit, 100);
        assert!(!clamped, "a request under the cap must not be flagged clamped");
    }

    #[test]
    fn list_limit_at_cap_is_not_clamped() {
        let (limit, clamped) = effective_list_limit(Some(LIST_LIMIT_MAX));
        assert_eq!(limit, LIST_LIMIT_MAX as usize);
        assert!(!clamped, "a request exactly at the cap is honored, not clamped");
    }

    #[test]
    fn list_limit_over_cap_is_clamped_and_signalled() {
        let (limit, clamped) = effective_list_limit(Some(LIST_LIMIT_MAX + 1));
        assert_eq!(limit, LIST_LIMIT_MAX as usize, "limit is clamped to the cap");
        assert!(clamped, "exceeding the cap must set the clamp flag (bug #17)");
    }

    #[test]
    fn list_limit_default_when_absent() {
        let (limit, clamped) = effective_list_limit(None);
        assert_eq!(limit, LIST_LIMIT_DEFAULT as usize);
        assert!(!clamped);
    }

    #[test]
    fn status_note_absent_when_index_and_blobs_agree() {
        assert_eq!(blob_divergence_note(42, 100), None, "populated index: no note");
    }

    #[test]
    fn status_note_absent_for_unscanned_empty_repo() {
        assert_eq!(
            blob_divergence_note(0, 0),
            None,
            "empty index with no blobs is a legitimately unscanned repo, not a lost index"
        );
    }

    #[test]
    fn status_note_present_when_index_empty_but_blobs_exist() {
        let note = blob_divergence_note(0, 7);
        assert!(
            note.is_some(),
            "lost-index-over-live-blobs must surface a note (bug #10)"
        );
        let note = note.unwrap();
        assert!(note.contains("7 blob file"), "note reports the blob count");
        assert!(note.contains("scan"), "note suggests a rescan");
    }
}
