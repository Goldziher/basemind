//! Git-aware query subcommands: 1:1 with the MCP git tools.
//!
//! Each handler builds the matching `*Params` struct and dispatches to the
//! identical `#[tool]` method on the in-process [`BasemindServer`].

use std::io::Write;

use anyhow::Result;
use clap::Subcommand;

use crate::mcp::BasemindServer;
use crate::mcp::params::*;

use super::render::emit;
use super::run_tool;

#[derive(Subcommand, Debug)]
pub enum GitCmd {
    /// Staged / unstaged / untracked working-tree status.
    WorkingTreeStatus,
    /// Recent commits with paths + summaries.
    RecentChanges {
        #[arg(long)]
        limit: Option<u32>,
        /// Omit the per-file change list.
        #[arg(long)]
        no_files: bool,
    },
    /// Full-text search over commit history (author / message / all) at full branch depth.
    /// This is the "what did <author> do" / "which commit mentions <X>" tool — it scans every
    /// commit reachable from HEAD, not a recent window.
    Search {
        /// Query tokens (lowercased, split on non-alphanumeric) matched as an AND.
        pattern: String,
        /// Field to search: `author` (name + email), `message` (summary + body), or `all`
        /// (default).
        #[arg(long)]
        field: Option<String>,
        /// Max commits to return (default 20, max 100).
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Commits that modified a given path.
    CommitsTouching {
        path: String,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Path-filtered commit log (regex over changed paths).
    FindCommitsByPath {
        pattern: String,
        #[arg(long)]
        window: Option<u32>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Churn-ranked files in a recent commit window.
    HotFiles {
        #[arg(long)]
        window: Option<u32>,
        #[arg(long)]
        top_k: Option<u32>,
    },
    /// File content diff between two revisions.
    DiffFile {
        path: String,
        rev_old: String,
        rev_new: String,
    },
    /// Symbol-set diff between the current view and a revision.
    DiffOutline {
        path: String,
        #[arg(long)]
        rev: Option<String>,
    },
    /// Per-line blame for a file.
    BlameFile {
        path: String,
        #[arg(long)]
        line_start: Option<u32>,
        #[arg(long)]
        line_end: Option<u32>,
        #[arg(long)]
        rev: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Blame clamped to a named symbol.
    BlameSymbol {
        path: String,
        name: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        rev: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Commits where a symbol's body changed.
    SymbolHistory {
        path: String,
        name: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        hash_mode: Option<String>,
    },
}

pub async fn run(server: &BasemindServer, cmd: GitCmd, json: bool, out: &mut impl Write) -> Result<()> {
    match cmd {
        GitCmd::WorkingTreeStatus => {
            let r = run_tool(
                "working_tree_status",
                server.working_tree_status(Parameters(WorkingTreeStatusParams {})).await,
            )?;
            emit("working_tree_status", &r, json, out)
        }
        GitCmd::RecentChanges { limit, no_files } => {
            let p = RecentChangesParams {
                limit,
                include_files: !no_files,
                cursor: None,
            };
            let r = run_tool("recent_changes", server.recent_changes(Parameters(p)).await)?;
            emit("recent_changes", &r, json, out)
        }
        GitCmd::Search { pattern, field, limit } => {
            let p = SearchGitHistoryParams {
                pattern,
                field,
                limit,
                cursor: None,
            };
            let r = run_tool("search_git_history", server.search_git_history(Parameters(p)).await)?;
            emit("search_git_history", &r, json, out)
        }
        GitCmd::CommitsTouching { path, limit } => {
            let p = CommitsTouchingParams {
                path: path.as_str().into(),
                limit,
                cursor: None,
            };
            let r = run_tool("commits_touching", server.commits_touching(Parameters(p)).await)?;
            emit("commits_touching", &r, json, out)
        }
        GitCmd::FindCommitsByPath { pattern, window, limit } => {
            let p = FindCommitsByPathParams {
                pattern,
                window,
                limit,
                cursor: None,
            };
            let r = run_tool("find_commits_by_path", server.find_commits_by_path(Parameters(p)).await)?;
            emit("find_commits_by_path", &r, json, out)
        }
        GitCmd::HotFiles { window, top_k } => {
            let p = HotFilesParams { window, top_k };
            let r = run_tool("hot_files", server.hot_files(Parameters(p)).await)?;
            emit("hot_files", &r, json, out)
        }
        GitCmd::DiffFile { path, rev_old, rev_new } => {
            let p = DiffFileParams {
                rev_old,
                rev_new,
                path: path.as_str().into(),
            };
            let r = run_tool("diff_file", server.diff_file(Parameters(p)).await)?;
            emit("diff_file", &r, json, out)
        }
        GitCmd::DiffOutline { path, rev } => {
            let p = DiffOutlineParams {
                path: path.as_str().into(),
                rev,
            };
            let r = run_tool("diff_outline", server.diff_outline(Parameters(p)).await)?;
            emit("diff_outline", &r, json, out)
        }
        GitCmd::BlameFile {
            path,
            line_start,
            line_end,
            rev,
            limit,
        } => {
            let p = BlameFileParams {
                path: path.as_str().into(),
                line_start,
                line_end,
                rev,
                limit,
                cursor: None,
            };
            let r = run_tool("blame_file", server.blame_file(Parameters(p)).await)?;
            emit("blame_file", &r, json, out)
        }
        GitCmd::BlameSymbol {
            path,
            name,
            kind,
            rev,
            limit,
        } => {
            let p = BlameSymbolParams {
                path: path.as_str().into(),
                name,
                kind,
                rev,
                limit,
                cursor: None,
            };
            let r = run_tool("blame_symbol", server.blame_symbol(Parameters(p)).await)?;
            emit("blame_symbol", &r, json, out)
        }
        GitCmd::SymbolHistory {
            path,
            name,
            kind,
            limit,
            hash_mode,
        } => {
            let p = SymbolHistoryParams {
                path: path.as_str().into(),
                name,
                kind,
                limit,
                hash_mode,
                cursor: None,
            };
            let r = run_tool("symbol_history", server.symbol_history(Parameters(p)).await)?;
            emit("symbol_history", &r, json, out)
        }
    }
}
