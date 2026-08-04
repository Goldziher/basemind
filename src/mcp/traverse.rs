//! Bounded, deterministic traversal over the shared code-graph (ADR-0003).
//!
//! Three shapes read the [`CodeGraph`](super::codegraph::CodeGraph) built by
//! [`codegraph::build`](super::codegraph::build):
//!
//! - [`neighbors`] — N-hop expansion outward from a set of root nodes, filtered by edge
//!   kind, direction, and a minimum-confidence floor.
//! - [`shortest_path`] — a confidence-weighted shortest path between two symbol sets, so a
//!   proven (EXTRACTED) edge is preferred over an inferred one of equal hop length.
//! - [`subgraph`] — the neighborhood around a root, cut to the significant head by local
//!   centrality so the result is a readable subgraph rather than a dump.
//!
//! This module is pure graph machinery: it operates on interned node ids over a prebuilt
//! [`Adjacency`] and never touches the store or the L1 cache — resolving names to nodes and
//! describing nodes for the response is the caller's job (see `helpers_traverse`). Edge cost
//! is an integer on the provenance ladder ([`edge_cost`]) so ordering is exact and
//! deterministic — no floating-point comparison in the hot path.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::Arc;

use ahash::{AHashMap, AHashSet};

use super::codegraph::{CodeEdge, CodeGraph, EdgeKind, EdgeKindSet, NodeKey, Provenance};

/// Traversal direction over the directed graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dir {
    /// Follow edges forward (from → to): what this node reaches.
    Out,
    /// Follow edges backward (to → from): what reaches this node.
    In,
    /// Both directions — the undirected neighborhood.
    Both,
}

impl Dir {
    /// Parse the tool `direction` param. Accepts the call-graph synonyms so an agent used to
    /// `call_graph` can reuse its vocabulary.
    pub(crate) fn parse(s: &str) -> Option<Dir> {
        match s {
            "out" | "callees" | "downstream" => Some(Dir::Out),
            "in" | "callers" | "upstream" => Some(Dir::In),
            "both" | "all" | "undirected" => Some(Dir::Both),
            _ => None,
        }
    }
}

/// Integer cost of crossing an edge, on the provenance ladder (ADR-0002): a proven edge is
/// cheaper than an inferred one, which is cheaper than an ambiguous one. Every cost is `>=
/// 10`, so hop count dominates and confidence breaks ties between equal-length routes.
pub(crate) fn edge_cost(provenance: Provenance) -> u32 {
    match provenance {
        Provenance::Extracted => 10,
        Provenance::Inferred => 15,
        Provenance::Ambiguous => 18,
    }
}

/// One incident edge in the interned adjacency. `other` is the node at the far end; the edge
/// always points `from → to` in the original graph regardless of which direction we walked.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AdjEdge {
    pub(crate) other: u32,
    pub(crate) kind: EdgeKind,
    pub(crate) provenance: Provenance,
    pub(crate) weight: u32,
}

/// An interned, bidirectional adjacency view over a built [`CodeGraph`]. Node ids are dense
/// `u32` indices into `nodes`; `out[id]` and `inc[id]` hold the forward/backward incident
/// edges. Built once per query and discarded.
pub(crate) struct Adjacency {
    // Each node is stored once behind an `Arc` shared between `nodes` and `index_of`, so interning
    // a fresh node deep-clones the (heap `RelPath`) key only once and both sides hold a refcount.
    nodes: Vec<Arc<NodeKey>>,
    index_of: AHashMap<Arc<NodeKey>, u32>,
    out: Vec<Vec<AdjEdge>>,
    inc: Vec<Vec<AdjEdge>>,
}

impl Adjacency {
    /// Intern every node in `graph.edges` and build forward/backward adjacency lists.
    /// Iteration follows `graph.edges`, which `codegraph::build` already sorts, so the
    /// resulting order is deterministic.
    pub(crate) fn build(graph: &CodeGraph) -> Self {
        Self::build_from_edges(&graph.edges)
    }

