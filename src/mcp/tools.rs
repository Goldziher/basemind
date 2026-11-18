//! The code verb tools of `BasemindServer`: `code_outline`, `code_symbols`, `code_read`,
//! `code_grep`, `code_search`, `code_refs` and `find_files` (ADR-0018).
//!
//! Each `#[tool]` is a thin shim: [`BasemindServer::answer`] rejects inexact arguments and reports
//! failures as `isError` results, and the `*_call` method beside it (also what `basemind code …`
//! runs) records telemetry around the body in `helpers_code.rs` or an engine op.
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
use super::adapter::{self, Op};
use super::cursor_bind::bound;
use super::helpers_code;
use super::strict::Strict;
use super::types_tools::{
    CodeGrepParams, CodeOutlineParams, CodeReadParams, CodeRefsParams, CodeSearchParams, CodeSymbolsParams,
    FindFilesToolParams,
};
use crate::engine::Workspace;

tokio::task_local! {
    /// The cancellation token of the MCP `code_grep` call running on this task. The sweep runs on
    /// rayon threads that aborting the tool's task does not stop, so it checks the token per file.
    static GREP_CANCEL: tokio_util::sync::CancellationToken;
}

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_core")]
impl BasemindServer {
    #[tool(
        title = "Outline a file",
        description = "Use to see what a file contains before, or instead of, reading it: every \
        symbol with its kind, signature and 1-based `line`/`end_line`, plus the file's imports. \
        Feed a span to `code_read`. Code files only. `stale` means the file changed since it was \
        indexed and is outlined as it is now. \
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
        description = "Use for \"where is X defined\" when you know (part of) the name: finds \
        functions, types, constants and other symbols by name across the repository; a row is \
        `path`, `line`, `kind`, `name`, `signature`. Ranked: the exact name first, then prefix, \
        then substring, then (with `fuzzy`) fuzzy matches; `stale`: file edited since the scan, \
        `line` is current. Definitions only: for any text use `code_grep`. \
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
        instead of the whole file. Pass `symbol` (plus `line` when several share the name), or a \
        `line`..`end_line` span. A body over 128 KiB is cut after a whole line and flagged \
        `truncated`: read on from `end_line` + 1. `stale` means the file changed since it was \
        indexed and the symbol was found by parsing it as it is now. \
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
        title = "Search code and text files",
        description = "Use instead of `rg`/`grep` to find exact text (a string, identifier or \
        comment) in indexed code and text files (markdown, config, data; not PDFs). `pattern` is \
        literal unless `regex` is true. `hits` maps each file to its hits: `line`, `text` and the \
        `symbol` it sits in; a hit in a file edited since it was indexed has `stale` and no symbol. \
        `skipped_files` counts files that could not be searched. A `path` or `language` no indexed \
        file has is an error. \
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
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        GREP_CANCEL
            .scope(ctx.ct, self.answer(args, |p| self.code_grep_call(p)))
            .await
    }

    #[tool(
        title = "Search code by meaning",
        description = "Use when you know what the code does but not a name or string in it: \
        searches code chunks by meaning and returns pointers (path, `line`/`end_line`, symbol), \
        not bodies; copies of a file share one hit (`also_at`). `lanes_unavailable` names lanes \
        that did not run. Read a hit with `code_read`. \
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
        title = "Find call sites, implementations and importers",
        description = "Use for \"what calls X\", \"what implements X\", \"what imports X\", \"which \
        definition does this bind to\" (`relation`). Callers are call sites only, not type uses or \
        `use` lines (`code_grep`). `match` `exact`: the name or a path ending in it; `substring`: \
        any name holding it. `hits` maps files to rows: `line`, `callee` if not `name`, `stale` \
        if edited since the scan. Without `path` hits match by name only; \
        `path` pins the definition there and sets `resolved` true (proven), false (not proven) or \
        absent (`resolution` says why). \
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
        its name or path, or to list files when `query` is omitted; `files` holds paths. Covers \
        code, documents and every other file git tracks; a query with one typo still finds its \
        file. A `path_prefix` or `language` no listed file has is an error. \
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
    /// Run the engine operation `op` as a verb-tool call: the body both a `#[tool]` shim and the
    /// matching `basemind` command run ([`adapter::call`]).
    pub(crate) async fn op<P, R>(
        &self,
        p: P,
        op: impl FnOnce(&Workspace, P) -> Result<R, crate::engine::EngineError> + Send + 'static,
    ) -> Result<CallToolResult, McpError>
    where
        P: super::cursor_bind::Paged + Send + 'static,
        R: serde::Serialize + 'static,
    {
        adapter::call(&self.state, p, op).await
    }

    /// [`op`](Self::op) for a verb tool that pages nothing (`git_diff`): no cursor to bind, the same
    /// telemetry row ([`adapter::call_unpaged`]).
    pub(crate) async fn op_unpaged<P, R>(&self, p: P, op: Op<P, R>) -> Result<CallToolResult, McpError>
    where
        P: super::strict::ToolArgs + serde::Serialize + Send + 'static,
        R: serde::Serialize + 'static,
    {
        adapter::call_unpaged(&self.state, p, op).await
    }

    /// Run the engine operation `op` for a CLI-only command (`git churn`, `git by-path`): no MCP
    /// tool, so no cursor binding and no telemetry row ([`adapter::run`]).
    pub(crate) async fn cli_op<P, R>(&self, p: P, op: Op<P, R>) -> Result<CallToolResult, McpError>
    where
        P: Send + 'static,
        R: serde::Serialize + 'static,
    {
        adapter::run(&self.state.shared, p, op).await
    }

    /// Body of `code_outline`, shared with `basemind code outline`.
    pub(crate) async fn code_outline_call(&self, p: CodeOutlineParams) -> Result<CallToolResult, McpError> {
        self.op(p, Workspace::code_outline).await
    }

    /// Body of the `code_symbols` tool.
    pub(crate) async fn code_symbols_call(&self, p: CodeSymbolsParams) -> Result<CallToolResult, McpError> {
        self.op(p, Workspace::code_symbols).await
    }

    /// Body of the `code_read` tool.
    pub(crate) async fn code_read_call(&self, p: CodeReadParams) -> Result<CallToolResult, McpError> {
        self.op_unpaged(p, Workspace::code_read).await
    }

    /// Body of the `code_grep` tool: [`Workspace::code_grep`], stopped when the MCP request running
    /// it is cancelled ([`GREP_CANCEL`]).
    pub(crate) async fn code_grep_call(&self, p: CodeGrepParams) -> Result<CallToolResult, McpError> {
        let cancel = GREP_CANCEL.try_with(Clone::clone).ok();
        self.op(p, move |ws: &Workspace, p| {
            ws.code_grep_until(p, &|| {
                cancel
                    .as_ref()
                    .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
            })
        })
        .await
    }

    /// Body of `code_search`, shared with `basemind code search`.
    pub(crate) async fn code_search_call(&self, p: CodeSearchParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "code_search",
            p,
            bound(p, |p| helpers_code::code_search(&self.state, p)).await
        )
    }

    /// Body of `code_refs`, shared with `basemind code refs`.
    pub(crate) async fn code_refs_call(&self, p: CodeRefsParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "code_refs",
            p,
            bound(p, |p| helpers_code::code_refs(&self.state, p)).await
        )
    }

    /// Body of the `find_files` tool.
    pub(crate) async fn find_files_call(&self, p: FindFilesToolParams) -> Result<CallToolResult, McpError> {
        self.op(p, Workspace::find_files).await
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

/// The `status` note: that a rescan is owed (see [`crate::store::Store::rescan_owed`]), else the
/// divergence note below.
///
/// The divergence note (bug #10): when the current view's index is empty
/// but content-addressed blobs are present on disk, the index was lost/wiped over live
/// blobs and a rescan would rebuild it. A legitimately unscanned repo (no blobs either)
/// gets no note.
pub(super) fn index_note(rescan_owed: bool, file_count: usize, blob_count: usize) -> Option<String> {
    if rescan_owed {
        return Some(crate::store::RESCAN_OWED.to_string());
    }
    blob_divergence_note(file_count, blob_count)
}

fn blob_divergence_note(file_count: usize, blob_count: usize) -> Option<String> {
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
    fn status_note_says_a_rescan_is_owed_before_anything_else() {
        let note = index_note(true, 0, 7).expect("an owed rescan always has a note");
        assert!(
            note.contains("basemind scan") && note.contains("index_rescan"),
            "{note}"
        );
        assert_eq!(index_note(false, 42, 100), None);
        assert!(index_note(false, 0, 7).is_some_and(|note| note.contains("7 blob file")));
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
