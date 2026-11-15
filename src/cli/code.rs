//! `basemind code` — the CLI half of the code verb tools.
//!
//! Every MCP code tool has a command here that runs the identical body (`outline` → `code_outline`,
//! `symbols` → `code_symbols`, `read` → `code_read`, `grep` → `code_grep`, `search` →
//! `code_search`, `refs` → `code_refs`, `find` → `find_files`), which is what `tests/cli_parity/`
//! asserts. `callers`, `implementations`, `dependents`, `definition` and `files` are shorthands for
//! one shape of `refs` / `find`; `chunk` has no MCP tool.

use std::io::Write;

use anyhow::Result;
use clap::Subcommand;

use crate::mcp::BasemindServer;
use crate::mcp::cursor::Cursor;
use crate::mcp::params::*;

use super::choices::{RefsMatch, RefsRelation, SearchLane};
use super::render::{Emit, emit};
use super::{resolve_path, run_tool};

#[derive(Subcommand, Debug)]
pub enum CodeCmd {
    /// File structure: symbols with 1-based line spans, plus imports. Read this instead of the
    /// file.
    Outline {
        /// Repository-relative path.
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
    /// Find a definition by name across every indexed file.
    Symbols {
        name: String,
        #[arg(long)]
        kind: Option<String>,
        /// Also match the name as a fuzzy subsequence, ranked last.
        #[arg(long)]
        fuzzy: bool,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// A symbol's source body (`--symbol`), or a 1-based inclusive line span (`--line`,
    /// `--end-line`).
    #[command(alias = "expand")]
    Read {
        /// Repository-relative path.
        path: String,
        /// Symbol whose body to print (exact name).
        #[arg(long)]
        symbol: Option<String>,
        /// Kind filter to disambiguate `--symbol`.
        #[arg(long)]
        kind: Option<String>,
        /// First line, 1-based.
        #[arg(long)]
        line: Option<u32>,
        /// Last line, inclusive (default: `--line`).
        #[arg(long)]
        end_line: Option<u32>,
    },
    /// Content search across indexed code files. The pattern is literal text unless `--regex`.
    Grep {
        pattern: String,
        /// Treat the pattern as a Rust regex.
        #[arg(long)]
        regex: bool,
        /// Substring of the repository-relative path (not a glob).
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        language: Option<String>,
        /// Add one line before and after each match.
        #[arg(long)]
        context: bool,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// Search code by meaning; returns pointers, read one with `code read`.
    #[command(alias = "semantic")]
    Search {
        query: String,
        /// Retrieval lane (default hybrid).
        #[arg(long, value_enum)]
        lane: Option<SearchLane>,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// Usages of a name: call sites (default), implementations, importers, or the definition a
    /// reference binds to.
    #[command(alias = "references")]
    Refs {
        name: String,
        /// What to list (default callers).
        #[arg(long, value_enum)]
        relation: Option<RefsRelation>,
        /// File defining the name (pins one definition); for `definition`, the file holding the
        /// reference.
        #[arg(long)]
        path: Option<String>,
        /// `definition` only: 1-based line of the reference.
        #[arg(long)]
        line: Option<u32>,
        /// `definition` only: 1-based column (default: where the name occurs on the line).
        #[arg(long)]
        column: Option<u32>,
        /// Name matching (default exact).
        #[arg(long = "match", value_enum)]
        match_mode: Option<RefsMatch>,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// Callers of the definition of `name` in `path`. Shorthand for `refs <name> --path <path>`.
    Callers {
        path: String,
        name: String,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// Types implementing a trait / interface / base class. Shorthand for
    /// `refs <name> --relation implementations`.
    Implementations {
        name: String,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// Files importing a module. Shorthand for `refs <module> --relation importers`.
    Dependents {
        module: String,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// The definition the reference `name` at `path`:`line` binds to. Shorthand for
    /// `refs <name> --relation definition --path <path> --line <line>`.
    Definition {
        /// Repository-relative path of the file holding the reference.
        path: String,
        /// 1-based line of the reference.
        line: u32,
        /// The referenced identifier.
        name: String,
        /// 1-based column (default: where the name occurs on the line).
        #[arg(long)]
        column: Option<u32>,
    },
    /// Locate a file by a fuzzy fragment of its name or path, or list indexed files when the query
    /// is omitted. Covers code and documents.
    Find {
        query: Option<String>,
        #[arg(long)]
        path_prefix: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// List indexed files. Shorthand for `find` with no query.
    Files {
        #[arg(long)]
        path_prefix: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// Fetch one semantic-search chunk body by path (CLI only).
    Chunk {
        /// Repository-relative path of the source file the chunk belongs to.
        path: String,
        /// The content-addressed chunk id from a search hit.
        #[arg(long)]
        chunk_id: Option<String>,
        /// The chunk's start byte offset, an alternative to `--chunk-id`.
        #[arg(long)]
        byte_start: Option<u32>,
    },
}

/// Dispatch a `code` subcommand through the in-process server.
pub async fn run(server: &BasemindServer, cmd: CodeCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let (tool, result) = match cmd {
        CodeCmd::Outline {
            path,
            limit,
            cursor,
            max_tokens,
        } => (
            "code_outline",
            server
                .code_outline_call(CodeOutlineParams {
                    path: resolve_path(server, &path),
                    limit,
                    cursor: cursor.map(Cursor),
                    max_tokens,
                })
                .await,
        ),
        CodeCmd::Symbols {
            name,
            kind,
            fuzzy,
            limit,
            cursor,
            max_tokens,
        } => (
            "code_symbols",
            server
                .code_symbols_call(CodeSymbolsParams {
                    name,
                    kind,
                    fuzzy: fuzzy.then_some(true),
                    limit,
                    cursor: cursor.map(Cursor),
                    max_tokens,
                })
                .await,
        ),
        CodeCmd::Read {
            path,
            symbol,
            kind,
            line,
            end_line,
        } => (
            "code_read",
            server
                .code_read_call(CodeReadParams {
                    path: resolve_path(server, &path),
                    symbol,
                    kind,
                    line,
                    end_line,
                })
                .await,
        ),
        CodeCmd::Grep {
            pattern,
            regex,
            path,
            language,
            context,
            limit,
            cursor,
            max_tokens,
        } => (
            "code_grep",
            server
                .code_grep_call(CodeGrepParams {
                    pattern,
                    regex: regex.then_some(true),
                    path,
                    language,
                    context: context.then_some(true),
                    limit,
                    cursor: cursor.map(Cursor),
                    max_tokens,
                })
                .await,
        ),
        CodeCmd::Search {
            query,
            lane,
            limit,
            cursor,
            max_tokens,
        } => (
            "code_search",
            server
                .code_search_call(CodeSearchParams {
                    query,
                    lane: lane.map(|lane| lane.as_str().to_string()),
                    limit,
                    cursor: cursor.map(Cursor),
                    max_tokens,
                })
                .await,
        ),
        CodeCmd::Refs {
            name,
            relation,
            path,
            line,
            column,
            match_mode,
            limit,
            cursor,
            max_tokens,
        } => {
            let refs = CodeRefsParams {
                name,
                relation: relation.map(|relation| relation.as_str().to_string()),
                path: path.map(|path| resolve_path(server, &path)),
                line,
                column,
                match_mode: match_mode.map(|matching| matching.as_str().to_string()),
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
            };
            ("code_refs", server.code_refs_call(refs).await)
        }
        CodeCmd::Callers {
            path,
            name,
            limit,
            cursor,
            max_tokens,
        } => {
            let refs = CodeRefsParams {
                path: Some(resolve_path(server, &path)),
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
                ..refs(name, RefsRelation::Callers)
            };
            ("code_refs", server.code_refs_call(refs).await)
        }
        CodeCmd::Implementations {
            name,
            limit,
            cursor,
            max_tokens,
        } => {
            let refs = CodeRefsParams {
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
                ..refs(name, RefsRelation::Implementations)
            };
            ("code_refs", server.code_refs_call(refs).await)
        }
        CodeCmd::Dependents { module, limit, cursor } => {
            let refs = CodeRefsParams {
                limit,
                cursor: cursor.map(Cursor),
                ..refs(module, RefsRelation::Importers)
            };
            ("code_refs", server.code_refs_call(refs).await)
        }
        CodeCmd::Definition {
            path,
            line,
            name,
            column,
        } => {
            let refs = CodeRefsParams {
                path: Some(resolve_path(server, &path)),
                line: Some(line),
                column,
                ..refs(name, RefsRelation::Definition)
            };
            ("code_refs", server.code_refs_call(refs).await)
        }
        CodeCmd::Find {
            query,
            path_prefix,
            language,
            limit,
            cursor,
            max_tokens,
        } => (
            "find_files",
            server
                .find_files_call(FindFilesToolParams {
                    query,
                    path_prefix,
                    language,
                    limit,
                    cursor: cursor.map(Cursor),
                    max_tokens,
                })
                .await,
        ),
        CodeCmd::Files {
            path_prefix,
            language,
            limit,
            cursor,
            max_tokens,
        } => (
            "find_files",
            server
                .find_files_call(FindFilesToolParams {
                    query: None,
                    path_prefix,
                    language,
                    limit,
                    cursor: cursor.map(Cursor),
                    max_tokens,
                })
                .await,
        ),
        CodeCmd::Chunk {
            path,
            chunk_id,
            byte_start,
        } => (
            "code_chunk",
            server
                .code_chunk_call(GetChunkParams {
                    path: resolve_path(server, &path),
                    chunk_id,
                    byte_start,
                })
                .await,
        ),
    };

    let r = run_tool(tool, result)?;
    emit(tool, &r, opts, out)
}

/// A `code_refs` call for `name` with only its relation set.
fn refs(name: String, relation: RefsRelation) -> CodeRefsParams {
    CodeRefsParams {
        name,
        relation: Some(relation.as_str().to_string()),
        path: None,
        line: None,
        column: None,
        match_mode: None,
        limit: None,
        cursor: None,
        max_tokens: None,
    }
}
