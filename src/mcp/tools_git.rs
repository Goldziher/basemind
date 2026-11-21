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
        touched this file\", \"which commit mentions X\" or \"what changed this function\". `{}` \
        lists recent commits; `path`, `query`/`author`, `path`+`symbol` or `rev` narrow it. A row \
        is `\"<sha> <summary>\"`, `[introduced]`/`[removed]` after a symbol's sha; recent and \
        directory lists add `files`: sha to path to change. `rev` shows one commit whole, with \
        author and time. A symbol's or directory's history pages by commit window: follow \
        `next_cursor`, even from an empty page (`span_limit`). Example: {\"path\":\"src/store.rs\",\"limit\":5}",
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
        description = "Use instead of `git blame` for \"who last changed this\": per-line authorship \
        of a file, a 1-based `line`..`end_line` span, or one `symbol` (at `rev`). \
        `hunks`: `\"<line>[-<end>] <sha>[ <old path>]\"` (the rest of the row, when moved); \
        `commits`: each sha's author, time, summary. \
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
        description = "Use instead of `git status`/`git diff`. No arguments returns the working-tree \
        status (staged, modified, untracked; 200 paths per list); a file `path` alone diffs its \
        working tree against HEAD, or `from`/`to`; `structural` lists the symbols added and \
        removed, and counts the `common` ones, instead of text hunks. A diff over 200 hunks or 128 KiB is cut (`truncated`). \
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
