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

use super::choices::*;
use super::render::{Emit, emit};
use super::{resolve_path, run_tool};

#[derive(Subcommand, Debug)]
pub enum GraphCmd {
    /// Walk the call chain up (callers) or down (callees) from one function.
    Calls {
        name: String,
        #[arg(long, value_enum, default_value_t = CallDirection::Callers)]
        direction: CallDirection,
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
        #[arg(long, value_enum, default_value_t = NeighborDirection::Both)]
        direction: NeighborDirection,
        #[arg(long)]
        depth: Option<u32>,
        #[arg(long, value_enum, default_value_t = EdgeLane::All)]
        edges: EdgeLane,
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
        #[arg(long, value_enum, default_value_t = EdgeLane::All)]
        edges: EdgeLane,
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
        #[arg(long, value_enum, default_value_t = EdgeLane::All)]
        edges: EdgeLane,
        #[arg(long)]
        min_confidence: Option<f32>,
        #[arg(long)]
        max_nodes: Option<u32>,
    },
    /// Cluster the code-graph into de-facto modules.
    Communities {
        #[arg(long, value_enum, default_value_t = EdgeLane::All)]
        edges: EdgeLane,
        #[arg(long, value_enum, default_value_t = CommunityAlgorithm::LabelPropagation)]
        algorithm: CommunityAlgorithm,
        #[arg(long)]
        min_confidence: Option<f32>,
        #[arg(long)]
        max_communities: Option<u32>,
        #[arg(long)]
        members_per_community: Option<u32>,
    },
    /// Whole-repo architecture map ranked by graph centrality + git churn.
    Map {
        #[arg(long, value_enum, default_value_t = Granularity::Module)]
        granularity: Granularity,
        #[arg(long)]
        focus: Option<String>,
        #[arg(long)]
        depth: Option<u32>,
        #[arg(long, value_enum, default_value_t = EdgeLane::Calls)]
        edges: EdgeLane,
        /// Skip the git-churn overlay (it is on by default).
        #[arg(long)]
        no_churn: bool,
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
        #[arg(long, value_enum, default_value_t = ExportFormat::NodeLink)]
        format: ExportFormat,
        #[arg(long)]
        focus: Option<String>,
        #[arg(long, value_enum, default_value_t = EdgeLane::All)]
        edges: EdgeLane,
        #[arg(long, value_enum, default_value_t = CommunityAlgorithm::LabelPropagation)]
        algorithm: CommunityAlgorithm,
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
        #[arg(long, value_enum, default_value_t = VisualFormat::Html)]
        format: VisualFormat,
        #[arg(long)]
        focus: Option<String>,
        #[arg(long, value_enum, default_value_t = EdgeLane::All)]
        edges: EdgeLane,
        #[arg(long, value_enum, default_value_t = CommunityAlgorithm::LabelPropagation)]
        algorithm: CommunityAlgorithm,
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
        #[arg(long, value_enum, default_value_t = VisualFormat::Html)]
        format: VisualFormat,
        #[arg(long)]
        focus: Option<String>,
        #[arg(long, value_enum, default_value_t = EdgeLane::All)]
        edges: EdgeLane,
        #[arg(long, value_enum, default_value_t = CommunityAlgorithm::LabelPropagation)]
        algorithm: CommunityAlgorithm,
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
            direction: Some(direction.as_str().to_string()),
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
            direction: Some(direction.as_str().to_string()),
            depth,
            edges: Some(edges.as_str().to_string()),
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
            edges: Some(edges.as_str().to_string()),
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
            edges: Some(edges.as_str().to_string()),
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
            edges: Some(edges.as_str().to_string()),
            algorithm: Some(algorithm.as_str().to_string()),
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
            no_churn,
            churn_window,
            max_nodes,
            max_edges,
            max_tokens,
        } => GraphParams {
            granularity: Some(granularity.as_str().to_string()),
            focus: focus.map(|s| resolve_path(server, &s)),
            depth,
            edges: Some(edges.as_str().to_string()),
            include_churn: Some(!no_churn),
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
            format: Some(format.as_str().to_string()),
            focus: focus.map(|s| resolve_path(server, &s)),
            edges: Some(edges.as_str().to_string()),
            algorithm: Some(algorithm.as_str().to_string()),
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
            format: Some(format.as_str().to_string()),
            focus: focus.map(|s| resolve_path(server, &s)),
            edges: Some(edges.as_str().to_string()),
            algorithm: Some(algorithm.as_str().to_string()),
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
            format: Some(format.as_str().to_string()),
            focus: focus.map(|s| resolve_path(server, &s)),
            edges: Some(edges.as_str().to_string()),
            algorithm: Some(algorithm.as_str().to_string()),
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
