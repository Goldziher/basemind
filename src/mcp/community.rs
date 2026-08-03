//! Deterministic, LLM-free community detection over the shared code-graph (ADR-0004).
//!
//! A code graph becomes navigable once it is clustered into communities — groups that relate to
//! each other far more than to the rest of the repo, i.e. the de-facto modules. This module reads
//! the interned [`Adjacency`](super::traverse::Adjacency) built over a [`CodeGraph`], symmetrises
//! it into an undirected weighted graph (edge weight folds in provenance confidence, per ADR-0002),
//! and partitions the nodes.
//!
//! Two algorithms, both **deterministic** (the same graph yields the same partition every run — the
//! constraint a stable UI and snapshot tests need) and **LLM-free**:
//!
//! - [`CommunityAlgo::LabelPropagation`] — the default: near-linear and hot-path friendly, made
//!   reproducible by initialising each node to its own label, sweeping nodes in a fixed id order,
//!   and breaking ties toward the smallest community id (basemind's hashing is randomised, so the
//!   order must be pinned explicitly).
//! - [`CommunityAlgo::Louvain`] — the opt-in higher-quality option: local-moving modularity
//!   optimisation (the first Louvain level), again sweeping in a fixed order with deterministic
//!   tie-breaks. It optimises modularity directly, at more work than label propagation.
//!
//! The result ([`Partition`]) is a dense community id per node plus a weighted-degree centrality
//! score per node; labelling (dominant path prefix + most central member) is the caller's job in
//! `helpers_community`, since it needs the L1 cache to name a symbol.

use ahash::AHashMap;

use super::traverse::Adjacency;

/// Which detection algorithm to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommunityAlgo {
    /// Label propagation — near-linear, the default.
    LabelPropagation,
    /// Local-moving modularity optimisation (Louvain first level) — opt-in, higher quality.
    Louvain,
}

impl CommunityAlgo {
    /// Parse the tool `algorithm` param. Accepts a few spellings of each.
    pub(crate) fn parse(s: &str) -> Option<CommunityAlgo> {
        match s {
            "label_propagation" | "labelprop" | "lpa" | "label" => Some(CommunityAlgo::LabelPropagation),
            "louvain" | "modularity" => Some(CommunityAlgo::Louvain),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CommunityAlgo::LabelPropagation => "label_propagation",
            CommunityAlgo::Louvain => "louvain",
        }
    }
}

/// A community partition of the graph.
#[derive(Debug, Default)]
pub(crate) struct Partition {
    /// Dense community id (`0..num_communities`) per node id.
    pub(crate) community_of: Vec<u32>,
    /// Weighted-degree centrality per node id (sum of incident undirected weights).
    pub(crate) centrality: Vec<u64>,
    /// Number of distinct communities.
    pub(crate) num_communities: u32,
}

/// An undirected, weighted, symmetric view of the graph: `nbr[i]` holds `(neighbour, weight)`
/// pairs (aggregated, self-loops dropped, sorted by neighbour id for deterministic iteration),
/// `degree[i]` the weighted degree, and `two_m` the sum of all degrees (`2m`).
struct Undirected {
    nbr: Vec<Vec<(u32, u64)>>,
    degree: Vec<u64>,
    two_m: u64,
}

impl Undirected {
    /// Symmetrise the interned adjacency. Each pair `(i, j)` accumulates the total weight between
    /// the two nodes over both directions; self-loops are dropped (they never affect which
    /// community a node joins).
    fn build(adj: &Adjacency) -> Undirected {
        let n = adj.node_count();
        let mut maps: Vec<AHashMap<u32, u64>> = vec![AHashMap::new(); n];
        for id in 0..n as u32 {
            for (other, w) in adj.undirected_weighted(id) {
                if other == id {
                    continue;
                }
                *maps[id as usize].entry(other).or_insert(0) += w;
            }
        }
        let mut nbr: Vec<Vec<(u32, u64)>> = Vec::with_capacity(n);
        let mut degree: Vec<u64> = Vec::with_capacity(n);
        let mut two_m: u64 = 0;
        for map in maps {
            let mut row: Vec<(u32, u64)> = map.into_iter().collect();
            row.sort_by_key(|&(other, _)| other);
            let d: u64 = row.iter().map(|&(_, w)| w).sum();
            degree.push(d);
            two_m += d;
            nbr.push(row);
        }
        Undirected { nbr, degree, two_m }
    }

