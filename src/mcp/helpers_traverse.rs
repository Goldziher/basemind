//! Bodies of the graph-traversal tools `neighbors`, `path`, and `subgraph` (ADR-0003).
//!
//! Each body builds the shared typed code-graph ([`codegraph::build`]) over the requested edge
//! lanes, interns it into an [`Adjacency`], resolves the root name(s) to graph nodes, runs the
//! matching pure walk in [`super::traverse`], and describes the resulting node ids back into the
//! response payload via the L1 cache. The graph is built on demand and discarded — no persisted
//! state (ADR-0001). Work is bounded by the same scan discipline as `architecture_map`.

use ahash::{AHashMap, AHashSet};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::MapCache;
use super::codegraph::{self, BuildOpts, EdgeKindSet, NodeKey};
use super::helpers::{elapsed_us, json_result, kind_to_str};
use super::traverse::{self, Adjacency, Bounds, Dir, Path, Subgraph, Walk, WalkEdge};
use super::types::LifecycleNotice;
use super::types_traverse::{
    GraphEdge, GraphNode, NeighborsParams, NeighborsResponse, PathParams, PathResponse, SubgraphParams,
    SubgraphResponse,
};
use crate::index::IndexDb;
use crate::path::RelPath;

const DEFAULT_DEPTH: u32 = 2;
const MAX_DEPTH: u32 = 4;
const DEFAULT_MAX_NODES: u32 = 100;
const MAX_MAX_NODES: u32 = 500;
const DEFAULT_SUBGRAPH_KEEP: u32 = 30;
const MAX_SUBGRAPH_KEEP: u32 = 200;
/// Dijkstra relaxation cap for `path` — bounds work when neither endpoint connects.
const PATH_RELAX_CAP: usize = 200_000;

/// Turn the `edges` param into a lane set, validating the value (unknown lanes fail loud rather
/// than silently degrading to calls-only). Unlike `architecture_map`'s parser, `"contains"` is a
/// selectable lane here; `include_contains` forces the containment lane on regardless (for
/// `path`'s `include_contains`). `"all"` is calls+imports+inherits — containment stays opt-in.
pub(super) fn kinds_from(edges: &str, include_contains: bool) -> Result<EdgeKindSet, McpError> {
    let (calls, imports, inherits, contains) = match edges {
        "all" => (true, true, true, false),
        "calls" => (true, false, false, false),
        "imports" => (false, true, false, false),
        "inherits" => (false, false, true, false),
        "both" => (true, true, false, false),
        "contains" => (false, false, false, true),
        other => {
            return Err(McpError::invalid_params(
                format!("edges must be all/calls/imports/inherits/both/contains, got {other:?}"),
                None,
            ));
        }
    };
    Ok(EdgeKindSet {
        calls,
        imports,
        inherits,
        contains: contains || include_contains,
    })
}

/// Resolve a symbol name (optionally pinned to one path) to its definition-site node keys.
/// Uses the authoritative L1 cache, so a symbol with no graph edges still resolves to a node.
fn resolve_roots(cache: &MapCache, name: &str, path_filter: Option<&RelPath>) -> Vec<NodeKey> {
    let mut roots: Vec<NodeKey> = Vec::new();
    for (path, l1) in &cache.by_path {
        if let Some(pf) = path_filter
            && pf != path
        {
            continue;
        }
        for sym in &l1.symbols {
            if sym.name == name {
                roots.push(NodeKey::Symbol {
                    path: path.clone(),
                    start_byte: sym.start_byte,
                });
            }
        }
    }
    roots
}

