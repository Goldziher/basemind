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

use super::context::CliContext;
use crate::engine::Workspace;
use crate::engine::cursor::Cursor;
use crate::engine::types::code::{CodeOutlineParams, CodeReadParams, CodeSymbolsParams};
use crate::engine::types::files::FindFilesToolParams;
use crate::engine::types::grep::CodeGrepParams;
use crate::engine::types::refs::CodeRefsParams;
use crate::engine::types::search::{CodeSearchParams, GetChunkParams};

use super::analysis::{self, AnalysisCmd};
use super::choices::{IndexedChoice, RefsMatch, RefsRelation, SearchLane};
use super::render::{Emit, emit};
use super::{resolve_path, run_tool};

#[derive(Subcommand, Debug)]
pub enum CodeCmd {
    /// File structure: symbols with 1-based line spans, plus imports. Read this instead of the
    /// file.
    #[command(after_long_help = crate::cli::help_code::CODE_OUTLINE)]
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
    #[command(after_long_help = crate::cli::help_code::CODE_SYMBOLS)]
    Symbols {
        name: String,
        #[arg(long)]
        kind: Option<String>,
        /// Repository-relative prefix, or a glob when it holds `*` (`src/`, `src/**/*.rs`).
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        language: Option<String>,
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
        /// Only files changed since this revision's merge base with `HEAD` (`HEAD`: the uncommitted
        /// changes), in `A..B` or in `A...B`; append `+importers` to add the files importing them.
        #[arg(long)]
        changed: Option<String>,
    },
    /// A symbol's source body (`--symbol`), or a 1-based inclusive line span (`--line`,
    /// `--end-line`).
    #[command(alias = "expand")]
    #[command(after_long_help = crate::cli::help_code::CODE_READ)]
    Read {
        /// Repository-relative path.
        path: String,
        /// Symbol whose body to print (exact name).
        #[arg(long)]
        symbol: Option<String>,
        /// Kind filter to disambiguate `--symbol`.
        #[arg(long)]
        kind: Option<String>,
        /// First line, 1-based. With `--symbol`: a line inside the definition meant.
        #[arg(long)]
        line: Option<u32>,
        /// Last line, inclusive (default: `--line`).
        #[arg(long)]
        end_line: Option<u32>,
    },
    /// Exact text search across indexed code, markdown, config and data files. The pattern is
    /// literal text unless `--regex`.
    #[command(after_long_help = crate::cli::help_code::CODE_GREP)]
    Grep {
        pattern: String,
        /// Treat the pattern as a Rust regex.
        #[arg(long)]
        regex: bool,
        /// Repository-relative prefix, or a glob when it holds `*` (`src/`, `src/**/*.rs`).
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        language: Option<String>,
        /// Lines to show before and after each match (0-10).
        #[arg(long, value_name = "N")]
        context: Option<u32>,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
        /// Only files changed since this revision's merge base with `HEAD` (`HEAD`: the uncommitted
        /// changes), in `A..B` or in `A...B`; append `+importers` to add the files importing them.
        #[arg(long)]
        changed: Option<String>,
    },
    /// Search code by meaning; returns pointers, read one with `code read`.
    #[command(alias = "semantic")]
    #[command(after_long_help = crate::cli::help_code::CODE_SEARCH)]
    Search {
        query: String,
        /// Retrieval lane (default hybrid).
        #[arg(long, value_enum)]
        lane: Option<SearchLane>,
        /// Repository-relative prefix, or a glob when it holds `*` (`src/`, `src/**/*.rs`).
        #[arg(long)]
        path: Option<String>,
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
        /// Only files changed since this revision's merge base with `HEAD` (`HEAD`: the uncommitted
        /// changes), in `A..B` or in `A...B`; append `+importers` to add the files importing them.
        #[arg(long)]
        changed: Option<String>,
    },
    /// Usages of a name: call sites (default), implementations, importers, or the definition a
    /// reference binds to; or a file's importers (`--relation importers --path`) and the import
    /// cycles (`--relation cycles`), which take no name.
    #[command(alias = "references")]
    #[command(after_long_help = crate::cli::help_code::CODE_REFS)]
    Refs {
        name: Option<String>,
        /// What to list (default callers).
        #[arg(long, value_enum)]
        relation: Option<RefsRelation>,
        /// File defining the name (pins one definition); for `definition`, the file holding the
        /// reference.
        #[arg(long)]
        path: Option<String>,
        /// With `--path`: a 1-based line inside the definition meant, when several share the name.
        /// For `definition`: the line of the reference.
        #[arg(long)]
        line: Option<u32>,
        /// `definition` only: 1-based column (default: where the name occurs on the line).
        #[arg(long)]
        column: Option<u32>,
        /// `importers` with `--path`: import hops to follow (default 1, max 5).
        #[arg(long)]
        depth: Option<u32>,
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
        /// Call sites only: keep rows of these resolutions, comma-separated (`same_file`,
        /// `imported`, `scope_resolved`, `elsewhere`, `name_only`, `external`, `path_match`).
        #[arg(long)]
        resolution: Option<String>,
        /// Call sites only: only files changed since this revision's merge base with `HEAD` (`HEAD`:
        /// the uncommitted changes), in `A..B` or in `A...B`; append `+importers` to add the files
        /// importing them.
        #[arg(long)]
        changed: Option<String>,
    },
    /// Callers of the definition of `name` in `path`. Shorthand for `refs <name> --path <path>`.
    #[command(after_long_help = crate::cli::help_code::CODE_CALLERS)]
    Callers {
        path: String,
        name: String,
        /// A 1-based line inside the definition meant, when several in the file share the name.
        #[arg(long)]
        line: Option<u32>,
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
    /// Types implementing a trait / interface / base class. Shorthand for
    /// `refs <name> --relation implementations`.
    #[command(after_long_help = crate::cli::help_code::CODE_IMPLEMENTATIONS)]
    Implementations {
        name: String,
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
    /// Files importing a module. Shorthand for `refs <module> --relation importers`.
    #[command(after_long_help = crate::cli::help_code::CODE_DEPENDENTS)]
    Dependents {
        module: String,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// The definition the reference `name` at `path`:`line` binds to. Shorthand for
    /// `refs <name> --relation definition --path <path> --line <line>`.
    #[command(after_long_help = crate::cli::help_code::CODE_DEFINITION)]
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
    #[command(after_long_help = crate::cli::help_code::CODE_FIND)]
    Find {
        query: Option<String>,
        /// Repository-relative prefix, or a glob when it holds `*` (`src/`, `src/**/*.rs`).
        #[arg(long)]
        path: Option<String>,
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
        /// Only code files of at least this many lines, largest first, with their line counts.
        #[arg(long)]
        min_lines: Option<u32>,
        /// Only files changed since this revision's merge base with `HEAD` (`HEAD`: the uncommitted
        /// changes), in `A..B` or in `A...B`; append `+importers` to add the files importing them.
        #[arg(long)]
        changed: Option<String>,
        /// `true`: indexed files only; `false`: list-only ones (lockfiles, archives); `any` (default): both.
        #[arg(long, value_enum)]
        indexed: Option<IndexedChoice>,
    },
    /// List indexed files. Shorthand for `find` with no query.
    #[command(after_long_help = crate::cli::help_code::CODE_FILES)]
    Files {
        /// Repository-relative prefix, or a glob when it holds `*` (`src/`, `src/**/*.rs`).
        #[arg(long)]
        path: Option<String>,
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
    #[command(after_long_help = crate::cli::help_code::CODE_CHUNK)]
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
    #[command(flatten)]
    Analysis(AnalysisCmd),
}

/// Dispatch a `code` subcommand through the in-process server.
pub async fn run(tools: &CliContext, cmd: CodeCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let mut gate = None;
    let (tool, result) = match cmd {
        CodeCmd::Outline {
            path,
            limit,
            cursor,
            max_tokens,
        } => (
            "code_outline",
            tools
                .call(
                    "code_outline",
                    CodeOutlineParams {
                        path: resolve_path(tools, &path),
                        limit,
                        cursor: cursor.map(Cursor),
                        max_tokens,
                    },
                    async |s, p| s.op(p, Workspace::code_outline).await,
                )
                .await,
        ),
        CodeCmd::Symbols {
            name,
            kind,
            path,
            language,
            fuzzy,
            limit,
            cursor,
            max_tokens,
            changed,
        } => (
            "code_symbols",
            tools
                .call(
                    "code_symbols",
                    CodeSymbolsParams {
                        name,
                        kind,
                        path,
                        language,
                        fuzzy: fuzzy.then_some(true),
                        limit,
                        cursor: cursor.map(Cursor),
                        max_tokens,
                        changed,
                    },
                    async |s, p| s.op(p, Workspace::code_symbols).await,
                )
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
            tools
                .call(
                    "code_read",
                    CodeReadParams {
                        path: resolve_path(tools, &path),
                        symbol,
                        kind,
                        line,
                        end_line,
                    },
                    async |s, p| s.op_unpaged(p, Workspace::code_read).await,
                )
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
            changed,
        } => (
            "code_grep",
            tools
                .call(
                    "code_grep",
                    CodeGrepParams {
                        pattern,
                        regex: regex.then_some(true),
                        path,
                        language,
                        context,
                        limit,
                        cursor: cursor.map(Cursor),
                        max_tokens,
                        changed,
                    },
                    async |s, p| s.op(p, Workspace::code_grep).await,
                )
                .await,
        ),
        CodeCmd::Search {
            query,
            lane,
            path,
            language,
            limit,
            cursor,
            max_tokens,
            changed,
        } => (
            "code_search",
            tools
                .call(
                    "code_search",
                    CodeSearchParams {
                        query,
                        lane: lane.map(|lane| lane.as_str().to_string()),
                        path,
                        language,
                        limit,
                        cursor: cursor.map(Cursor),
                        max_tokens,
                        changed,
                    },
                    async |s, p| s.code_search(p).await,
                )
                .await,
        ),
        CodeCmd::Refs {
            name,
            relation,
            path,
            line,
            column,
            depth,
            match_mode,
            limit,
            cursor,
            max_tokens,
            resolution,
            changed,
        } => {
            let refs = CodeRefsParams {
                name,
                relation: relation.map(|relation| relation.as_str().to_string()),
                path: path.map(|path| resolve_path(tools, &path)),
                line,
                column,
                depth,
                match_mode: match_mode.map(|matching| matching.as_str().to_string()),
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
                resolution,
                changed,
            };
            (
                "code_refs",
                tools
                    .call("code_refs", refs, async |s, p| s.op(p, Workspace::code_refs).await)
                    .await,
            )
        }
        CodeCmd::Callers {
            path,
            name,
            line,
            match_mode,
            limit,
            cursor,
            max_tokens,
        } => {
            let refs = CodeRefsParams {
                path: Some(resolve_path(tools, &path)),
                line,
                match_mode: match_mode.map(|matching| matching.as_str().to_string()),
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
                ..refs(name, RefsRelation::Callers)
            };
            (
                "code_refs",
                tools
                    .call("code_refs", refs, async |s, p| s.op(p, Workspace::code_refs).await)
                    .await,
            )
        }
        CodeCmd::Implementations {
            name,
            match_mode,
            limit,
            cursor,
            max_tokens,
        } => {
            let refs = CodeRefsParams {
                match_mode: match_mode.map(|matching| matching.as_str().to_string()),
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
                ..refs(name, RefsRelation::Implementations)
            };
            (
                "code_refs",
                tools
                    .call("code_refs", refs, async |s, p| s.op(p, Workspace::code_refs).await)
                    .await,
            )
        }
        CodeCmd::Dependents {
            module,
            limit,
            cursor,
            max_tokens,
        } => {
            let refs = CodeRefsParams {
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
                ..refs(module, RefsRelation::Importers)
            };
            (
                "code_refs",
                tools
                    .call("code_refs", refs, async |s, p| s.op(p, Workspace::code_refs).await)
                    .await,
            )
        }
        CodeCmd::Definition {
            path,
            line,
            name,
            column,
        } => {
            let refs = CodeRefsParams {
                path: Some(resolve_path(tools, &path)),
                line: Some(line),
                column,
                ..refs(name, RefsRelation::Definition)
            };
            (
                "code_refs",
                tools
                    .call("code_refs", refs, async |s, p| s.op(p, Workspace::code_refs).await)
                    .await,
            )
        }
        CodeCmd::Find {
            query,
            path,
            language,
            limit,
            cursor,
            max_tokens,
            min_lines,
            changed,
            indexed,
        } => (
            "find_files",
            tools
                .call(
                    "find_files",
                    FindFilesToolParams {
                        query,
                        path,
                        language,
                        limit,
                        cursor: cursor.map(Cursor),
                        max_tokens,
                        min_lines,
                        changed,
                        indexed: indexed.map(IndexedChoice::filter),
                    },
                    async |s, p| s.op(p, Workspace::find_files).await,
                )
                .await,
        ),
        CodeCmd::Files {
            path,
            language,
            limit,
            cursor,
            max_tokens,
        } => (
            "find_files",
            tools
                .call(
                    "find_files",
                    FindFilesToolParams {
                        query: None,
                        path,
                        language,
                        limit,
                        cursor: cursor.map(Cursor),
                        max_tokens,
                        min_lines: None,
                        changed: None,
                        indexed: None,
                    },
                    async |s, p| s.op(p, Workspace::find_files).await,
                )
                .await,
        ),
        CodeCmd::Chunk {
            path,
            chunk_id,
            byte_start,
        } => (
            "code_chunk",
            tools
                .call(
                    "code_chunk",
                    GetChunkParams {
                        path: resolve_path(tools, &path),
                        chunk_id,
                        byte_start,
                    },
                    async |s, p| s.cli_op(p, Workspace::code_chunk).await,
                )
                .await,
        ),
        CodeCmd::Analysis(cmd) => {
            gate = analysis::gate(&cmd);
            analysis::call(tools, cmd).await
        }
    };

    let r = run_tool(tool, result)?;
    emit(tool, &r, opts, out)?;
    gate.map_or(Ok(()), |gate| gate(&r))
}

/// A `code_refs` call for `name` with only its relation set.
fn refs(name: String, relation: RefsRelation) -> CodeRefsParams {
    CodeRefsParams {
        name: Some(name),
        relation: Some(relation.as_str().to_string()),
        path: None,
        line: None,
        column: None,
        depth: None,
        match_mode: None,
        limit: None,
        cursor: None,
        max_tokens: None,
        resolution: None,
        changed: None,
    }
}
