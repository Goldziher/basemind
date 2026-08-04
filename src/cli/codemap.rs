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

use super::render::{Emit, emit};
use super::run_tool;

/// Resolve a user-supplied CLI path into the repo-relative `RelPath` key the
/// index is keyed by (scanner-produced: no leading `./`, never absolute).
///
/// `query outline /abs/repo/src/foo.rs` and `query outline ./src/foo.rs` both
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
    /// Architecture map ranked by graph centrality + git churn.
    ArchitectureMap {
        #[arg(long, default_value = "module")]
        granularity: String,
        #[arg(long)]
        focus: Option<String>,
        #[arg(long)]
        depth: Option<u32>,
        #[arg(long, default_value = "calls")]
        edges: String,
        #[arg(long, default_value_t = true)]
        include_churn: bool,
        #[arg(long)]
        churn_window: Option<u32>,
        #[arg(long)]
        max_nodes: Option<u32>,
        #[arg(long)]
        max_edges: Option<u32>,
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// N-hop neighborhood around a symbol over the unified code-graph.
    Neighbors {
        name: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long, default_value = "both")]
        direction: String,
        #[arg(long)]
        depth: Option<u32>,
        #[arg(long, default_value = "all")]
        edges: String,
        #[arg(long)]
        min_confidence: Option<f32>,
        #[arg(long)]
        max_nodes: Option<u32>,
    },
    /// Confidence-weighted shortest path between two symbols over the code-graph.
    Path {
        from: String,
        to: String,
        #[arg(long)]
        from_path: Option<String>,
        #[arg(long)]
        to_path: Option<String>,
        #[arg(long, default_value = "all")]
        edges: String,
        /// Include containment (file→symbol) edges in the search.
        #[arg(long)]
        include_contains: bool,
        #[arg(long)]
        min_confidence: Option<f32>,
    },
    /// Readable neighborhood subgraph around a symbol, cut to the central head.
    Subgraph {
        name: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        depth: Option<u32>,
        #[arg(long, default_value = "all")]
        edges: String,
        #[arg(long)]
        min_confidence: Option<f32>,
        #[arg(long)]
        max_nodes: Option<u32>,
    },
    /// Detect communities (de-facto modules) over the code-graph.
    Communities {
        #[arg(long, default_value = "all")]
        edges: String,
        #[arg(long, default_value = "label_propagation")]
        algorithm: String,
        #[arg(long)]
        min_confidence: Option<f32>,
        #[arg(long)]
        max_communities: Option<u32>,
        #[arg(long)]
        members_per_community: Option<u32>,
    },
    /// Render the code-graph to a text format (node_link/dot/mermaid/graphml/cypher/html/svg).
    GraphExport {
        #[arg(long, default_value = "node_link")]
        format: String,
        #[arg(long)]
        focus: Option<String>,
        #[arg(long, default_value = "all")]
        edges: String,
        #[arg(long, default_value = "label_propagation")]
        algorithm: String,
        #[arg(long)]
        min_confidence: Option<f32>,
        #[arg(long)]
        max_nodes: Option<u32>,
        /// Also write the rendered export to the cache and print its path in `output_path`.
        #[arg(long)]
        write: bool,
    },
    /// Render a visual view of the code-graph (html/svg) and open it in your default desktop viewer.
    Display {
        #[arg(long, default_value = "html")]
        format: String,
        #[arg(long)]
        focus: Option<String>,
        #[arg(long, default_value = "all")]
        edges: String,
        #[arg(long, default_value = "label_propagation")]
        algorithm: String,
        #[arg(long)]
        min_confidence: Option<f32>,
        #[arg(long)]
        max_nodes: Option<u32>,
        /// Only write the export and print its path; do not open a viewer.
        #[arg(long = "no-open")]
        no_open: bool,
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
    /// Fuzzy filename/path search (fzf/fd-style), ranked by score.
    FindFiles {
        query: String,
        #[arg(long)]
        path_prefix: Option<String>,
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
    /// Search indexed code chunks — `hybrid` (RRF fusion, default), `semantic` (vector), or
    /// `keyword` (BM25). Returns pointers; fetch bodies with `get-chunk`. Needs `--features
    /// code-search`.
    SearchCode {
        query: String,
        #[arg(long)]
        limit: Option<u32>,
        /// Retrieval lane: `hybrid` (default), `semantic`, or `keyword`.
        #[arg(long)]
        mode: Option<String>,
        /// Run the cross-encoder rerank pass over the fused hits (first call downloads a model).
        #[arg(long)]
        rerank: bool,
        /// Reranker preset name (default `bge-reranker-base`).
        #[arg(long)]
        rerank_preset: Option<String>,
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

pub async fn run(server: &BasemindServer, cmd: QueryCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    match cmd {
        QueryCmd::Outline { path, l2 } => {
            let p = OutlineParams {
                path: resolve_path(server, &path),
                l2,
                max_tokens: None,
                format: None,
            };
            let r = run_tool("outline", server.outline(Parameters(Lenient(p))).await)?;
            emit("outline", &r, opts, out)
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
            emit("search_symbols", &r, opts, out)
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
            emit("find_references", &r, opts, out)
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
            emit("find_callers", &r, opts, out)
        }
        QueryCmd::GotoDefinition { path, line, column } => {
            let p = GotoDefinitionParams {
                path: resolve_path(server, &path),
                line,
                column,
            };
            let r = run_tool("goto_definition", server.goto_definition(Parameters(Lenient(p))).await)?;
            emit("goto_definition", &r, opts, out)
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
            emit("find_implementations", &r, opts, out)
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
            emit("call_graph", &r, opts, out)
        }
        QueryCmd::ArchitectureMap {
            granularity,
            focus,
            depth,
            edges,
            include_churn,
            churn_window,
            max_nodes,
            max_edges,
            max_tokens,
        } => {
            let p = ArchitectureMapParams {
                granularity,
                focus,
                depth,
                edges,
                include_churn,
                churn_window,
                max_nodes,
                max_edges,
                max_tokens,
            };
            let r = run_tool("architecture_map", server.architecture_map(Parameters(p)).await)?;
            emit("architecture_map", &r, opts, out)
        }
        QueryCmd::Neighbors {
            name,
            path,
            direction,
            depth,
            edges,
            min_confidence,
            max_nodes,
        } => {
            let p = NeighborsParams {
                name,
                path: path.map(|s| resolve_path(server, &s)),
                direction,
                depth,
                edges,
                min_confidence,
                max_nodes,
            };
            let r = run_tool("neighbors", server.neighbors(Parameters(p)).await)?;
            emit("neighbors", &r, opts, out)
        }
        QueryCmd::Path {
            from,
            to,
            from_path,
            to_path,
            edges,
            include_contains,
            min_confidence,
        } => {
            let p = PathParams {
                from,
                from_path: from_path.map(|s| resolve_path(server, &s)),
                to,
                to_path: to_path.map(|s| resolve_path(server, &s)),
                edges,
                include_contains,
                min_confidence,
            };
            let r = run_tool("path", server.path(Parameters(p)).await)?;
            emit("path", &r, opts, out)
        }
        QueryCmd::Subgraph {
            name,
            path,
            depth,
            edges,
            min_confidence,
            max_nodes,
        } => {
            let p = SubgraphParams {
                name,
                path: path.map(|s| resolve_path(server, &s)),
                depth,
                edges,
                min_confidence,
                max_nodes,
            };
            let r = run_tool("subgraph", server.subgraph(Parameters(p)).await)?;
            emit("subgraph", &r, opts, out)
        }
        QueryCmd::Communities {
            edges,
            algorithm,
            min_confidence,
            max_communities,
            members_per_community,
        } => {
            let p = CommunitiesParams {
                edges,
                algorithm,
                min_confidence,
                max_communities,
                members_per_community,
            };
            let r = run_tool("communities", server.communities(Parameters(p)).await)?;
            emit("communities", &r, opts, out)
        }
        QueryCmd::GraphExport {
            format,
            focus,
            edges,
            algorithm,
            min_confidence,
            max_nodes,
            write,
        } => {
            let p = GraphExportParams {
                format,
                focus,
                edges,
                algorithm,
                min_confidence,
                max_nodes,
                write,
            };
            let r = run_tool("graph_export", server.graph_export(Parameters(p)).await)?;
            emit("graph_export", &r, opts, out)
        }
        QueryCmd::Display {
            format,
            focus,
            edges,
            algorithm,
            min_confidence,
            max_nodes,
            no_open,
        } => {
            let p = DisplayParams {
                format,
                focus,
                edges,
                algorithm,
                min_confidence,
                max_nodes,
                open: !no_open,
            };
            let r = run_tool("display", server.display(Parameters(p)).await)?;
            emit("display", &r, opts, out)
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
            emit("workspace_grep", &r, opts, out)
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
            emit("list_files", &r, opts, out)
        }
        QueryCmd::FindFiles {
            query,
            path_prefix,
            language,
            limit,
        } => {
            let p = FindFilesParams {
                query,
                path_prefix,
                language,
                limit,
                max_tokens: None,
                format: None,
                cursor: None,
            };
            let r = run_tool("find_files", server.find_files(Parameters(Lenient(p))).await)?;
            emit("find_files", &r, opts, out)
        }
        QueryCmd::Status => {
            let r = run_tool("status", server.status(Parameters(StatusParams {})).await)?;
            emit("status", &r, opts, out)
        }
        QueryCmd::RepoInfo => {
            let r = run_tool("repo_info", server.repo_info(Parameters(RepoInfoParams {})).await)?;
            emit("repo_info", &r, opts, out)
        }
        QueryCmd::Dependents { module } => {
            let p = DependentsParams { module };
            let r = run_tool("dependents", server.dependents(Parameters(Lenient(p))).await)?;
            emit("dependents", &r, opts, out)
        }
        QueryCmd::SearchCode {
            query,
            limit,
            mode,
            rerank,
            rerank_preset,
            format,
        } => {
            let p = SearchCodeParams {
                query,
                limit,
                max_tokens: None,
                mode,
                reranker_enabled: rerank.then_some(true),
                reranker_preset: rerank_preset,
                reranker_top_k: None,
                format,
            };
            let r = run_tool("search_code", server.search_code(Parameters(Lenient(p))).await)?;
            emit("search_code", &r, opts, out)
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
            emit("get_chunk", &r, opts, out)
        }
        QueryCmd::Expand { path, name, kind } => {
            let p = ExpandParams {
                path: resolve_path(server, &path),
                name,
                kind,
            };
            let r = run_tool("expand", server.expand(Parameters(p)).await)?;
            emit("expand", &r, opts, out)
        }
    }
}
