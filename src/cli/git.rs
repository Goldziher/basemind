//! `basemind git` — the CLI half of the git verb tools.
//!
//! `log`, `blame` and `diff` run the identical bodies as the MCP tools `git_log`, `git_blame` and
//! `git_diff`, which is what `tests/cli_parity/` asserts. `status`, `recent`, `touching`,
//! `search`, `show`, `symbol-history`, `blame-symbol` and `diff-outline` are shorthands for one
//! shape of those three; `churn` and `by-path` have no MCP tool.

use std::io::Write;

use anyhow::Result;
use clap::Subcommand;

use crate::engine::Workspace;
use crate::mcp::BasemindServer;
use crate::mcp::cursor::Cursor;
use crate::mcp::params::*;

use super::render::{Emit, emit};
use super::{resolve_path, run_tool};

#[derive(Subcommand, Debug)]
pub enum GitCmd {
    /// Commit history: recent commits, a path's history, a commit search, a symbol's history, or
    /// one commit in full.
    Log {
        /// Only commits touching this file (or directory, when it ends with `/`).
        #[arg(long)]
        path: Option<String>,
        /// Full-text search of commit messages.
        #[arg(long)]
        query: Option<String>,
        /// Only commits whose author name or email holds every one of these terms.
        #[arg(long)]
        author: Option<String>,
        /// History of this symbol's body (needs `--path`).
        #[arg(long)]
        symbol: Option<String>,
        /// With `--symbol`: a 1-based line inside the definition meant, when several share the name.
        #[arg(long)]
        line: Option<u32>,
        /// Show this one commit in full.
        #[arg(long)]
        rev: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// Who last changed each line of a file, a 1-based line span, or one symbol.
    Blame {
        path: String,
        /// First line, 1-based. With `--symbol`: a line inside the definition meant.
        #[arg(long)]
        line: Option<u32>,
        /// Last line, inclusive (default: `--line`).
        #[arg(long)]
        end_line: Option<u32>,
        /// Blame this symbol's lines instead of a span.
        #[arg(long)]
        symbol: Option<String>,
        #[arg(long)]
        rev: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// The working-tree status (no path), a file's text diff, or its symbol-level diff.
    Diff {
        path: Option<String>,
        /// Old revision (default HEAD).
        #[arg(long)]
        from: Option<String>,
        /// New revision (default: the working tree).
        #[arg(long)]
        to: Option<String>,
        /// Symbol-level diff instead of text hunks.
        #[arg(long)]
        structural: bool,
    },
    /// Working-tree status: staged / modified / untracked. Shorthand for `diff` with no arguments.
    Status,
    /// Recent commits. Shorthand for `log` with no filter.
    Recent {
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// Commits touching a path. Shorthand for `log --path <path>`.
    Touching {
        path: String,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// Full-text search over commit messages. Shorthand for `log --query <query>`.
    Search {
        query: String,
        /// Only commits whose author name or email holds every one of these terms. Matched against
        /// the author alone, never the message.
        #[arg(long)]
        author: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// One commit with its full message and changed files. Shorthand for `log --rev <rev>`.
    Show { rev: String },
    /// Commits where a symbol's body changed. Shorthand for `log --path <path> --symbol <name>`.
    SymbolHistory {
        path: String,
        name: String,
        /// A 1-based line inside the definition meant, when several share the name.
        #[arg(long)]
        line: Option<u32>,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Blame for one symbol's line span. Shorthand for `blame <path> --symbol <name>`.
    BlameSymbol {
        path: String,
        name: String,
        /// A 1-based line inside the definition meant, when several share the name.
        #[arg(long)]
        line: Option<u32>,
        #[arg(long)]
        rev: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// Symbols a file gained or lost since a revision. Shorthand for
    /// `diff <path> --structural --from <rev>`.
    DiffOutline {
        path: String,
        #[arg(long)]
        rev: Option<String>,
    },
    /// Top-K hot files by commit count over a recent window (CLI only).
    Churn {
        #[arg(long)]
        window: Option<u32>,
        #[arg(long)]
        top_k: Option<u32>,
    },
    /// Commits whose changed paths match a regex (CLI only).
    ByPath {
        pattern: String,
        #[arg(long)]
        window: Option<u32>,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
}

/// A `git_log` call with no filter set.
fn log() -> GitLogParams {
    GitLogParams {
        path: None,
        query: None,
        author: None,
        symbol: None,
        line: None,
        rev: None,
        limit: None,
        cursor: None,
        max_tokens: None,
    }
}

/// Dispatch a `git` subcommand through the in-process server.
pub async fn run(server: &BasemindServer, cmd: GitCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let (tool, result) = match cmd {
        GitCmd::Log {
            path,
            query,
            author,
            symbol,
            line,
            rev,
            limit,
            cursor,
            max_tokens,
        } => {
            let p = GitLogParams {
                path: path.map(|path| resolve_path(server, &path)),
                query,
                author,
                symbol,
                line,
                rev,
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
            };
            ("git_log", server.op(p, Workspace::git_log).await)
        }
        GitCmd::Recent {
            limit,
            cursor,
            max_tokens,
        } => {
            let p = GitLogParams {
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
                ..log()
            };
            ("git_log", server.op(p, Workspace::git_log).await)
        }
        GitCmd::Touching {
            path,
            limit,
            cursor,
            max_tokens,
        } => {
            let p = GitLogParams {
                path: Some(resolve_path(server, &path)),
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
                ..log()
            };
            ("git_log", server.op(p, Workspace::git_log).await)
        }
        GitCmd::Search {
            query,
            author,
            limit,
            cursor,
            max_tokens,
        } => {
            let p = GitLogParams {
                query: Some(query),
                author,
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
                ..log()
            };
            ("git_log", server.op(p, Workspace::git_log).await)
        }
        GitCmd::Show { rev } => {
            let p = GitLogParams {
                rev: Some(rev),
                ..log()
            };
            ("git_log", server.op(p, Workspace::git_log).await)
        }
        GitCmd::SymbolHistory {
            path,
            name,
            line,
            limit,
            cursor,
        } => {
            let p = GitLogParams {
                path: Some(resolve_path(server, &path)),
                symbol: Some(name),
                line,
                limit,
                cursor: cursor.map(Cursor),
                ..log()
            };
            ("git_log", server.op(p, Workspace::git_log).await)
        }
        GitCmd::Blame {
            path,
            line,
            end_line,
            symbol,
            rev,
            limit,
            cursor,
            max_tokens,
        } => {
            let p = GitBlameParams {
                path: resolve_path(server, &path),
                line,
                end_line,
                symbol,
                rev,
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
            };
            ("git_blame", server.op(p, Workspace::git_blame).await)
        }
        GitCmd::BlameSymbol {
            path,
            name,
            line,
            rev,
            limit,
            cursor,
            max_tokens,
        } => {
            let p = GitBlameParams {
                path: resolve_path(server, &path),
                line,
                end_line: None,
                symbol: Some(name),
                rev,
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
            };
            ("git_blame", server.op(p, Workspace::git_blame).await)
        }
        GitCmd::Diff {
            path,
            from,
            to,
            structural,
        } => {
            let p = GitDiffParams {
                path: path.map(|path| resolve_path(server, &path)),
                from,
                to,
                structural: structural.then_some(true),
            };
            ("git_diff", server.op_unpaged(p, Workspace::git_diff).await)
        }
        GitCmd::Status => {
            let p = GitDiffParams {
                path: None,
                from: None,
                to: None,
                structural: None,
            };
            ("git_diff", server.op_unpaged(p, Workspace::git_diff).await)
        }
        GitCmd::DiffOutline { path, rev } => {
            let p = GitDiffParams {
                path: Some(resolve_path(server, &path)),
                from: rev,
                to: None,
                structural: Some(true),
            };
            ("git_diff", server.op_unpaged(p, Workspace::git_diff).await)
        }
        GitCmd::Churn { window, top_k } => (
            "git_churn",
            server
                .cli_op(HotFilesParams { window, top_k }, Workspace::git_churn)
                .await,
        ),
        GitCmd::ByPath {
            pattern,
            window,
            limit,
            cursor,
            max_tokens,
        } => (
            "git_by_path",
            server
                .cli_op(
                    FindCommitsByPathParams {
                        pattern,
                        window,
                        limit,
                        cursor: cursor.map(Cursor),
                        max_tokens,
                    },
                    Workspace::git_by_path,
                )
                .await,
        ),
    };

    let r = run_tool(tool, result)?;
    emit(tool, &r, opts, out)
}