/// Describe a node key for the response — resolving a `Symbol` back to its name/kind/location
/// via the cache, a `File` to its basename, and a virtual `Name` to an `external` node.
pub(super) fn describe(cache: &MapCache, key: &NodeKey) -> GraphNode {
    match key {
        NodeKey::Symbol { path, start_byte } => {
            let found = cache
                .by_path
                .get(path)
                .and_then(|l1| l1.symbols.iter().find(|s| s.start_byte == *start_byte));
            match found {
                Some(sym) => GraphNode {
                    name: sym.name.clone(),
                    kind: kind_to_str(sym.kind).to_string(),
                    path: Some(path.clone()),
                    start_row: Some(sym.start_row),
                    start_col: Some(sym.start_col),
                    depth: None,
                    centrality: None,
                },
                None => GraphNode {
                    name: String::new(),
                    kind: "symbol".to_string(),
                    path: Some(path.clone()),
                    start_row: None,
                    start_col: None,
                    depth: None,
                    centrality: None,
                },
            }
        }
        NodeKey::File { path } => GraphNode {
            name: path
                .as_str()
                .map(|s| s.rsplit('/').next().unwrap_or(s).to_string())
                .unwrap_or_default(),
            kind: "file".to_string(),
            path: Some(path.clone()),
            start_row: None,
            start_col: None,
            depth: None,
            centrality: None,
        },
        NodeKey::Name(name) => GraphNode {
            name: name.clone(),
            kind: "external".to_string(),
            path: None,
            start_row: None,
            start_col: None,
            depth: None,
            centrality: None,
        },
    }
}

/// Build the shared code-graph over `kinds` and intern it into an adjacency. Returns the
/// adjacency plus whether the underlying call scan was truncated.
fn build_adjacency(idx: Option<&IndexDb>, cache: &MapCache, kinds: EdgeKindSet) -> Result<(Adjacency, bool), McpError> {
    let graph = codegraph::build(
        idx,
        cache,
        &BuildOpts {
            kinds,
            focus: None,
            scan_cap: codegraph::CODEGRAPH_SCAN_CAP,
        },
    )?;
    let truncated = graph.truncated;
    Ok((Adjacency::build(&graph), truncated))
}

/// Render an adjacency id → response index map for `ids` (in given order), then emit the
/// `GraphNode`s. Edges are remapped in the callers via [`remap_edges`].
fn index_nodes(adj: &Adjacency, cache: &MapCache, ids: &[u32]) -> (AHashMap<u32, u32>, Vec<GraphNode>) {
    let mut index_of: AHashMap<u32, u32> = AHashMap::new();
    let mut nodes: Vec<GraphNode> = Vec::with_capacity(ids.len());
    for &id in ids {
        if index_of.contains_key(&id) {
            continue;
        }
        index_of.insert(id, nodes.len() as u32);
        nodes.push(describe(cache, adj.node(id)));
    }
    (index_of, nodes)
}

/// Remap walk edges (adjacency ids) to response `GraphEdge`s, dropping any edge whose
/// endpoints are not both in the kept node set.
fn remap_edges(edges: &[WalkEdge], index_of: &AHashMap<u32, u32>) -> Vec<GraphEdge> {
    let mut out: Vec<GraphEdge> = Vec::new();
    for e in edges {
        let (Some(&from), Some(&to)) = (index_of.get(&e.from), index_of.get(&e.to)) else {
            continue;
        };
        out.push(GraphEdge {
            from,
            to,
            kind: e.kind.as_str().to_string(),
            provenance: e.provenance.as_str().to_string(),
            confidence: e.provenance.confidence(),
            weight: e.weight,
        });
    }
    out
}

/// `neighbors` — N-hop expansion around a symbol.
pub(super) fn run_neighbors(
    idx: Option<&IndexDb>,
    cache: &MapCache,
    params: NeighborsParams,
    notice: Option<LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let dir = Dir::parse(&params.direction).ok_or_else(|| {
        McpError::invalid_params(
            format!("direction must be out/in/both, got {:?}", params.direction),
            None,
        )
    })?;
    let depth = params.depth.unwrap_or(DEFAULT_DEPTH).min(MAX_DEPTH);
    let max_nodes = params.max_nodes.unwrap_or(DEFAULT_MAX_NODES).min(MAX_MAX_NODES) as usize;
    let min_conf = params.min_confidence.unwrap_or(0.0).clamp(0.0, 1.0);
    let kinds = kinds_from(&params.edges, false)?;

    let (mut adj, scan_truncated) = build_adjacency(idx, cache, kinds)?;
    let roots: Vec<u32> = resolve_roots(cache, &params.name, params.path.as_ref())
        .iter()
        .map(|k| adj.intern(k))
        .collect();

    let walk: Walk = traverse::neighbors(
        &adj,
        &roots,
        dir,
        kinds,
        Bounds {
            depth,
            max_nodes,
            min_conf,
        },
    );

    let ids: Vec<u32> = walk.nodes.iter().map(|&(id, _)| id).collect();
    let (index_of, mut nodes) = index_nodes(&adj, cache, &ids);
    for &(id, d) in &walk.nodes {
        if let Some(&ri) = index_of.get(&id) {
            nodes[ri as usize].depth = Some(d);
        }
    }
    let edges = remap_edges(&walk.edges, &index_of);

    json_result(&NeighborsResponse {
        root: params.name,
        direction: params.direction,
        nodes,
        edges,
        truncated: walk.truncated || scan_truncated,
        notice,
        elapsed_us: elapsed_us(started),
    })
}

