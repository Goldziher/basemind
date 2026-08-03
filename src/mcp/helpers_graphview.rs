//! Body of the `graph_export` tool (ADR-0005).
//!
//! [`build_graph_view`] assembles the canonical [`GraphView`] from the shared code-graph: it builds
//! the graph over the requested lanes, filters to the confidence floor, detects communities
//! (ADR-0004) and labels them, ranks nodes by centrality, caps the view, and describes each kept
//! node via the L1 cache. [`run_graph_export`] renders that payload into the requested text format
//! ([`graph_view::render`]). The graph is built on demand and discarded — no persisted state.

use ahash::{AHashMap, AHashSet};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::MapCache;
use super::codegraph::{self, BuildOpts, CodeEdge, CodeGraph, EdgeKindSet};
use super::community::{self, CommunityAlgo};
use super::graph_view::{self, GraphFormat, GraphView, GraphViewEdge, GraphViewNode};
use super::helpers::{elapsed_us, json_result};
use super::helpers_community::label_for;
use super::helpers_traverse::{describe, kinds_from};
use super::traverse::Adjacency;
use super::types::LifecycleNotice;
use super::types_graphview::{GraphExportParams, GraphExportResponse};
use crate::index::IndexDb;

/// Sweep bound for community detection when tagging nodes; converges well inside this on real graphs.
const GRAPHVIEW_COMMUNITY_ITERS: u32 = 50;
const DEFAULT_MAX_NODES: u32 = 500;
const MAX_MAX_NODES: u32 = 2000;

/// Label each detected community from its members (most central first): dominant path prefix + most
/// central member (ADR-0004). Returns a label per dense community id.
fn label_communities(adj: &Adjacency, cache: &MapCache, partition: &community::Partition) -> Vec<String> {
    let mut by_comm: Vec<Vec<u32>> = vec![Vec::new(); partition.num_communities as usize];
    for (id, &c) in partition.community_of.iter().enumerate() {
        by_comm[c as usize].push(id as u32);
    }
    by_comm
        .iter_mut()
        .map(|members| {
            members.sort_by(|&a, &b| {
                partition.centrality[b as usize]
                    .cmp(&partition.centrality[a as usize])
                    .then(a.cmp(&b))
            });
            label_for(adj, cache, members)
        })
        .collect()
}

/// Keep the most central `max_nodes` node ids (centrality desc, id asc), returned in ascending id
/// order for stable output. Returns the kept original ids, a map from original id → dense output
/// index, and whether the view was capped.
fn select_nodes(partition: &community::Partition, n: usize, max_nodes: usize) -> (Vec<u32>, AHashMap<u32, u32>, bool) {
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_by(|&a, &b| {
        partition.centrality[b as usize]
            .cmp(&partition.centrality[a as usize])
            .then(a.cmp(&b))
    });
    let capped = order.len() > max_nodes;
    order.truncate(max_nodes);
    order.sort_unstable();
    let mut remap: AHashMap<u32, u32> = AHashMap::with_capacity(order.len());
    for (new_id, &orig) in order.iter().enumerate() {
        remap.insert(orig, new_id as u32);
    }
    (order, remap, capped)
}

/// Assemble the canonical graph-view payload from the shared code-graph. `max_nodes` keeps the most
/// central nodes (id-ascending among the kept set for readable output) and the edges induced among
/// them; `truncated` flags a capped or scan-truncated view.
pub(super) fn build_graph_view(
    idx: Option<&IndexDb>,
    cache: &MapCache,
    kinds: EdgeKindSet,
    min_conf: f32,
    algo: CommunityAlgo,
    focus: Option<String>,
    max_nodes: usize,
) -> Result<GraphView, McpError> {
    let built = codegraph::build(
        idx,
        cache,
        &BuildOpts {
            kinds,
            focus,
            scan_cap: codegraph::CODEGRAPH_SCAN_CAP,
        },
    )?;
    let scan_truncated = built.truncated;
    let edges: Vec<CodeEdge> = built
        .edges
        .into_iter()
        .filter(|e| e.provenance.confidence() >= min_conf)
        .collect();
    let graph = CodeGraph {
        edges,
        truncated: scan_truncated,
    };
    let adj = Adjacency::build(&graph);
    let partition = community::detect(&adj, algo, GRAPHVIEW_COMMUNITY_ITERS);
    let comm_label = label_communities(&adj, cache, &partition);
    let (order, remap, capped) = select_nodes(&partition, adj.node_count(), max_nodes);

    let nodes: Vec<GraphViewNode> = order
        .iter()
        .enumerate()
        .map(|(new_id, &orig)| {
            let described = describe(cache, adj.node(orig));
            let community = partition.community_of[orig as usize];
            GraphViewNode {
                id: new_id as u32,
                name: if described.name.is_empty() {
                    described.kind.clone()
                } else {
                    described.name
                },
                kind: described.kind,
                path: described.path,
                start_row: described.start_row,
                start_col: described.start_col,
                community,
                community_label: comm_label[community as usize].clone(),
                centrality: partition.centrality[orig as usize],
            }
        })
        .collect();

    let mut view_edges: Vec<GraphViewEdge> = Vec::new();
    for e in &graph.edges {
        let from = adj.id(&e.from).and_then(|i| remap.get(&i).copied());
        let to = adj.id(&e.to).and_then(|i| remap.get(&i).copied());
        let (Some(from), Some(to)) = (from, to) else {
            continue;
        };
        view_edges.push(GraphViewEdge {
            from,
            to,
            kind: e.kind.as_str().to_string(),
            provenance: e.provenance.as_str().to_string(),
            confidence: e.provenance.confidence(),
            weight: e.weight,
        });
    }

    Ok(GraphView {
        nodes,
        edges: view_edges,
        truncated: scan_truncated || capped,
    })
}

/// `graph_export` — render the code-graph into a chosen text format.
pub(super) fn run_graph_export(
    idx: Option<&IndexDb>,
    cache: &MapCache,
    params: GraphExportParams,
    notice: Option<LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let format = GraphFormat::parse(&params.format).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "format must be node_link/dot/mermaid/graphml/cypher/html, got {:?}",
                params.format
            ),
            None,
        )
    })?;
    let algo = CommunityAlgo::parse(&params.algorithm).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "algorithm must be label_propagation or louvain, got {:?}",
                params.algorithm
            ),
            None,
        )
    })?;
    let kinds = kinds_from(&params.edges, false)?;
    let min_conf = params.min_confidence.unwrap_or(0.0).clamp(0.0, 1.0);
    let max_nodes = params.max_nodes.unwrap_or(DEFAULT_MAX_NODES).min(MAX_MAX_NODES) as usize;

    let view = build_graph_view(idx, cache, kinds, min_conf, algo, params.focus, max_nodes)?;

    let mut comms: AHashSet<u32> = AHashSet::new();
    for node in &view.nodes {
        comms.insert(node.community);
    }
    let node_count = view.nodes.len() as u32;
    let edge_count = view.edges.len() as u32;
    let community_count = comms.len() as u32;
    let truncated = view.truncated;
    let content = graph_view::render(&view, format);

    json_result(&GraphExportResponse {
        format: format.as_str().to_string(),
        content,
        node_count,
        edge_count,
        community_count,
        truncated,
        notice,
        elapsed_us: elapsed_us(started),
    })
}
