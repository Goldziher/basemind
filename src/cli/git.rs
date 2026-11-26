//! `basemind git` — the CLI half of the git verb tools.
//!
//! `log`, `blame` and `diff` run the identical bodies as the MCP tools `git_log`, `git_blame` and
//! `git_diff`, which is what `tests/cli_parity/` asserts. `status`, `recent`, `touching`,
//! `search`, `show`, `symbol-history`, `blame-symbol` and `diff-outline` are shorthands for one
//! shape of those three; `churn`, `hotspots` and `by-path` have no MCP tool.

use std::io::Write;

use anyhow::Result;
use clap::Subcommand;

use super::context::CliContext;
use crate::engine::Workspace;
use crate::engine::cursor::Cursor;
use crate::engine::types::git::{
    FindCommitsByPathParams, GitBlameParams, GitDiffParams, GitLogParams, HotFilesParams, HotspotsParams,
};

use super::render::{Emit, emit};
use super::{resolve_path, run_tool};

/// Whether `cmd` reads only the repository (commits, blobs, the working tree, the git-history index)
/// and never a view's index, so it answers the same on every view. A symbol's history picks the
/// definition from the view's outline, and a structural diff compares against the view's map.
pub(crate) fn reads_only_the_repository(cmd: &GitCmd) -> bool {
    !matches!(
        cmd,
        GitCmd::Log { symbol: Some(_), .. }
            | GitCmd::SymbolHistory { .. }
            | GitCmd::Diff { structural: true, .. }
            | GitCmd::DiffOutline { .. }
            | GitCmd::Hotspots { .. }
    )
}

#[derive(Subcommand, Debug)]
pub enum GitCmd {
    /// Commit history: recent commits, a path's history, a commit search, a symbol's history, or
    /// one commit in full.
    #[command(after_long_help = crate::cli::help_code::GIT_LOG)]
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
        /// With a search: only commits since this author time, `YYYY-MM-DD` or `<N>d`/`w`/`m`/`y` ago.
        #[arg(long)]
        since: Option<String>,
        /// With a search: only commits until this author time, same forms, day included.
        #[arg(long)]
        until: Option<String>,
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
        /// Also list the files each commit changed (recent and directory lists).
        #[arg(long)]
        files: bool,
    },
    /// Who last changed each line of a file, a 1-based line span, or one symbol.
    #[command(after_long_help = crate::cli::help_code::GIT_BLAME)]
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
    #[command(after_long_help = crate::cli::help_code::GIT_DIFF)]
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
    #[command(after_long_help = crate::cli::help_code::GIT_STATUS)]
    Status,
    /// Recent commits. Shorthand for `log` with no filter.
    #[command(after_long_help = crate::cli::help_code::GIT_RECENT)]
    Recent {
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
        /// Also list the files each commit changed.
        #[arg(long)]
        files: bool,
    },
    /// Commits touching a path. Shorthand for `log --path <path>`.
    #[command(after_long_help = crate::cli::help_code::GIT_TOUCHING)]
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
    #[command(after_long_help = crate::cli::help_code::GIT_SEARCH)]
    Search {
        query: String,
        /// Only commits whose author name or email holds every one of these terms. Matched against
        /// the author alone, never the message.
        #[arg(long)]
        author: Option<String>,
        /// With a search: only commits since this author time, `YYYY-MM-DD` or `<N>d`/`w`/`m`/`y` ago.
        #[arg(long)]
        since: Option<String>,
        /// With a search: only commits until this author time, same forms, day included.
        #[arg(long)]
        until: Option<String>,
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
    #[command(after_long_help = crate::cli::help_code::GIT_SHOW)]
    Show { rev: String },
    /// Commits where a symbol's body changed. Shorthand for `log --path <path> --symbol <name>`.
    #[command(after_long_help = crate::cli::help_code::GIT_SYMBOL_HISTORY)]
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
    #[command(after_long_help = crate::cli::help_code::GIT_BLAME_SYMBOL)]
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
    #[command(after_long_help = crate::cli::help_code::GIT_DIFF_OUTLINE)]
    DiffOutline {
        path: String,
        #[arg(long)]
        rev: Option<String>,
    },
    /// Top-K hot files by commit count over a recent window (CLI only).
    #[command(after_long_help = crate::cli::help_code::GIT_CHURN)]
    Churn {
        #[arg(long)]
        window: Option<u32>,
        #[arg(long)]
        top_k: Option<u32>,
    },
    /// Files where change and size meet: commits and authors in a window, lines, symbols, the
    /// longest function and importers, listing files with two or more signals in the top 10% of
    /// the files changed in the window. Raw numbers, no score; branch and nesting counts are not
    /// measured (CLI only).
    #[command(after_long_help = crate::cli::help_code::GIT_HOTSPOTS)]
    Hotspots {
        /// Window ending at HEAD's commit time: `<N>d`, `<N>w` or `<N>y` (default `90d`).
        #[arg(long)]
        since: Option<String>,
        /// Only files at this path or under this directory.
        #[arg(long)]
        path: Option<String>,
        /// Rows to print (default 20, at most 200).
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Commits whose changed paths match a regex (CLI only).
    #[command(after_long_help = crate::cli::help_code::GIT_BY_PATH)]
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
        since: None,
        until: None,
        symbol: None,
        line: None,
        rev: None,
        limit: None,
        cursor: None,
        max_tokens: None,
        files: false,
    }
}

