//! Body of the `communities` tool (ADR-0004).
//!
//! Builds the shared typed code-graph ([`codegraph::build`]) over the requested lanes, filters it
//! to the confidence floor, interns it into an [`Adjacency`], runs the chosen detection algorithm
//! ([`community::detect`]), then groups nodes into communities, labels each deterministically
//! (dominant path prefix + most central member), and describes capped member lists back via the L1
//! cache. The graph is built on demand and discarded — no persisted state (ADR-0001).

use ahash::AHashMap;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::MapCache;
use super::codegraph::{self, BuildOpts, CodeEdge, CodeGraph};
use super::community::{self, CommunityAlgo};
use super::helpers::{elapsed_us, json_result};
use super::helpers_traverse::{describe, kinds_from};
use super::traverse::Adjacency;
use super::types::LifecycleNotice;
use super::types_community::{CommunitiesParams, CommunitiesResponse, Community};
use super::types_traverse::GraphNode;
use crate::index::IndexDb;

/// Sweep bound for both detection algorithms; they converge well inside this on real graphs.
const COMMUNITY_MAX_ITERS: u32 = 50;
const DEFAULT_MAX_COMMUNITIES: u32 = 50;
const MAX_MAX_COMMUNITIES: u32 = 200;
const DEFAULT_MEMBERS_PER: u32 = 10;
const MAX_MEMBERS_PER: u32 = 100;

/// Deterministic, LLM-free label for a community: its dominant directory prefix joined to its most
/// central member's name. `ranked` is the community's node ids, most central first.
pub(super) fn label_for(adj: &Adjacency, cache: &MapCache, ranked: &[u32]) -> String {
    let central = describe(cache, adj.node(ranked[0]));
    let central_name = if central.name.is_empty() {
        central.kind
    } else {
        central.name
    };

    // Dominant directory: most frequent parent dir over the *full* member list (not the capped
    // subset), so the label reflects the whole community; ties break to the smallest dir string
    // so it is reproducible.
    let mut dir_counts: AHashMap<String, u32> = AHashMap::new();
    for &id in ranked {
        if let Some(path) = adj.node(id).file().and_then(|p| p.as_str()) {
            let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            if !dir.is_empty() {
                *dir_counts.entry(dir.to_string()).or_insert(0) += 1;
            }
        }
    }
    let mut dirs: Vec<(String, u32)> = dir_counts.into_iter().collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    match dirs.into_iter().next() {
        Some((dir, _)) => format!("{dir} · {central_name}"),
        None => central_name,
    }
}

/// `communities` — cluster the code-graph into de-facto modules with deterministic labels.
pub(super) fn run_communities(
    idx: Option<&IndexDb>,
    cache: &MapCache,
    params: CommunitiesParams,
    notice: Option<LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
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
    let max_communities = params
        .max_communities
        .unwrap_or(DEFAULT_MAX_COMMUNITIES)
        .min(MAX_MAX_COMMUNITIES) as usize;
    let members_per = params
        .members_per_community
        .unwrap_or(DEFAULT_MEMBERS_PER)
        .min(MAX_MEMBERS_PER) as usize;

    let built = codegraph::build(
        idx,
        cache,
        &BuildOpts {
            kinds,
            focus: None,
            scan_cap: codegraph::CODEGRAPH_SCAN_CAP,
        },
    )?;
    let scan_truncated = built.truncated;
    // The confidence floor drops weak edges before detection so a community reflects only edges
    // the caller trusts.
    let edges: Vec<CodeEdge> = built
        .edges
        .into_iter()
        .filter(|e| e.provenance.confidence() >= min_conf)
        .collect();
    let edge_count = edges.len() as u32;
    let graph = CodeGraph {
        edges,
        truncated: scan_truncated,
    };
    let adj = Adjacency::build(&graph);
    let node_count = adj.node_count() as u32;

    let partition = community::detect(&adj, algo, COMMUNITY_MAX_ITERS);

    // Group node ids by community.
    let mut by_comm: Vec<Vec<u32>> = vec![Vec::new(); partition.num_communities as usize];
    for (id, &c) in partition.community_of.iter().enumerate() {
        by_comm[c as usize].push(id as u32);
    }

    let mut communities: Vec<Community> = Vec::with_capacity(by_comm.len());
    for (cid, members) in by_comm.into_iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        let mut ranked = members;
        // Most central first; id ascending as a deterministic tie-break.
        ranked.sort_by(|&a, &b| {
            partition.centrality[b as usize]
                .cmp(&partition.centrality[a as usize])
                .then(a.cmp(&b))
        });
        let size = ranked.len() as u32;
        let label = label_for(&adj, cache, &ranked);
        let member_nodes: Vec<GraphNode> = ranked
            .iter()
            .take(members_per)
            .map(|&id| {
                let mut node = describe(cache, adj.node(id));
                node.centrality = Some(partition.centrality[id as usize]);
                node
            })
            .collect();
        communities.push(Community {
            id: cid as u32,
            label,
            size,
            members: member_nodes,
        });
    }

    let total_communities = communities.len() as u32;
    let members_capped = communities.iter().any(|c| c.size as usize > c.members.len());
    // Largest first; id ascending as a deterministic tie-break.
    communities.sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id)));
    let comm_capped = communities.len() > max_communities;
    communities.truncate(max_communities);

    json_result(&CommunitiesResponse {
        communities,
        num_communities: total_communities,
        node_count,
        edge_count,
        algorithm: algo.as_str().to_string(),
        truncated: scan_truncated || comm_capped || members_capped,
        notice,
        elapsed_us: elapsed_us(started),
    })
}