    fn node_count(&self) -> usize {
        self.degree.len()
    }
}

/// Compact arbitrary community labels into a dense `0..k` range, assigning ids in ascending order
/// of first appearance by node id so the numbering is deterministic. Returns the compacted labels
/// and the community count.
fn compact(labels: &[u32]) -> (Vec<u32>, u32) {
    let mut remap: AHashMap<u32, u32> = AHashMap::new();
    let mut out: Vec<u32> = Vec::with_capacity(labels.len());
    let mut next: u32 = 0;
    for &l in labels {
        let dense = *remap.entry(l).or_insert_with(|| {
            let id = next;
            next += 1;
            id
        });
        out.push(dense);
    }
    (out, next)
}

/// Deterministic weighted label propagation. Each node adopts the label carrying the most
/// neighbour weight; ties break toward the smallest label, and a node keeps its current label when
/// that label is itself among the maxima (which damps oscillation). Sweeps in ascending node order
/// until a full pass makes no change or `max_iters` is hit.
fn label_propagation(graph: &Undirected, max_iters: u32) -> Vec<u32> {
    let n = graph.node_count();
    let mut label: Vec<u32> = (0..n as u32).collect();
    for _ in 0..max_iters {
        let mut changed = false;
        for id in 0..n {
            if graph.nbr[id].is_empty() {
                continue;
            }
            let mut tally: AHashMap<u32, u64> = AHashMap::new();
            for &(other, w) in &graph.nbr[id] {
                *tally.entry(label[other as usize]).or_insert(0) += w;
            }
            let current = label[id];
            let current_w = tally.get(&current).copied().unwrap_or(0);
            // Deterministic argmax: higher weight wins; on a tie the smaller label wins. The
            // current label is only displaced by a strict weight improvement or an equal-weight
            // tie held by a smaller label id, which damps oscillation.
            let mut entries: Vec<(u32, u64)> = tally.into_iter().collect();
            entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            if let Some(&(cand, cand_w)) = entries.first()
                && cand != current
                && (cand_w > current_w || (cand_w == current_w && cand < current))
            {
                label[id] = cand;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    label
}

/// Deterministic local-moving modularity optimisation (the first Louvain level). Repeatedly sweeps
/// nodes in ascending order, moving each to the neighbouring community that maximises the standard
/// modularity gain `w_to[C] - Σtot[C] · k_i / 2m`; ties break toward the smallest community id, and
/// a node stays put unless a move strictly improves the gain. Converges when a full sweep makes no
/// move (or after `max_iters`).
fn louvain_local_moving(graph: &Undirected, max_iters: u32) -> Vec<u32> {
    let n = graph.node_count();
    let mut comm: Vec<u32> = (0..n as u32).collect();
    let mut comm_tot: Vec<u64> = graph.degree.clone();
    if graph.two_m == 0 {
        return comm;
    }
    let inv_two_m = 1.0f64 / graph.two_m as f64;

    for _ in 0..max_iters {
        let mut moved = false;
        for id in 0..n {
            if graph.nbr[id].is_empty() {
                continue;
            }
            let k_i = graph.degree[id] as f64;
            let own = comm[id];
            // Detach the node from its community before scoring candidates.
            comm_tot[own as usize] -= graph.degree[id];

            // Weight from this node into each neighbouring community.
            let mut w_to: AHashMap<u32, u64> = AHashMap::new();
            for &(other, w) in &graph.nbr[id] {
                *w_to.entry(comm[other as usize]).or_insert(0) += w;
            }

            // Score each candidate community deterministically (own community included so the
            // node can stay). Sort by community id for a stable scan.
            let mut best_comm = own;
            let mut best_gain =
                w_to.get(&own).copied().unwrap_or(0) as f64 - comm_tot[own as usize] as f64 * k_i * inv_two_m;
            let mut cands: Vec<(u32, u64)> = w_to.into_iter().collect();
            cands.sort_by_key(|&(c, _)| c);
            for (cand, w) in cands {
                let gain = w as f64 - comm_tot[cand as usize] as f64 * k_i * inv_two_m;
                if gain > best_gain {
                    best_gain = gain;
                    best_comm = cand;
                }
            }

            comm_tot[best_comm as usize] += graph.degree[id];
            if best_comm != own {
                comm[id] = best_comm;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    comm
}

/// The modularity `Q` of a partition — the fraction of edge weight inside communities minus its
/// expected value in a degree-preserving random graph. Range `(-0.5, 1]`; higher is stronger
/// community structure. Used by the tests and callers who want to compare partitions.
#[cfg(test)]
fn modularity(graph: &Undirected, comm: &[u32]) -> f64 {
    if graph.two_m == 0 {
        return 0.0;
    }
    let two_m = graph.two_m as f64;
    let mut inside: f64 = 0.0;
    for i in 0..graph.node_count() {
        for &(j, w) in &graph.nbr[i] {
            if comm[i] == comm[j as usize] {
                inside += w as f64;
            }
        }
    }
    // `inside` counts each internal edge twice (once from each endpoint) — exactly the `2·Σin` the
    // modularity numerator wants over `2m`.
    let mut expected: f64 = 0.0;
    let ncomm = comm.iter().copied().max().map(|m| m + 1).unwrap_or(0) as usize;
    let mut tot: Vec<f64> = vec![0.0; ncomm];
    for i in 0..graph.node_count() {
        tot[comm[i] as usize] += graph.degree[i] as f64;
    }
    for t in tot {
        expected += (t / two_m) * (t / two_m);
    }
    inside / two_m - expected
}

/// Detect communities over the interned graph with the chosen algorithm. Returns a dense
/// partition plus a weighted-degree centrality score per node.
pub(crate) fn detect(adj: &Adjacency, algo: CommunityAlgo, max_iters: u32) -> Partition {
    let graph = Undirected::build(adj);
    let raw = match algo {
        CommunityAlgo::LabelPropagation => label_propagation(&graph, max_iters),
        CommunityAlgo::Louvain => louvain_local_moving(&graph, max_iters),
    };
    let (community_of, num_communities) = compact(&raw);
    Partition {
        community_of,
        centrality: graph.degree,
        num_communities,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::codegraph::{CodeEdge, CodeGraph, EdgeKind, NodeKey, Provenance};
    use crate::mcp::traverse::Adjacency;

    fn n(name: &str) -> NodeKey {
        NodeKey::Name(name.to_string())
    }

    fn edge(from: &str, to: &str, prov: Provenance) -> CodeEdge {
        CodeEdge {
            from: n(from),
            to: n(to),
            kind: EdgeKind::Calls,
            provenance: prov,
            weight: 1,
        }
    }

    /// Two tight triangles (A-B-C and D-E-F) joined by a single bridge C-D. A good partition puts
    /// each triangle in its own community.
    fn two_triangles() -> CodeGraph {
        let e = Provenance::Extracted;
        CodeGraph {
            edges: vec![
                edge("A", "B", e),
                edge("B", "C", e),
                edge("C", "A", e),
                edge("D", "E", e),
                edge("E", "F", e),
                edge("F", "D", e),
                edge("C", "D", e), // the single bridge between the two triangles
            ],
            truncated: false,
        }
    }

    /// Two triangles with no edge between them — two disconnected components. Every algorithm,
    /// including label propagation, must separate these.
    fn two_components() -> CodeGraph {
        let e = Provenance::Extracted;
        CodeGraph {
            edges: vec![
                edge("A", "B", e),
                edge("B", "C", e),
                edge("C", "A", e),
                edge("D", "E", e),
                edge("E", "F", e),
                edge("F", "D", e),
            ],
            truncated: false,
        }
    }

    fn id_of(adj: &Adjacency, name: &str) -> u32 {
        adj.id(&n(name)).expect("node present")
    }

    fn same_community(p: &Partition, adj: &Adjacency, a: &str, b: &str) -> bool {
        p.community_of[id_of(adj, a) as usize] == p.community_of[id_of(adj, b) as usize]
    }

    #[test]
    fn algo_parse_accepts_synonyms() {
        assert_eq!(CommunityAlgo::parse("louvain"), Some(CommunityAlgo::Louvain));
        assert_eq!(CommunityAlgo::parse("lpa"), Some(CommunityAlgo::LabelPropagation));
        assert_eq!(
            CommunityAlgo::parse("label_propagation"),
            Some(CommunityAlgo::LabelPropagation)
        );
        assert_eq!(CommunityAlgo::parse("bogus"), None);
    }

    #[test]
    fn label_propagation_separates_disconnected_components() {
        let g = two_components();
        let adj = Adjacency::build(&g);
        let p = detect(&adj, CommunityAlgo::LabelPropagation, 20);
        // Each triangle's members share a community; the two components do not.
        assert!(same_community(&p, &adj, "A", "B"));
        assert!(same_community(&p, &adj, "A", "C"));
        assert!(same_community(&p, &adj, "D", "E"));
        assert!(same_community(&p, &adj, "D", "F"));
        assert!(
            !same_community(&p, &adj, "A", "D"),
            "disconnected components are distinct"
        );
        assert_eq!(p.num_communities, 2);
    }

    #[test]
    fn louvain_separates_the_two_triangles() {
        let g = two_triangles();
        let adj = Adjacency::build(&g);
        let p = detect(&adj, CommunityAlgo::Louvain, 20);
        assert!(same_community(&p, &adj, "A", "B"));
        assert!(same_community(&p, &adj, "D", "F"));
        assert!(!same_community(&p, &adj, "A", "D"));
        assert_eq!(p.num_communities, 2);
    }

    #[test]
    fn louvain_modularity_is_at_least_label_propagation() {
        let g = two_triangles();
        let adj = Adjacency::build(&g);
        let graph = Undirected::build(&adj);
        let lpa = compact(&label_propagation(&graph, 20)).0;
        let lou = compact(&louvain_local_moving(&graph, 20)).0;
        // On a graph with clear structure Louvain must not do worse than label propagation.
        assert!(modularity(&graph, &lou) >= modularity(&graph, &lpa) - 1e-9);
        // And the good 2-triangle partition beats lumping everything together.
        let all_one = vec![0u32; graph.node_count()];
        assert!(modularity(&graph, &lou) > modularity(&graph, &all_one));
    }

    #[test]
    fn detection_is_deterministic() {
        let g = two_triangles();
        let adj = Adjacency::build(&g);
        let run = || detect(&adj, CommunityAlgo::LabelPropagation, 20).community_of;
        assert_eq!(run(), run());
        let run_l = || detect(&adj, CommunityAlgo::Louvain, 20).community_of;
        assert_eq!(run_l(), run_l());
    }

    #[test]
    fn centrality_is_weighted_degree() {
        let g = two_triangles();
        let adj = Adjacency::build(&g);
        let p = detect(&adj, CommunityAlgo::LabelPropagation, 20);
        // C sits in a triangle *and* holds the bridge to D, so it has the top degree (3 edges); ~keep
        // A/B have 2. All extracted edges weigh 1 * round(1.0*10) = 10. ~keep
        let c = p.centrality[id_of(&adj, "C") as usize];
        let a = p.centrality[id_of(&adj, "A") as usize];
        assert_eq!(c, 30, "C has three incident extracted edges");
        assert_eq!(a, 20, "A has two");
        assert!(c > a);
    }

    #[test]
    fn empty_graph_yields_no_communities() {
        let g = CodeGraph {
            edges: vec![],
            truncated: false,
        };
        let adj = Adjacency::build(&g);
        let p = detect(&adj, CommunityAlgo::Louvain, 20);
        assert_eq!(p.num_communities, 0);
        assert!(p.community_of.is_empty());
    }
}