    /// Build the adjacency directly from an edge slice. Lets a caller that applies a per-call
    /// confidence filter feed the memoized graph's edges by reference — borrowing the whole set
    /// when no filter is active — instead of cloning them into a throwaway [`CodeGraph`].
    pub(crate) fn build_from_edges(edges: &[CodeEdge]) -> Self {
        let mut adj = Adjacency {
            nodes: Vec::new(),
            index_of: AHashMap::new(),
            out: Vec::new(),
            inc: Vec::new(),
        };
        for e in edges {
            let from = adj.intern(&e.from);
            let to = adj.intern(&e.to);
            adj.out[from as usize].push(AdjEdge {
                other: to,
                kind: e.kind,
                provenance: e.provenance,
                weight: e.weight,
            });
            adj.inc[to as usize].push(AdjEdge {
                other: from,
                kind: e.kind,
                provenance: e.provenance,
                weight: e.weight,
            });
        }
        adj
    }

    /// Intern a node, allocating a fresh id (with empty adjacency lists) on first sight.
    /// Callers use it to add an isolated root that carries no edges.
    pub(crate) fn intern(&mut self, key: &NodeKey) -> u32 {
        if let Some(&id) = self.index_of.get(key) {
            return id;
        }
        let id = self.nodes.len() as u32;
        let k = Arc::new(key.clone());
        self.nodes.push(Arc::clone(&k));
        self.out.push(Vec::new());
        self.inc.push(Vec::new());
        self.index_of.insert(k, id);
        id
    }

    /// The id of an already-interned node, or `None` if the node is absent from the graph.
    pub(crate) fn id(&self, key: &NodeKey) -> Option<u32> {
        self.index_of.get(key).copied()
    }

    /// The node behind an id.
    pub(crate) fn node(&self, id: u32) -> &NodeKey {
        self.nodes[id as usize].as_ref()
    }

    /// Number of interned nodes.
    pub(crate) fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Undirected incident edges of `id` for community detection: `(other_id, weight)` over both
    /// directions, unfiltered by kind/confidence. Weight folds provenance confidence in
    /// (`edge.weight * round(confidence * 10)`) so a proven edge pulls harder than an ambiguous
    /// one. Parallel edges are yielded separately; callers that need per-neighbor totals sum them.
    pub(crate) fn undirected_weighted(&self, id: u32) -> impl Iterator<Item = (u32, u64)> + '_ {
        let weigh = |e: &AdjEdge| e.weight as u64 * (e.provenance.confidence() * 10.0).round() as u64;
        self.out[id as usize]
            .iter()
            .map(move |e| (e.other, weigh(e)))
            .chain(self.inc[id as usize].iter().map(move |e| (e.other, weigh(e))))
    }

    /// Incident edges of `id` in traversal direction `dir`, filtered to the selected `kinds`
    /// and to edges whose confidence is at least `min_conf`. Each yielded tuple is the
    /// *directed* edge as it appears in the graph — `(from_id, to_id, AdjEdge)` — so callers
    /// record faithful direction even when walking `In`.
    fn incident<'a>(
        &'a self,
        id: u32,
        dir: Dir,
        kinds: EdgeKindSet,
        min_conf: f32,
    ) -> impl Iterator<Item = (u32, u32, AdjEdge)> + 'a {
        let forward = matches!(dir, Dir::Out | Dir::Both);
        let backward = matches!(dir, Dir::In | Dir::Both);
        let out = if forward { self.out[id as usize].as_slice() } else { &[] };
        let inc = if backward {
            self.inc[id as usize].as_slice()
        } else {
            &[]
        };
        out.iter()
            .map(move |e| (id, e.other, *e))
            .chain(inc.iter().map(move |e| (e.other, id, *e)))
            .filter(move |(_, _, e)| kinds.contains_kind(e.kind) && e.provenance.confidence() >= min_conf)
    }
}

/// An edge in a traversal result: endpoints as node ids plus the edge's typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WalkEdge {
    pub(crate) from: u32,
    pub(crate) to: u32,
    pub(crate) kind: EdgeKind,
    pub(crate) provenance: Provenance,
    pub(crate) weight: u32,
}

/// A bounded neighborhood walk: the reached nodes (with hop depth from the nearest root) and
/// the edges among them.
#[derive(Debug, Default)]
pub(crate) struct Walk {
    /// `(node_id, depth)` in discovery order; roots are depth 0 and come first.
    pub(crate) nodes: Vec<(u32, u32)>,
    pub(crate) edges: Vec<WalkEdge>,
    /// True when `max_nodes` capped discovery before the neighborhood was exhausted.
    pub(crate) truncated: bool,
}

/// Bounds shared by the neighborhood walks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bounds {
    pub(crate) depth: u32,
    pub(crate) max_nodes: usize,
    pub(crate) min_conf: f32,
}