/// Dispatch a `git` subcommand through the in-process server.
pub async fn run(tools: &CliContext, cmd: GitCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let (tool, result) = match cmd {
        GitCmd::Log {
            path,
            query,
            author,
            since,
            until,
            symbol,
            line,
            rev,
            limit,
            cursor,
            max_tokens,
            files,
        } => {
            let p = GitLogParams {
                path: path.map(|path| resolve_path(tools, &path)),
                query,
                author,
                since,
                until,
                symbol,
                line,
                rev,
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
                files,
            };
            (
                "git_log",
                tools
                    .call("git_log", p, async |s, p| s.op(p, Workspace::git_log).await)
                    .await,
            )
        }
        GitCmd::Recent {
            limit,
            cursor,
            max_tokens,
            files,
        } => {
            let p = GitLogParams {
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
                files,
                ..log()
            };
            (
                "git_log",
                tools
                    .call("git_log", p, async |s, p| s.op(p, Workspace::git_log).await)
                    .await,
            )
        }
        GitCmd::Touching {
            path,
            limit,
            cursor,
            max_tokens,
        } => {
            let p = GitLogParams {
                path: Some(resolve_path(tools, &path)),
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
                ..log()
            };
            (
                "git_log",
                tools
                    .call("git_log", p, async |s, p| s.op(p, Workspace::git_log).await)
                    .await,
            )
        }
        GitCmd::Search {
            query,
            author,
            since,
            until,
            limit,
            cursor,
            max_tokens,
        } => {
            let p = GitLogParams {
                query: Some(query),
                author,
                since,
                until,
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
                ..log()
            };
            (
                "git_log",
                tools
                    .call("git_log", p, async |s, p| s.op(p, Workspace::git_log).await)
                    .await,
            )
        }
        GitCmd::Show { rev } => {
            let p = GitLogParams {
                rev: Some(rev),
                ..log()
            };
            (
                "git_log",
                tools
                    .call("git_log", p, async |s, p| s.op(p, Workspace::git_log).await)
                    .await,
            )
        }
        GitCmd::SymbolHistory {
            path,
            name,
            line,
            limit,
            cursor,
        } => {
            let p = GitLogParams {
                path: Some(resolve_path(tools, &path)),
                symbol: Some(name),
                line,
                limit,
                cursor: cursor.map(Cursor),
                ..log()
            };
            (
                "git_log",
                tools
                    .call("git_log", p, async |s, p| s.op(p, Workspace::git_log).await)
                    .await,
            )
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
                path: resolve_path(tools, &path),
                line,
                end_line,
                symbol,
                rev,
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
            };
            (
                "git_blame",
                tools
                    .call("git_blame", p, async |s, p| s.op(p, Workspace::git_blame).await)
                    .await,
            )
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
                path: resolve_path(tools, &path),
                line,
                end_line: None,
                symbol: Some(name),
                rev,
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
            };
            (
                "git_blame",
                tools
                    .call("git_blame", p, async |s, p| s.op(p, Workspace::git_blame).await)
                    .await,
            )
        }
        GitCmd::Diff {
            path,
            from,
            to,
            structural,
        } => {
            let p = GitDiffParams {
                path: path.map(|path| resolve_path(tools, &path)),
                from,
                to,
                structural: structural.then_some(true),
            };
            (
                "git_diff",
                tools
                    .call("git_diff", p, async |s, p| s.op_unpaged(p, Workspace::git_diff).await)
                    .await,
            )
        }
        GitCmd::Status => {
            let p = GitDiffParams {
                path: None,
                from: None,
                to: None,
                structural: None,
            };
            (
                "git_diff",
                tools
                    .call("git_diff", p, async |s, p| s.op_unpaged(p, Workspace::git_diff).await)
                    .await,
            )
        }
        GitCmd::DiffOutline { path, rev } => {
            let p = GitDiffParams {
                path: Some(resolve_path(tools, &path)),
                from: rev,
                to: None,
                structural: Some(true),
            };
            (
                "git_diff",
                tools
                    .call("git_diff", p, async |s, p| s.op_unpaged(p, Workspace::git_diff).await)
                    .await,
            )
        }
        GitCmd::Churn { window, top_k } => (
            "git_churn",
            tools
                .call("git_churn", HotFilesParams { window, top_k }, async |s, p| {
                    s.cli_op(p, Workspace::git_churn).await
                })
                .await,
        ),
        GitCmd::Hotspots { since, path, limit } => {
            let p = HotspotsParams {
                since,
                path: path.map(|path| resolve_path(tools, &path)),
                limit,
            };
            (
                "git_hotspots",
                tools
                    .call("git_hotspots", p, async |s, p| {
                        s.cli_op(p, Workspace::git_hotspots).await
                    })
                    .await,
            )
        }
        GitCmd::ByPath {
            pattern,
            window,
            limit,
            cursor,
            max_tokens,
        } => (
            "git_by_path",
            tools
                .call(
                    "git_by_path",
                    FindCommitsByPathParams {
                        pattern,
                        window,
                        limit,
                        cursor: cursor.map(Cursor),
                        max_tokens,
                        include_files: false,
                    },
                    async |s, p| s.cli_op(p, Workspace::git_by_path).await,
                )
                .await,
        ),
    };

    let r = run_tool(tool, result)?;
    emit(tool, &r, opts, out)
}
