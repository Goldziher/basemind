//! `basemind graph` — the CLI half of the `graph` domain.
//!
//! Real clap subcommands rather than a `--mode` flag, so each operation keeps its own `--help` and
//! its own argument validation; they map one-to-one onto the MCP `graph` tool's [`GraphMode`]
//! values, which is what `tests/cli_parity.rs` asserts.
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

/// Resolve a user-supplied CLI path into the repo-relative key the index is keyed by, falling back
/// to the raw input so the tool reports "not indexed" rather than silently mangling it.
fn resolve_path(server: &BasemindServer, path: &str) -> RelPath {
    match normalize_query_path(path, &server.state.shared.root) {
        Some(rel) => RelPath::from(rel),
        None => RelPath::from(path),
    }
}

#[derive(Subcommand, Debug)]
pub enum GraphCmd {
    /// Walk the call chain up (callers) or down (callees) from one function.
    Calls {
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
    /// Cluster the code-graph into de-facto modules.
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
    /// Whole-repo architecture map ranked by graph centrality + git churn.
    Map {
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
    /// Render the code-graph to a text format (node_link/dot/mermaid/graphml/cypher/html/svg).
    Export {
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
        #[arg(long)]
        max_edges: Option<u32>,
        /// Also write the rendered export to the cache and print its path in `output_path`.
        #[arg(long)]
        write: bool,
    },
    /// Render a visual view (html/svg) and open it in your default desktop viewer.
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
        #[arg(long)]
        max_edges: Option<u32>,
        /// Only write the export and print its path; do not open a viewer.
        #[arg(long = "no-open")]
        no_open: bool,
    },
    /// Return a browsable URL for the interactive graph UI — a live `http://…/ui` page when a
    /// basemind daemon is serving, else a `file://` export.
    Open {
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
        #[arg(long)]
        max_edges: Option<u32>,
        /// Only resolve/write the UI and print its URL; do not open a viewer.
        #[arg(long = "no-open")]
        no_open: bool,
    },
}

/// Dispatch a `graph` subcommand through the in-process server.
pub async fn run(server: &BasemindServer, cmd: GraphCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let p = match cmd {
        GraphCmd::Calls {
            name,
            direction,
            path,
            max_depth,
            max_nodes,
        } => GraphParams {
            name: Some(name),
            direction: Some(direction),
            path: path.map(|s| resolve_path(server, &s)),
            max_depth,
            max_nodes,
            ..GraphParams::new(GraphMode::Calls)
        },
        GraphCmd::Neighbors {
            name,
            path,
            direction,
            depth,
            edges,
            min_confidence,
            max_nodes,
        } => GraphParams {
            name: Some(name),
            path: path.map(|s| resolve_path(server, &s)),
            direction: Some(direction),
            depth,
            edges: Some(edges),
            min_confidence,
            max_nodes,
            ..GraphParams::new(GraphMode::Neighbors)
        },
        GraphCmd::Path {
            from,
            to,
            from_path,
            to_path,
            edges,
            include_contains,
            min_confidence,
        } => GraphParams {
            from: Some(from),
            from_path: from_path.map(|s| resolve_path(server, &s)),
            to: Some(to),
            to_path: to_path.map(|s| resolve_path(server, &s)),
            edges: Some(edges),
            include_contains: Some(include_contains),
            min_confidence,
            ..GraphParams::new(GraphMode::Path)
        },
        GraphCmd::Subgraph {
            name,
            path,
            depth,
            edges,
            min_confidence,
            max_nodes,
        } => GraphParams {
            name: Some(name),
            path: path.map(|s| resolve_path(server, &s)),
            depth,
            edges: Some(edges),
            min_confidence,
            max_nodes,
            ..GraphParams::new(GraphMode::Subgraph)
        },
        GraphCmd::Communities {
            edges,
            algorithm,
            min_confidence,
            max_communities,
            members_per_community,
        } => GraphParams {
            edges: Some(edges),
            algorithm: Some(algorithm),
            min_confidence,
            max_communities,
            members_per_community,
            ..GraphParams::new(GraphMode::Communities)
        },
        GraphCmd::Map {
            granularity,
            focus,
            depth,
            edges,
            include_churn,
            churn_window,
            max_nodes,
            max_edges,
            max_tokens,
        } => GraphParams {
            granularity: Some(granularity),
            focus: focus.map(RelPath::from),
            depth,
            edges: Some(edges),
            include_churn: Some(include_churn),
            churn_window,
            max_nodes,
            max_edges,
            max_tokens,
            ..GraphParams::new(GraphMode::Map)
        },
        GraphCmd::Export {
            format,
            focus,
            edges,
            algorithm,
            min_confidence,
            max_nodes,
            max_edges,
            write,
        } => GraphParams {
            format: Some(format),
            focus: focus.map(RelPath::from),
            edges: Some(edges),
            algorithm: Some(algorithm),
            min_confidence,
            max_nodes,
            max_edges,
            write: Some(write),
            ..GraphParams::new(GraphMode::Export)
        },
        GraphCmd::Display {
            format,
            focus,
            edges,
            algorithm,
            min_confidence,
            max_nodes,
            max_edges,
            no_open,
        } => GraphParams {
            format: Some(format),
            focus: focus.map(RelPath::from),
            edges: Some(edges),
            algorithm: Some(algorithm),
            min_confidence,
            max_nodes,
            max_edges,
            open: Some(!no_open),
            ..GraphParams::new(GraphMode::Display)
        },
        GraphCmd::Open {
            format,
            focus,
            edges,
            algorithm,
            min_confidence,
            max_nodes,
            max_edges,
            no_open,
        } => GraphParams {
            format: Some(format),
            focus: focus.map(RelPath::from),
            edges: Some(edges),
            algorithm: Some(algorithm),
            min_confidence,
            max_nodes,
            max_edges,
            open: Some(!no_open),
            ..GraphParams::new(GraphMode::Open)
        },
    };

    let key = p.mode.telemetry_key();
    let r = run_tool(key, server.graph(Parameters(Lenient(p))).await)?;
    emit(key, &r, opts, out)
}