/// N-hop expansion from `roots`, following `dir` over the selected `kinds`. Deterministic:
/// BFS in adjacency order (itself derived from the sorted graph), edges deduplicated by
/// `(from, to, kind)`. Stops adding *non-root* nodes once `max_nodes` is reached and flags
/// `truncated`. Roots are admitted unconditionally (they must be present), so a name resolving to
/// more than `max_nodes` definition sites can exceed the cap — bounded only by the repo's symbol count.
pub(crate) fn neighbors(adj: &Adjacency, roots: &[u32], dir: Dir, kinds: EdgeKindSet, bounds: Bounds) -> Walk {
    let mut walk = Walk::default();
    let mut depth_of: AHashMap<u32, u32> = AHashMap::new();
    let mut seen_edges: AHashSet<(u32, u32, EdgeKind)> = AHashSet::new();
    let mut queue: VecDeque<u32> = VecDeque::new();

    for &r in roots {
        if depth_of.contains_key(&r) {
            continue;
        }
        depth_of.insert(r, 0);
        walk.nodes.push((r, 0));
        queue.push_back(r);
    }

    // Phase 1 — discover the node set by BFS, bounded by depth and `max_nodes`. Only nodes are
    // admitted here; edges are collected in phase 2 once the set is final, so the result never
    // references a node the cap excluded (the `Walk` contract: edges only among returned nodes).
    while let Some(id) = queue.pop_front() {
        let depth = depth_of[&id];
        if depth >= bounds.depth {
            continue;
        }
        for (_, _, e) in adj.incident(id, dir, kinds, bounds.min_conf) {
            if depth_of.contains_key(&e.other) {
                continue;
            }
            if walk.nodes.len() >= bounds.max_nodes {
                walk.truncated = true;
                continue;
            }
            depth_of.insert(e.other, depth + 1);
            walk.nodes.push((e.other, depth + 1));
            queue.push_back(e.other);
        }
    }

    // Phase 2 — induced edges: every selected edge between two admitted nodes, including edges
    // among the outermost (max-depth) ring that phase 1's expansion never visits. Deduped by
    // `(from, to, kind)`; iteration follows the deterministic node-discovery order.
    for &(id, _) in &walk.nodes {
        for (from, to, e) in adj.incident(id, dir, kinds, bounds.min_conf) {
            if !depth_of.contains_key(&e.other) {
                continue;
            }
            if seen_edges.insert((from, to, e.kind)) {
                walk.edges.push(WalkEdge {
                    from,
                    to,
                    kind: e.kind,
                    provenance: e.provenance,
                    weight: e.weight,
                });
            }
        }
    }
    walk
}

/// A confidence-weighted shortest path: node ids from a source to a target, the edges that
/// connect them in order, and the total integer cost ([`edge_cost`] summed).
#[derive(Debug)]
pub(crate) struct Path {
    pub(crate) nodes: Vec<u32>,
    pub(crate) edges: Vec<WalkEdge>,
    pub(crate) cost: u32,
}

/// Confidence-weighted shortest directed path from any node in `sources` to any node in
/// `targets`, following forward edges over the selected `kinds`. Integer Dijkstra with a
/// deterministic `(cost, node_id)` tie-break; edges below `min_conf` are excluded. Returns
/// `None` when no target is reachable. `scan_cap` bounds the number of node relaxations so a
/// hub root cannot trigger unbounded work.
pub(crate) fn shortest_path(
    adj: &Adjacency,
    sources: &[u32],
    targets: &AHashSet<u32>,
    kinds: EdgeKindSet,
    min_conf: f32,
    scan_cap: usize,
) -> Option<Path> {
    let mut dist: AHashMap<u32, u32> = AHashMap::new();
    // prev[node] = (predecessor_id, edge into node)
    let mut prev: AHashMap<u32, (u32, WalkEdge)> = AHashMap::new();
    let mut heap: BinaryHeap<Reverse<(u32, u32)>> = BinaryHeap::new();

    for &s in sources {
        if dist.get(&s).is_none_or(|&d| d > 0) {
            dist.insert(s, 0);
            heap.push(Reverse((0, s)));
        }
    }

    let mut relaxations = 0usize;
    while let Some(Reverse((d, id))) = heap.pop() {
        if d > *dist.get(&id).unwrap_or(&u32::MAX) {
            continue; // stale heap entry
        }
        if targets.contains(&id) {
            return Some(reconstruct(id, &prev));
        }
        relaxations += 1;
        if relaxations > scan_cap {
            break;
        }
        // Forward edges only — `Out` — in adjacency order for a deterministic tie-break.
        for (from, to, e) in adj.incident(id, Dir::Out, kinds, min_conf) {
            let nd = d.saturating_add(edge_cost(e.provenance));
            if nd < *dist.get(&to).unwrap_or(&u32::MAX) {
                dist.insert(to, nd);
                prev.insert(
                    to,
                    (
                        from,
                        WalkEdge {
                            from,
                            to,
                            kind: e.kind,
                            provenance: e.provenance,
                            weight: e.weight,
                        },
                    ),
                );
                heap.push(Reverse((nd, to)));
            }
        }
    }
    None
}

