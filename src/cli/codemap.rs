//! Code-map query subcommands: 1:1 with the MCP code-map tools.
//!
//! Each handler builds the matching `*Params` struct from clap args, calls the
//! identical `#[tool]` method on the in-process [`BasemindServer`], and renders
//! the result. No query logic lives here — parity is by construction.

use std::io::Write;

use anyhow::Result;
use clap::Subcommand;

use crate::mcp::BasemindServer;
use crate::mcp::params::*;
use crate::path::{RelPath, normalize_query_path};

use super::render::emit;
use super::run_tool;

/// Resolve a user-supplied CLI path into the repo-relative `RelPath` key the
/// index is keyed by (scanner-produced: no leading `./`, never absolute).
///
/// `query outline /abs/repo/src/foo.rs` and `query outline ./src/foo.rs` both
/// resolve to `src/foo.rs`. Paths that escape or fall outside the repository
/// can't match an indexed file, so we fall back to the raw input and let the
/// downstream tool report "file not indexed" rather than silently mangling it.
fn resolve_path(server: &BasemindServer, path: &str) -> RelPath {
    match normalize_query_path(path, &server.state.root) {
        Some(rel) => RelPath::from(rel),
        None => RelPath::from(path),
    }
}

#[derive(Subcommand, Debug)]
pub enum QueryCmd {
    /// File outline: symbols + imports, optionally calls + docs (L2).
    Outline {
        /// Repository-relative path.
        path: String,
        /// Also include calls + doc comments (L2).
        #[arg(long)]
        l2: bool,
    },
    /// Search symbols by name substring (alias of `search`).
    Symbol {
        needle: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Search symbols by name substring.
    Search {
        needle: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Call sites of any callee whose identifier matches `name`.
    References {
        name: String,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Callers of a specific definition (path + name + optional kind).
    Callers {
        path: String,
        name: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Resolve the reference at a position to its scope-resolved definition.
    GotoDefinition {
        /// Repository-relative path of the file holding the reference.
        path: String,
        /// 1-based line of the reference identifier.
        line: u32,
        /// 0-based byte column of the reference within the line (default 0).
        #[arg(long, default_value_t = 0)]
        column: u32,
    },
    /// Types implementing / extending / inheriting from a trait or base class.
    Implementations {
        trait_name: String,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Transitive call-graph walk from a root function.
    CallGraph {
        name: String,
        #[arg(long, default_value = "callers")]
        direction: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        max_depth: Option<u32>,
        #[arg(long)]
        max_nodes: Option<u32>,
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
    ListFiles {
        #[arg(long)]
        path_contains: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// High-level repo + cache state.
    Status,
    /// Workdir + branch + HEAD sha.
    RepoInfo,
    /// Files whose imports mention the given module (heuristic).
    Dependents { module: String },
    /// Semantic (vector) search over indexed code chunks. Returns pointers; fetch bodies with
    /// `get-chunk`. Needs `--features code-search`.
    SearchCode {
        query: String,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        format: Option<String>,
    },
    /// Fetch one code chunk's body by path (from a `search-code` hit). Needs `--features
    /// code-search`.
    GetChunk {
        /// Repository-relative path of the source file.
        path: String,
        #[arg(long)]
        chunk_id: Option<String>,
        #[arg(long)]
        byte_start: Option<u32>,
    },
    /// Expand a symbol to its raw source body (the inverse of an outline entry).
    Expand {
        /// Repository-relative path of the indexed file.
        path: String,
        /// Symbol name (matched exactly, case-sensitive).
        name: String,
        /// Kind filter to disambiguate (e.g. `function`, `struct`, `method`).
        #[arg(long)]
        kind: Option<String>,
    },
}

pub async fn run(server: &BasemindServer, cmd: QueryCmd, json: bool, out: &mut impl Write) -> Result<()> {
    match cmd {
        QueryCmd::Outline { path, l2 } => {
            let p = OutlineParams {
                path: resolve_path(server, &path),
                l2,
                max_tokens: None,
                format: None,
            };
            let r = run_tool("outline", server.outline(Parameters(Lenient(p))).await)?;
            emit("outline", &r, json, out)
        }
        QueryCmd::Symbol { needle, kind, limit } | QueryCmd::Search { needle, kind, limit } => {
            let p = SearchSymbolsParams {
                needle,
                kind,
                limit,
                max_tokens: None,
                format: None,
                cursor: None,
            };
            let r = run_tool("search_symbols", server.search_symbols(Parameters(Lenient(p))).await)?;
            emit("search_symbols", &r, json, out)
        }
        QueryCmd::References { name, limit } => {
            let p = FindReferencesParams {
                name,
                limit,
                max_tokens: None,
                format: None,
                cursor: None,
            };
            let r = run_tool("find_references", server.find_references(Parameters(Lenient(p))).await)?;
            emit("find_references", &r, json, out)
        }
        QueryCmd::Callers {
            path,
            name,
            kind,
            limit,
        } => {
            let p = FindCallersParams {
                path: resolve_path(server, &path),
                name,
                kind,
                limit,
                max_tokens: None,
                cursor: None,
            };
            let r = run_tool("find_callers", server.find_callers(Parameters(Lenient(p))).await)?;
            emit("find_callers", &r, json, out)
        }
        QueryCmd::GotoDefinition { path, line, column } => {
            let p = GotoDefinitionParams {
                path: resolve_path(server, &path),
                line,
                column,
            };
            let r = run_tool("goto_definition", server.goto_definition(Parameters(Lenient(p))).await)?;
            emit("goto_definition", &r, json, out)
        }
        QueryCmd::Implementations {
            trait_name,
            language,
            limit,
        } => {
            let p = FindImplementationsParams {
                trait_name,
                language,
                limit,
                max_tokens: None,
                cursor: None,
            };
            let r = run_tool(
                "find_implementations",
                server.find_implementations(Parameters(Lenient(p))).await,
            )?;
            emit("find_implementations", &r, json, out)
        }
        QueryCmd::CallGraph {
            name,
            direction,
            path,
            max_depth,
            max_nodes,
        } => {
            let p = CallGraphParams {
                name,
                direction,
                path: path.map(|s| resolve_path(server, &s)),
                max_depth,
                max_nodes,
            };
            let r = run_tool("call_graph", server.call_graph(Parameters(p)).await)?;
            emit("call_graph", &r, json, out)
        }
        QueryCmd::Grep {
            pattern,
            language,
            path_contains,
            limit,
            no_context,
        } => {
            let p = WorkspaceGrepParams {
                pattern,
                language,
                path_contains,
                limit,
                max_tokens: None,
                format: None,
                include_context: !no_context,
                cursor: None,
            };
            let r = run_tool("workspace_grep", server.workspace_grep(Parameters(Lenient(p))).await)?;
            emit("workspace_grep", &r, json, out)
        }
        QueryCmd::ListFiles {
            path_contains,
            language,
            limit,
        } => {
            let p = ListFilesParams {
                path_contains,
                language,
                limit,
                max_tokens: None,
                format: None,
                cursor: None,
            };
            let r = run_tool("list_files", server.list_files(Parameters(p)).await)?;
            emit("list_files", &r, json, out)
        }
        QueryCmd::Status => {
            let r = run_tool("status", server.status(Parameters(StatusParams {})).await)?;
            emit("status", &r, json, out)
        }
        QueryCmd::RepoInfo => {
            let r = run_tool("repo_info", server.repo_info(Parameters(RepoInfoParams {})).await)?;
            emit("repo_info", &r, json, out)
        }
        QueryCmd::Dependents { module } => {
            let p = DependentsParams { module };
            let r = run_tool("dependents", server.dependents(Parameters(Lenient(p))).await)?;
            emit("dependents", &r, json, out)
        }
        QueryCmd::SearchCode { query, limit, format } => {
            let p = SearchCodeParams {
                query,
                limit,
                max_tokens: None,
                format,
            };
            let r = run_tool("search_code", server.search_code(Parameters(Lenient(p))).await)?;
            emit("search_code", &r, json, out)
        }
        QueryCmd::GetChunk {
            path,
            chunk_id,
            byte_start,
        } => {
            let p = GetChunkParams {
                path: resolve_path(server, &path),
                chunk_id,
                byte_start,
            };
            let r = run_tool("get_chunk", server.get_chunk(Parameters(Lenient(p))).await)?;
            emit("get_chunk", &r, json, out)
        }
        QueryCmd::Expand { path, name, kind } => {
            let p = ExpandParams {
                path: resolve_path(server, &path),
                name,
                kind,
            };
            let r = run_tool("expand", server.expand(Parameters(p)).await)?;
            emit("expand", &r, json, out)
        }
    }
}
