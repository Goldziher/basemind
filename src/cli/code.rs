//! `basemind code` — the CLI half of the `code` domain.
//!
//! Real clap subcommands rather than a `--mode` flag, so each lookup keeps its own `--help` and its
//! own argument validation; they map one-to-one onto the MCP `code` tool's [`CodeMode`] values,
//! which is what `tests/cli_parity.rs` asserts.
//!
//! Each handler leaves every field its mode does not use `None`: the helper rejects a field
//! belonging to another mode, so populating them blindly would fail the call.

use std::io::Write;

use anyhow::Result;
use clap::Subcommand;

use crate::mcp::BasemindServer;
use crate::mcp::params::*;
use crate::path::{RelPath, normalize_query_path};

use super::render::{Emit, emit};
use super::run_tool;

/// Resolve a user-supplied CLI path into the repo-relative `RelPath` key the
/// index is keyed by (scanner-produced: no leading `./`, never absolute).
///
/// `code outline /abs/repo/src/foo.rs` and `code outline ./src/foo.rs` both
/// resolve to `src/foo.rs`. Paths that escape or fall outside the repository
/// can't match an indexed file, so we fall back to the raw input and let the
/// downstream tool report "file not indexed" rather than silently mangling it.
fn resolve_path(server: &BasemindServer, path: &str) -> RelPath {
    match normalize_query_path(path, &server.state.shared.root) {
        Some(rel) => RelPath::from(rel),
        None => RelPath::from(path),
    }
}

#[derive(Subcommand, Debug)]
pub enum CodeCmd {
    /// File structure: symbols + imports, optionally calls + docs (L2). Read this instead of the
    /// file.
    Outline {
        /// Repository-relative path.
        path: String,
        /// Also include calls + doc comments (L2).
        #[arg(long)]
        l2: bool,
    },
    /// Find a definition by name across every indexed file (case-sensitive substring).
    Symbols {
        name: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Regex content search across indexed files.
    Grep {
        pattern: String,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        path_contains: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        /// Suppress the 1-line before/after context for each match.
        #[arg(long = "no-context")]
        no_context: bool,
    },
    /// List indexed files, optionally filtered.
    Files {
        #[arg(long)]
        path_contains: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Locate a file by a fuzzy fragment of its name or path (fzf/fd-style), ranked by score.
    Find {
        query: String,
        #[arg(long)]
        path_prefix: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Resolve the reference at a position to the definition it binds to (scope-resolved).
    Definition {
        /// Repository-relative path of the file holding the reference.
        path: String,
        /// 1-based line of the reference identifier.
        line: u32,
        /// 0-based byte column of the reference within the line (default 0).
        #[arg(long, default_value_t = 0)]
        column: u32,
    },
    /// Every call site of a name (name-only — no scope resolution).
    References {
        name: String,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Callers of one specific definition (path + name + optional kind).
    Callers {
        path: String,
        name: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Types implementing / extending / inheriting from a trait, interface, or base class.
    Implementations {
        trait_name: String,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Files whose imports mention the given module (heuristic).
    Dependents { module: String },
    /// One symbol's raw source body — the inverse of an outline entry.
    Expand {
        /// Repository-relative path of the indexed file.
        path: String,
        /// Symbol name (matched exactly, case-sensitive).
        name: String,
        /// Kind filter to disambiguate (e.g. `function`, `struct`, `method`).
        #[arg(long)]
        kind: Option<String>,
    },
    /// Search code by meaning. Returns pointers; fetch bodies with `chunk`. Needs
    /// `--features code-search`.
    Semantic {
        query: String,
        #[arg(long)]
        limit: Option<u32>,
        /// Retrieval lane: `hybrid` (default), `semantic`, or `keyword`.
        #[arg(long)]
        lane: Option<String>,
        /// Run the cross-encoder rerank pass over the fused hits (first call downloads a model).
        #[arg(long)]
        rerank: bool,
        /// Reranker preset name (default `bge-reranker-base`).
        #[arg(long)]
        rerank_preset: Option<String>,
        #[arg(long)]
        format: Option<String>,
    },
    /// Fetch one code chunk's source body by path. Needs `--features code-search`.
    Chunk {
        /// Repository-relative path of the source file.
        path: String,
        #[arg(long)]
        chunk_id: Option<String>,
        #[arg(long)]
        byte_start: Option<u32>,
    },
}

pub async fn run(server: &BasemindServer, cmd: CodeCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let p = match cmd {
        CodeCmd::Outline { path, l2 } => CodeParams {
            path: Some(resolve_path(server, &path)),
            l2: Some(l2),
            ..CodeParams::new(CodeMode::Outline)
        },
        CodeCmd::Symbols { name, kind, limit } => CodeParams {
            name: Some(name),
            kind,
            limit,
            ..CodeParams::new(CodeMode::Symbols)
        },
        CodeCmd::Grep {
            pattern,
            language,
            path_contains,
            limit,
            no_context,
        } => CodeParams {
            pattern: Some(pattern),
            language,
            path_contains,
            limit,
            include_context: Some(!no_context),
            ..CodeParams::new(CodeMode::Grep)
        },
        CodeCmd::Files {
            path_contains,
            language,
            limit,
        } => CodeParams {
            path_contains,
            language,
            limit,
            ..CodeParams::new(CodeMode::Files)
        },
        CodeCmd::Find {
            query,
            path_prefix,
            language,
            limit,
        } => CodeParams {
            query: Some(query),
            path_prefix,
            language,
            limit,
            ..CodeParams::new(CodeMode::Find)
        },
        CodeCmd::Definition { path, line, column } => CodeParams {
            path: Some(resolve_path(server, &path)),
            line: Some(line),
            column: Some(column),
            ..CodeParams::new(CodeMode::Definition)
        },
        CodeCmd::References { name, limit } => CodeParams {
            name: Some(name),
            limit,
            ..CodeParams::new(CodeMode::References)
        },
        CodeCmd::Callers {
            path,
            name,
            kind,
            limit,
        } => CodeParams {
            path: Some(resolve_path(server, &path)),
            name: Some(name),
            kind,
            limit,
            ..CodeParams::new(CodeMode::Callers)
        },
        CodeCmd::Implementations {
            trait_name,
            language,
            limit,
        } => CodeParams {
            trait_name: Some(trait_name),
            language,
            limit,
            ..CodeParams::new(CodeMode::Implementations)
        },
        CodeCmd::Dependents { module } => CodeParams {
            module: Some(module),
            ..CodeParams::new(CodeMode::Dependents)
        },
        CodeCmd::Expand { path, name, kind } => CodeParams {
            path: Some(resolve_path(server, &path)),
            name: Some(name),
            kind,
            ..CodeParams::new(CodeMode::Expand)
        },
        CodeCmd::Semantic {
            query,
            limit,
            lane,
            rerank,
            rerank_preset,
            format,
        } => CodeParams {
            query: Some(query),
            limit,
            lane,
            rerank: rerank.then_some(true),
            rerank_preset,
            format,
            ..CodeParams::new(CodeMode::Semantic)
        },
        CodeCmd::Chunk {
            path,
            chunk_id,
            byte_start,
        } => CodeParams {
            path: Some(resolve_path(server, &path)),
            chunk_id,
            byte_start,
            ..CodeParams::new(CodeMode::Chunk)
        },
    };

    let key = p.mode.telemetry_key();
    let r = run_tool(key, server.code(Parameters(Lenient(p))).await)?;
    emit(key, &r, opts, out)
}