/// Walk `prev` back from `target` to a source, producing the path in forward order.
fn reconstruct(target: u32, prev: &AHashMap<u32, (u32, WalkEdge)>) -> Path {
    let mut nodes = vec![target];
    let mut edges: Vec<WalkEdge> = Vec::new();
    let mut cost = 0u32;
    let mut cur = target;
    while let Some(&(pred, edge)) = prev.get(&cur) {
        edges.push(edge);
        cost = cost.saturating_add(edge_cost(edge.provenance));
        nodes.push(pred);
        cur = pred;
    }
    nodes.reverse();
    edges.reverse();
    Path { nodes, edges, cost }
}

/// A subgraph result: the kept nodes with their centrality score, and the induced edges.
#[derive(Debug, Default)]
pub(crate) struct Subgraph {
    /// `(node_id, score)` sorted by descending score; roots are always kept.
    pub(crate) nodes: Vec<(u32, u64)>,
    pub(crate) edges: Vec<WalkEdge>,
    pub(crate) truncated: bool,
}

/// The neighborhood around `roots` cut to the `max_keep` most central nodes. Gathers the
/// `Both`-direction neighborhood within `bounds.depth`, scores each node by local weighted
/// centrality (incident `weight * confidence`, integer), always keeps the roots, keeps the
/// top `max_keep` by score, and returns the edges induced among the kept set. Deterministic:
/// ties break by node id.
pub(crate) fn subgraph(
    adj: &Adjacency,
    roots: &[u32],
    kinds: EdgeKindSet,
    bounds: Bounds,
    max_keep: usize,
) -> Subgraph {
    let walk = neighbors(adj, roots, Dir::Both, kinds, bounds);
    if walk.nodes.is_empty() {
        return Subgraph::default();
    }

    // Local weighted centrality: sum of `weight * round(confidence*10)` over incident edges ~keep
    // within the gathered neighborhood. Integer, so ranking is exact. ~keep
    let mut score: AHashMap<u32, u64> = walk.nodes.iter().map(|&(id, _)| (id, 0u64)).collect();
    for e in &walk.edges {
        let w = e.weight as u64 * (e.provenance.confidence() * 10.0).round() as u64;
        if let Some(s) = score.get_mut(&e.from) {
            *s += w;
        }
        if let Some(s) = score.get_mut(&e.to) {
            *s += w;
        }
    }

    let root_set: AHashSet<u32> = roots.iter().copied().collect();
    let mut ranked: Vec<(u32, u64)> = score.into_iter().collect();
    // Roots first, then by descending score, then by id for determinism.
    ranked.sort_by(|a, b| {
        let ra = root_set.contains(&a.0);
        let rb = root_set.contains(&b.0);
        rb.cmp(&ra).then_with(|| b.1.cmp(&a.1)).then_with(|| a.0.cmp(&b.0))
    });

    // Never cut below the root set — the contract guarantees roots are always kept, even when
    // more names resolve to roots than `max_keep`.
    let keep = max_keep.max(root_set.len());
    let truncated = walk.truncated || ranked.len() > keep;
    ranked.truncate(keep);
    let kept: AHashSet<u32> = ranked.iter().map(|&(id, _)| id).collect();
    let edges: Vec<WalkEdge> = walk
        .edges
        .into_iter()
        .filter(|e| kept.contains(&e.from) && kept.contains(&e.to))
        .collect();

    Subgraph {
        nodes: ranked,
        edges,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(name: &str) -> NodeKey {
        NodeKey::Name(name.to_string())
    }

    fn edge(from: &str, to: &str, kind: EdgeKind, prov: Provenance, weight: u32) -> CodeEdge {
        CodeEdge {
            from: n(from),
            to: n(to),
            kind,
            provenance: prov,
            weight,
        }
    }

    /// A → B → C → D chain of calls, plus a low-confidence A → D shortcut, and an unrelated
    /// import edge E → B. Enough to exercise direction, depth, path weighting, and filtering.
    fn chain_graph() -> CodeGraph {
        CodeGraph {
            edges: vec![
                edge("A", "B", EdgeKind::Calls, Provenance::Extracted, 1),
                edge("B", "C", EdgeKind::Calls, Provenance::Extracted, 1),
                edge("C", "D", EdgeKind::Calls, Provenance::Extracted, 1),
                edge("A", "D", EdgeKind::Calls, Provenance::Ambiguous, 1),
                edge("E", "B", EdgeKind::Imports, Provenance::Inferred, 1),
            ],
            truncated: false,
        }
    }

    fn id_of(adj: &Adjacency, name: &str) -> u32 {
        adj.id(&n(name)).expect("node present")
    }

    fn names(adj: &Adjacency, ids: &[u32]) -> Vec<String> {
        ids.iter()
            .map(|&i| match adj.node(i) {
                NodeKey::Name(s) => s.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    const ALL: EdgeKindSet = EdgeKindSet {
        calls: true,
        imports: true,
        inherits: true,
        contains: true,
        annotates: false,
        cites: false,
    };
    const CALLS: EdgeKindSet = EdgeKindSet {
        calls: true,
        imports: false,
        inherits: false,
        contains: false,
        annotates: false,
        cites: false,
    };

    #[test]
    fn dir_parse_accepts_synonyms() {
        assert_eq!(Dir::parse("out"), Some(Dir::Out));
        assert_eq!(Dir::parse("callees"), Some(Dir::Out));
        assert_eq!(Dir::parse("in"), Some(Dir::In));
        assert_eq!(Dir::parse("callers"), Some(Dir::In));
        assert_eq!(Dir::parse("both"), Some(Dir::Both));
        assert_eq!(Dir::parse("bogus"), None);
    }

    #[test]
    fn edge_cost_follows_confidence_ladder() {
        assert!(edge_cost(Provenance::Extracted) < edge_cost(Provenance::Inferred));
        assert!(edge_cost(Provenance::Inferred) < edge_cost(Provenance::Ambiguous));
    }

    #[test]
    fn neighbors_out_respects_depth() {
        let g = chain_graph();
        let adj = Adjacency::build(&g);
        let a = id_of(&adj, "A");
        let walk = neighbors(
            &adj,
            &[a],
            Dir::Out,
            CALLS,
            Bounds {
                depth: 1,
                max_nodes: 100,
                min_conf: 0.0,
            },
        );
        let mut reached = names(&adj, &walk.nodes.iter().map(|&(id, _)| id).collect::<Vec<_>>());
        reached.sort();
        // depth 1 from A over calls: B (A→B) and D (A→D shortcut). Not C (2 hops).
        assert_eq!(reached, vec!["A", "B", "D"]);
    }

    #[test]
    fn neighbors_in_walks_backward() {
        let g = chain_graph();
        let adj = Adjacency::build(&g);
        let c = id_of(&adj, "C");
        let walk = neighbors(
            &adj,
            &[c],
            Dir::In,
            CALLS,
            Bounds {
                depth: 2,
                max_nodes: 100,
                min_conf: 0.0,
            },
        );
        let mut reached = names(&adj, &walk.nodes.iter().map(|&(id, _)| id).collect::<Vec<_>>());
        reached.sort();
        // who reaches C within 2 hops backward over calls: B (B→C), A (A→B→C).
        assert_eq!(reached, vec!["A", "B", "C"]);
    }

    #[test]
    fn neighbors_min_confidence_prunes_edges() {
        let g = chain_graph();
        let adj = Adjacency::build(&g);
        let a = id_of(&adj, "A");
        // floor above ambiguous (0.2) drops the A→D shortcut; only the extracted A→B remains.
        let walk = neighbors(
            &adj,
            &[a],
            Dir::Out,
            CALLS,
            Bounds {
                depth: 1,
                max_nodes: 100,
                min_conf: 0.5,
            },
        );
        let mut reached = names(&adj, &walk.nodes.iter().map(|&(id, _)| id).collect::<Vec<_>>());
        reached.sort();
        assert_eq!(reached, vec!["A", "B"]);
    }

    #[test]
    fn neighbors_kind_filter_selects_lane() {
        let g = chain_graph();
        let adj = Adjacency::build(&g);
        let b = id_of(&adj, "B");
        // Both directions, imports lane only: E→B is the sole imports edge touching B.
        let walk = neighbors(
            &adj,
            &[b],
            Dir::Both,
            EdgeKindSet {
                calls: false,
                imports: true,
                inherits: false,
                contains: false,
                annotates: false,
                cites: false,
            },
            Bounds {
                depth: 1,
                max_nodes: 100,
                min_conf: 0.0,
            },
        );
        let mut reached = names(&adj, &walk.nodes.iter().map(|&(id, _)| id).collect::<Vec<_>>());
        reached.sort();
        assert_eq!(reached, vec!["B", "E"]);
    }

    #[test]
    fn neighbors_max_nodes_truncates() {
        let g = chain_graph();
        let adj = Adjacency::build(&g);
        let a = id_of(&adj, "A");
        let walk = neighbors(
            &adj,
            &[a],
            Dir::Both,
            ALL,
            Bounds {
                depth: 10,
                max_nodes: 2,
                min_conf: 0.0,
            },
        );
        assert!(walk.truncated, "capping node discovery flags truncated");
        assert_eq!(walk.nodes.len(), 2);
    }

    #[test]
    fn shortest_path_prefers_confidence_over_the_ambiguous_shortcut() {
        let g = chain_graph();
        let adj = Adjacency::build(&g);
        let a = id_of(&adj, "A");
        let d = id_of(&adj, "D");
        let targets: AHashSet<u32> = [d].into_iter().collect();
        let path = shortest_path(&adj, &[a], &targets, CALLS, 0.0, 10_000).expect("A reaches D");
        // Two routes A→D: the 1-hop ambiguous shortcut (cost 18) and A→B→C→D (3×10=30).
        // Fewer hops still wins here — the shortcut is the shortest.
        assert_eq!(names(&adj, &path.nodes), vec!["A", "D"]);
        assert_eq!(path.cost, edge_cost(Provenance::Ambiguous));
    }

    #[test]
    fn shortest_path_confidence_floor_forces_the_long_route() {
        let g = chain_graph();
        let adj = Adjacency::build(&g);
        let a = id_of(&adj, "A");
        let d = id_of(&adj, "D");
        let targets: AHashSet<u32> = [d].into_iter().collect();
        // Floor above ambiguous removes the shortcut, so the only route is the extracted chain.
        let path = shortest_path(&adj, &[a], &targets, CALLS, 0.5, 10_000).expect("chain still connects");
        assert_eq!(names(&adj, &path.nodes), vec!["A", "B", "C", "D"]);
        assert_eq!(path.cost, 3 * edge_cost(Provenance::Extracted));
    }

    #[test]
    fn shortest_path_returns_none_when_unreachable() {
        let g = chain_graph();
        let adj = Adjacency::build(&g);
        let d = id_of(&adj, "D");
        let a = id_of(&adj, "A");
        let targets: AHashSet<u32> = [a].into_iter().collect();
        // D has no outgoing edges — it cannot reach A.
        assert!(shortest_path(&adj, &[d], &targets, CALLS, 0.0, 10_000).is_none());
    }

    #[test]
    fn subgraph_keeps_roots_and_cuts_to_head() {
        let g = chain_graph();
        let adj = Adjacency::build(&g);
        let b = id_of(&adj, "B");
        let sg = subgraph(
            &adj,
            &[b],
            ALL,
            Bounds {
                depth: 3,
                max_nodes: 100,
                min_conf: 0.0,
            },
            2,
        );
        assert_eq!(sg.nodes.len(), 2, "cut to max_keep");
        assert_eq!(sg.nodes[0].0, b, "root is kept and ranked first");
        assert!(sg.truncated, "cutting below the neighborhood size flags truncated");
        // Every induced edge connects two kept nodes.
        let kept: AHashSet<u32> = sg.nodes.iter().map(|&(id, _)| id).collect();
        for e in &sg.edges {
            assert!(kept.contains(&e.from) && kept.contains(&e.to));
        }
    }

    /// A → B, A → C, B → C — a triangle where B and C both sit at depth 1 from A. Exercises
    /// edges among nodes on the outermost (max-depth) ring.
    fn triangle_graph() -> CodeGraph {
        CodeGraph {
            edges: vec![
                edge("A", "B", EdgeKind::Calls, Provenance::Extracted, 1),
                edge("A", "C", EdgeKind::Calls, Provenance::Extracted, 1),
                edge("B", "C", EdgeKind::Calls, Provenance::Extracted, 1),
            ],
            truncated: false,
        }
    }

    #[test]
    fn neighbors_max_nodes_emits_no_dangling_edge() {
        let g = chain_graph();
        let adj = Adjacency::build(&g);
        let a = id_of(&adj, "A");
        let walk = neighbors(
            &adj,
            &[a],
            Dir::Both,
            ALL,
            Bounds {
                depth: 10,
                max_nodes: 2,
                min_conf: 0.0,
            },
        );
        // Every edge endpoint must be a returned node — the cap must not leave a dangling edge.
        let node_set: AHashSet<u32> = walk.nodes.iter().map(|&(id, _)| id).collect();
        for e in &walk.edges {
            assert!(
                node_set.contains(&e.from) && node_set.contains(&e.to),
                "edge {:?}→{:?} references a node cut by max_nodes",
                e.from,
                e.to
            );
        }
    }

    #[test]
    fn neighbors_emits_edges_among_frontier_peers() {
        let g = triangle_graph();
        let adj = Adjacency::build(&g);
        let a = id_of(&adj, "A");
        let b = id_of(&adj, "B");
        let c = id_of(&adj, "C");
        // Depth 1 from A: B and C are both frontier nodes. The B→C edge between them must be
        // present even though neither frontier node is expanded.
        let walk = neighbors(
            &adj,
            &[a],
            Dir::Out,
            CALLS,
            Bounds {
                depth: 1,
                max_nodes: 100,
                min_conf: 0.0,
            },
        );
        assert!(
            walk.edges.iter().any(|e| e.from == b && e.to == c),
            "the B→C edge among two depth-1 frontier peers is missing"
        );
    }

    #[test]
    fn subgraph_keeps_all_roots_even_when_over_max_keep() {
        let g = chain_graph();
        let adj = Adjacency::build(&g);
        let roots = [id_of(&adj, "A"), id_of(&adj, "B"), id_of(&adj, "C")];
        // max_keep is below the root count; every root must still survive the cut.
        let sg = subgraph(
            &adj,
            &roots,
            ALL,
            Bounds {
                depth: 1,
                max_nodes: 100,
                min_conf: 0.0,
            },
            1,
        );
        let kept: AHashSet<u32> = sg.nodes.iter().map(|&(id, _)| id).collect();
        for r in roots {
            assert!(kept.contains(&r), "root {r:?} was dropped despite the keep guarantee");
        }
    }

    #[test]
    fn shortest_path_zero_when_source_is_target() {
        let g = chain_graph();
        let adj = Adjacency::build(&g);
        let a = id_of(&adj, "A");
        let targets: AHashSet<u32> = [a].into_iter().collect();
        let path = shortest_path(&adj, &[a], &targets, CALLS, 0.0, 10_000).expect("self is reachable");
        assert_eq!(names(&adj, &path.nodes), vec!["A"]);
        assert_eq!(path.cost, 0);
    }

    #[test]
    fn shortest_path_multi_target_picks_nearest() {
        let g = chain_graph();
        let adj = Adjacency::build(&g);
        let a = id_of(&adj, "A");
        let c = id_of(&adj, "C");
        let d = id_of(&adj, "D");
        // From A: D via the 1-hop ambiguous shortcut (cost 18) beats C via A→B→C (cost 20).
        let targets: AHashSet<u32> = [c, d].into_iter().collect();
        let path = shortest_path(&adj, &[a], &targets, CALLS, 0.0, 10_000).expect("a target is reachable");
        assert_eq!(names(&adj, &path.nodes), vec!["A", "D"]);
        assert_eq!(path.cost, edge_cost(Provenance::Ambiguous));
    }

    #[test]
    fn neighbors_is_deterministic() {
        let g = chain_graph();
        let adj = Adjacency::build(&g);
        let a = id_of(&adj, "A");
        let run = || {
            let w = neighbors(
                &adj,
                &[a],
                Dir::Both,
                ALL,
                Bounds {
                    depth: 10,
                    max_nodes: 100,
                    min_conf: 0.0,
                },
            );
            w.nodes.iter().map(|&(id, d)| (id, d)).collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }
}