/// `path` — confidence-weighted shortest route between two symbols.
pub(super) fn run_path(
    idx: Option<&IndexDb>,
    cache: &MapCache,
    params: PathParams,
    notice: Option<LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let min_conf = params.min_confidence.unwrap_or(0.0).clamp(0.0, 1.0);
    let kinds = kinds_from(&params.edges, params.include_contains)?;

    let (mut adj, scan_truncated) = build_adjacency(idx, cache, kinds)?;
    let sources: Vec<u32> = resolve_roots(cache, &params.from, params.from_path.as_ref())
        .iter()
        .map(|k| adj.intern(k))
        .collect();
    let targets: AHashSet<u32> = resolve_roots(cache, &params.to, params.to_path.as_ref())
        .iter()
        .map(|k| adj.intern(k))
        .collect();

    let found: Option<Path> = if sources.is_empty() || targets.is_empty() {
        None
    } else {
        traverse::shortest_path(&adj, &sources, &targets, kinds, min_conf, PATH_RELAX_CAP)
    };

    let (nodes, edges, cost) = match found {
        Some(path) => {
            let (index_of, nodes) = index_nodes(&adj, cache, &path.nodes);
            let edges = remap_edges(&path.edges, &index_of);
            (nodes, edges, path.cost)
        }
        None => (Vec::new(), Vec::new(), 0),
    };

    json_result(&PathResponse {
        from: params.from,
        to: params.to,
        found: !nodes.is_empty(),
        nodes,
        edges,
        cost,
        truncated: scan_truncated,
        notice,
        elapsed_us: elapsed_us(started),
    })
}

/// `subgraph` — the neighborhood around a symbol, cut to the central head.
pub(super) fn run_subgraph(
    idx: Option<&IndexDb>,
    cache: &MapCache,
    params: SubgraphParams,
    notice: Option<LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let depth = params.depth.unwrap_or(DEFAULT_DEPTH).min(MAX_DEPTH);
    let max_keep = params.max_nodes.unwrap_or(DEFAULT_SUBGRAPH_KEEP).min(MAX_SUBGRAPH_KEEP) as usize;
    let min_conf = params.min_confidence.unwrap_or(0.0).clamp(0.0, 1.0);
    let kinds = kinds_from(&params.edges, false)?;

    let (mut adj, scan_truncated) = build_adjacency(idx, cache, kinds)?;
    let roots: Vec<u32> = resolve_roots(cache, &params.name, params.path.as_ref())
        .iter()
        .map(|k| adj.intern(k))
        .collect();

    let sg: Subgraph = traverse::subgraph(
        &adj,
        &roots,
        kinds,
        Bounds {
            depth,
            max_nodes: MAX_MAX_NODES as usize,
            min_conf,
        },
        max_keep,
    );

    let ids: Vec<u32> = sg.nodes.iter().map(|&(id, _)| id).collect();
    let (index_of, mut nodes) = index_nodes(&adj, cache, &ids);
    for &(id, score) in &sg.nodes {
        if let Some(&ri) = index_of.get(&id) {
            nodes[ri as usize].centrality = Some(score);
        }
    }
    let edges = remap_edges(&sg.edges, &index_of);

    json_result(&SubgraphResponse {
        root: params.name,
        nodes,
        edges,
        truncated: sg.truncated || scan_truncated,
        notice,
        elapsed_us: elapsed_us(started),
    })
}
