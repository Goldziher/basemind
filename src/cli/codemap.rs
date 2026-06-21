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

use super::render::emit;
use super::run_tool;

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
}

pub async fn run(
    server: &BasemindServer,
    cmd: QueryCmd,
    json: bool,
    out: &mut impl Write,
) -> Result<()> {
    match cmd {
        QueryCmd::Outline { path, l2 } => {
            let p = OutlineParams {
                path: path.as_str().into(),
                l2,
                max_tokens: None,
            };
            let r = run_tool("outline", server.outline(Parameters(Lenient(p))).await)?;
            emit("outline", &r, json, out)
        }
        QueryCmd::Symbol {
            needle,
            kind,
            limit,
        }
        | QueryCmd::Search {
            needle,
            kind,
            limit,
        } => {
            let p = SearchSymbolsParams {
                needle,
                kind,
                limit,
                max_tokens: None,
                cursor: None,
            };
            let r = run_tool(
                "search_symbols",
                server.search_symbols(Parameters(Lenient(p))).await,
            )?;
            emit("search_symbols", &r, json, out)
        }
        QueryCmd::References { name, limit } => {
            let p = FindReferencesParams {
                name,
                limit,
                max_tokens: None,
                cursor: None,
            };
            let r = run_tool(
                "find_references",
                server.find_references(Parameters(Lenient(p))).await,
            )?;
            emit("find_references", &r, json, out)
        }
        QueryCmd::Callers {
            path,
            name,
            kind,
            limit,
        } => {
            let p = FindCallersParams {
                path: path.as_str().into(),
                name,
                kind,
                limit,
                max_tokens: None,
                cursor: None,
            };
            let r = run_tool(
                "find_callers",
                server.find_callers(Parameters(Lenient(p))).await,
            )?;
            emit("find_callers", &r, json, out)
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
                path: path.map(|s| s.as_str().into()),
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
                include_context: !no_context,
                cursor: None,
            };
            let r = run_tool(
                "workspace_grep",
                server.workspace_grep(Parameters(Lenient(p))).await,
            )?;
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
            let r = run_tool(
                "repo_info",
                server.repo_info(Parameters(RepoInfoParams {})).await,
            )?;
            emit("repo_info", &r, json, out)
        }
        QueryCmd::Dependents { module } => {
            let p = DependentsParams { module };
            let r = run_tool(
                "dependents",
                server.dependents(Parameters(Lenient(p))).await,
            )?;
            emit("dependents", &r, json, out)
        }
    }
}
