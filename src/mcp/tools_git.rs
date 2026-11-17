//! The git verb tools of `BasemindServer`: `git_log`, `git_blame` and `git_diff` (ADR-0018).
//!
//! Thin shims over `helpers_git.rs` / `helpers_git_file.rs` / `helpers_git_by_path.rs`; see
//! `tools.rs` for the shim contract.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;

use super::BasemindServer;
use super::cursor_bind::bound;
use super::index_route::blocking;
use super::strict::Strict;
use super::types::{FindCommitsByPathParams, HotFilesParams};
use super::types_tools::{GitBlameParams, GitDiffParams, GitLogParams};
use super::{helpers_git, helpers_git_by_path};

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_git")]
impl BasemindServer {
    #[tool(
        title = "Commit history",
        description = "Use instead of `git log`/`git show` for \"what changed recently\", \"what \
        touched this file\", \"which commit mentions X\" or \"when did this function change\". No \
        arguments lists recent commits; `path`, `query`/`author`, `path`+`symbol` or `rev` narrow \
        it. A row is a 12-digit `sha`, `summary`, `author` and time; `rev` shows one commit in \
        full. A symbol's or a directory's history is read one window of commits per page: follow \
        `next_cursor`, even from an empty page (`truncated_reason` `window`). \
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
        description = "Use instead of `git blame` for \"who last changed this\": per-line authorship \
        of a file, of a 1-based `line`..`end_line` span, or of one `symbol`, located in the file \
        as it is at `rev`. \
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
        status (staged, modified, untracked; 200 paths per list, a `path` ending in `/` narrows it \
        to that directory); a file `path` alone diffs its working tree against HEAD; `from`/`to` \
        pick revisions; `structural` lists the symbols added and removed, and counts the `common` \
        ones, instead of text hunks. A diff over 200 hunks or 128 KiB is cut and flagged \
        `truncated`. \
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
    /// Body of `git_log`, shared with `basemind git log`.
    pub(crate) async fn git_log_call(&self, p: GitLogParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "git_log",
            p,
            bound(p, |p| async { blocking(|| helpers_git::git_log(&self.state, p)) }).await
        )
    }

    /// Body of `git_blame`, shared with `basemind git blame`.
    pub(crate) async fn git_blame_call(&self, p: GitBlameParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "git_blame",
            p,
            bound(p, |p| async { blocking(|| helpers_git::git_blame(&self.state, p)) }).await
        )
    }

    /// Body of `git_diff`, shared with `basemind git diff`.
    pub(crate) async fn git_diff_call(&self, p: GitDiffParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(&self.state, "git_diff", p, helpers_git::git_diff(&self.state, p).await)
    }

    /// Churn-ranked files. No MCP tool: `basemind git churn`.
    pub(crate) fn git_churn_call(&self, p: HotFilesParams) -> Result<CallToolResult, McpError> {
        helpers_git::run_churn(&self.state, p)
    }

    /// Commits whose changed paths match a regex. No MCP tool of its own (`git_log` with a
    /// directory `path` covers the common case): `basemind git by-path`.
    pub(crate) fn git_by_path_call(&self, p: FindCommitsByPathParams) -> Result<CallToolResult, McpError> {
        helpers_git_by_path::run_by_path(&self.state, p)
    }
}
