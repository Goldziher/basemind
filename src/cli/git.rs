//! `basemind git` — the CLI half of the `git` domain.
//!
//! Real clap subcommands rather than a `--mode` flag, so each operation keeps its own `--help` and
//! its own argument validation; they map one-to-one onto the MCP `git` tool's [`GitMode`] values,
//! which is what `tests/cli_parity.rs` asserts.

use std::io::Write;

use anyhow::Result;
use clap::Subcommand;

use crate::mcp::BasemindServer;
use crate::mcp::params::*;

use super::render::{Emit, emit};
use super::run_tool;

#[derive(Subcommand, Debug)]
pub enum GitCmd {
    /// Staged / unstaged / untracked working-tree status.
    Status,
    /// Recent commits with paths + summaries (a recency window, not a search).
    Recent {
        #[arg(long)]
        limit: Option<u32>,
        /// Omit the per-file change list.
        #[arg(long)]
        no_files: bool,
    },
    /// Full-text search over commit history (author / message / all) at full branch depth.
    /// This is the "what did <author> do" / "which commit mentions <X>" mode — it scans every
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
    Touching {
        path: String,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Path-filtered commit log (regex over changed paths).
    ByPath {
        pattern: String,
        #[arg(long)]
        window: Option<u32>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Churn-ranked files in a recent commit window.
    Churn {
        #[arg(long)]
        window: Option<u32>,
        #[arg(long)]
        top_k: Option<u32>,
    },
    /// File content diff between two revisions.
    Diff {
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
    Blame {
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

/// Dispatch a `git` subcommand through the in-process server.
pub async fn run(server: &BasemindServer, cmd: GitCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let p = match cmd {
        GitCmd::Status => GitParams::new(GitMode::Status),
        GitCmd::Recent { limit, no_files } => GitParams {
            limit,
            include_files: no_files.then_some(false),
            ..GitParams::new(GitMode::Recent)
        },
        GitCmd::Search { pattern, field, limit } => GitParams {
            pattern: Some(pattern),
            field,
            limit,
            ..GitParams::new(GitMode::Search)
        },
        GitCmd::Touching { path, limit } => GitParams {
            path: Some(path.as_str().into()),
            limit,
            ..GitParams::new(GitMode::Touching)
        },
        GitCmd::ByPath { pattern, window, limit } => GitParams {
            pattern: Some(pattern),
            window,
            limit,
            ..GitParams::new(GitMode::ByPath)
        },
        GitCmd::Churn { window, top_k } => GitParams {
            window,
            top_k,
            ..GitParams::new(GitMode::Churn)
        },
        GitCmd::Diff { path, rev_old, rev_new } => GitParams {
            path: Some(path.as_str().into()),
            rev_old: Some(rev_old),
            rev_new: Some(rev_new),
            ..GitParams::new(GitMode::Diff)
        },
        GitCmd::DiffOutline { path, rev } => GitParams {
            path: Some(path.as_str().into()),
            rev,
            ..GitParams::new(GitMode::DiffOutline)
        },
        GitCmd::Blame {
            path,
            line_start,
            line_end,
            rev,
            limit,
        } => GitParams {
            path: Some(path.as_str().into()),
            line_start,
            line_end,
            rev,
            limit,
            ..GitParams::new(GitMode::Blame)
        },
        GitCmd::BlameSymbol {
            path,
            name,
            kind,
            rev,
            limit,
        } => GitParams {
            path: Some(path.as_str().into()),
            name: Some(name),
            kind,
            rev,
            limit,
            ..GitParams::new(GitMode::BlameSymbol)
        },
        GitCmd::SymbolHistory {
            path,
            name,
            kind,
            limit,
            hash_mode,
        } => GitParams {
            path: Some(path.as_str().into()),
            name: Some(name),
            kind,
            limit,
            hash_mode,
            ..GitParams::new(GitMode::SymbolHistory)
        },
    };

    let key = p.mode.telemetry_key();
    let r = run_tool(key, server.git(Parameters(Lenient(p))).await)?;
    emit(key, &r, opts, out)
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser, Subcommand as _};

    use super::GitCmd;

    #[derive(Parser)]
    struct Harness {
        #[command(subcommand)]
        cmd: GitCmd,
    }

    /// The CLI half of the parity contract, checked from this side too: every `git` mode the MCP
    /// tool advertises must resolve to a clap subcommand of the same (kebab-cased) name.
    #[test]
    fn should_expose_one_subcommand_per_advertised_git_mode() {
        let command = GitCmd::augment_subcommands(Harness::command());
        let names: Vec<String> = command.get_subcommands().map(|s| s.get_name().to_string()).collect();
        for mode in crate::mcp::mode::GitMode::ALL_MODES {
            let expected = mode.replace('_', "-");
            assert!(
                names.contains(&expected),
                "`git` mode `{mode}` has no `basemind git {expected}` subcommand; got {names:?}"
            );
        }
    }
}
