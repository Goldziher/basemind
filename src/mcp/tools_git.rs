//! The git verb tools of `BasemindServer`: `git_log`, `git_blame` and `git_diff` (ADR-0018).
//!
//! Thin shims over the engine's git operations (`src/engine/git`); see `tools.rs` for the shim
//! contract.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;

use super::BasemindServer;
use super::strict::Strict;
use super::types_tools::{GitBlameParams, GitDiffParams, GitLogParams};
use crate::engine::Workspace;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_git")]
impl BasemindServer {
    #[tool(
        title = "Commit history",
        description = "Use instead of `git log`/`git show` for \"what changed recently\", \"what \
        touched this file or function\" or \"which commit mentions X\". `{}` lists recent commits; \
        `path`, `query`/`author`, `path`+`symbol` or `rev` narrow it. Rows: sha (8), author, \
        date, subject; `[introduced]`/`[removed]` for a symbol. Symbol and directory history is \
        read in batches: an empty page with `next_cursor` is not the end. \
        Example: {\"path\":\"src/store.rs\",\"limit\":5}",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn git_log(
        &self,
        Parameters(args): Parameters<Strict<GitLogParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.git_log_call(p)).await
    }

    #[tool(
        title = "Blame",
        description = "Use instead of `git blame` for \"who last changed this\": per-line \
        authorship of a file, a 1-based `line`..`end_line` span or one `symbol`, at `rev`. \
        `hunks`: `\"<line>[-<end>] <sha>[ <old path>]\"` (old path when moved); `commits`: each \
        sha's author, time and summary. \
        Example: {\"path\":\"src/store.rs\",\"symbol\":\"open\"}",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn git_blame(
        &self,
        Parameters(args): Parameters<Strict<GitBlameParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.git_blame_call(p)).await
    }

    #[tool(
        title = "Diff and status",
        description = "Use instead of `git status`/`git diff`. No arguments: the working-tree \
        status (staged, modified, untracked; 200 paths a list). A file `path` diffs its working \
        tree against HEAD, or `from`/`to`; `structural`: symbols added and removed and a `common` \
        count. A diff over 200 hunks or 128 KiB is cut (`truncated`). \
        Example: {\"path\":\"src/store.rs\",\"from\":\"HEAD~1\",\"to\":\"HEAD\"}",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn git_diff(
        &self,
        Parameters(args): Parameters<Strict<GitDiffParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.git_diff_call(p)).await
    }
}

impl BasemindServer {
    /// Body of the `git_log` tool.
    pub(crate) async fn git_log_call(&self, p: GitLogParams) -> Result<CallToolResult, McpError> {
        self.op(p, Workspace::git_log).await
    }

    /// Body of the `git_blame` tool.
    pub(crate) async fn git_blame_call(&self, p: GitBlameParams) -> Result<CallToolResult, McpError> {
        self.op(p, Workspace::git_blame).await
    }

    /// Body of the `git_diff` tool.
    pub(crate) async fn git_diff_call(&self, p: GitDiffParams) -> Result<CallToolResult, McpError> {
        self.op_unpaged(p, Workspace::git_diff).await
    }
}
