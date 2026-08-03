//! Body of the `call_graph` MCP tool — re-expressed over the shared `codegraph` (ADR-0001).
//!
//! Rather than re-scanning `calls_by_callee` / `calls_by_path` itself, `call_graph` now builds
//! the calls lane of the shared, memoized [`codegraph`](super::codegraph) once (an `Arc` clone
//! when another graph tool already built it this snapshot) and **projects its resolved `Calls`
//! edges to function-name granularity** — the node model this tool has always exposed. From that
//! projection it BFS-walks in either direction:
//!
//! - `direction = "callers"` — who calls into `name`: the projected callers of each frontier name.
//! - `direction = "callees"` — what `name` calls: the projected callees of each frontier name.
//!
//! Cycle detection is name-keyed (a visited set of names); a recursive function lands at the root
//! with one self-edge.
//!
//! One deliberate consequence of reading the typed graph: a call whose callee does **not** resolve
//! to an in-repo function-like definition (an external/library call such as `println!`) carries no
//! edge in `codegraph`, so it no longer surfaces as an empty-`sites` callee node. `call_graph` now
//! reports only resolved, in-repo call relationships — consistent with the rest of the graph layer.
//! File-scope call sites (a call enclosed by no function) likewise attribute to no caller name and
//! are dropped, exactly as before.

use std::collections::VecDeque;

use ahash::{AHashMap, AHashSet};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::MapCache;
use super::codegraph::{self, BuildOpts, CodeGraph, EdgeKind, EdgeKindSet, NodeKey};
use super::helpers::{elapsed_us, json_result, kind_to_str};
use super::shared_state::SharedReadStack;
use super::types_graph::{CallGraphNode, CallGraphParams, CallGraphResponse, CallGraphSite};
use crate::extract::SymbolKind;
use crate::path::RelPath;

const MAX_DEPTH_CEILING: u32 = 6;
const MAX_NODES_CEILING: u32 = 500;
const DEFAULT_MAX_DEPTH: u32 = 3;
const DEFAULT_MAX_NODES: u32 = 100;

/// Function-like symbol kinds that can act as call-graph nodes. A call site whose enclosing
/// symbol is not one of these (e.g. a top-level expression in a Python module body) is treated
/// as file-scope and dropped — there's no parent function to attribute the call to. Shared with
/// [`codegraph`](super::codegraph), which uses the same predicate when attributing call edges.
pub(super) fn is_function_like(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor | SymbolKind::Getter | SymbolKind::Setter
    )
}

/// Entry point — builds the shared calls graph, projects it, and runs the BFS.
pub(super) fn run_call_graph(
    shared: &SharedReadStack,
    idx: Option<&crate::index::IndexDb>,
    params: CallGraphParams,
    cache: &MapCache,
    notice: Option<super::types::LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let direction = params.direction.as_str();
    let direction_owned = match direction {
        "callers" | "callees" => direction.to_string(),
        other => {
            return Err(McpError::invalid_params(
                format!("direction must be \"callers\" or \"callees\", got {other:?}"),
                None,
            ));
        }
    };
    let max_depth = params.max_depth.unwrap_or(DEFAULT_MAX_DEPTH).min(MAX_DEPTH_CEILING);
    let max_nodes = params.max_nodes.unwrap_or(DEFAULT_MAX_NODES).min(MAX_NODES_CEILING) as usize;

    // Build (or reuse) the calls lane of the shared graph over the whole repo — `focus` is left
    // unset so this shares the memo entry other graph tools use; the `path` param scopes only the
    // root node here, not the whole build.
    let graph = shared.graph(
        idx,
        cache,
        &BuildOpts {
            kinds: EdgeKindSet::from_edges_param("calls"),
            focus: None,
            scan_cap: codegraph::CODEGRAPH_SCAN_CAP,
        },
    )?;
    // `(path, start_byte)` → function-name index, built once from this snapshot and shared by the
    // projection and the (rare) callees-with-path root seed below.
    let name_index = build_name_index(cache);
    let projection = CallProjection::build(&graph, cache, &name_index);

    let outcome = if direction == "callers" {
        projection.bfs_callers(
            &params.name,
            params.path.as_ref(),
            max_depth,
            max_nodes,
            graph.truncated,
        )
    } else {
        // `path` disambiguates which root definition's body seeds the walk (depth 0 only), so its
        // callees come from just the matching sites rather than every same-named definition.
        let root_override = params
            .path
            .as_ref()
            .map(|p| root_path_callees(&graph, &name_index, &params.name, p));
        projection.bfs_callees(
            &params.name,
            params.path.as_ref(),
            root_override.as_deref(),
            max_depth,
            max_nodes,
            graph.truncated,
        )
    };

    json_result(&CallGraphResponse {
        root: params.name,
        direction: direction_owned,
        nodes: outcome.nodes,
        truncated: outcome.truncated,
        truncation_reason: outcome.truncation_reason,
        notice,
        elapsed_us: elapsed_us(started),
    })
}

struct BfsOutcome {
    nodes: Vec<CallGraphNode>,
    truncated: bool,
    truncation_reason: Option<&'static str>,
}

/// `(path, start_byte)` → function-like symbol name, borrowed from the cache. Only function-like
/// symbols are indexed, so a `Symbol` node that shares a start byte with a co-located non-function
/// symbol (a struct/class declaration at the same offset) resolves to the function — the endpoints
/// of a `Calls` edge are always function-like. An `O(1)` lookup replacing a per-edge linear scan.
type NameIndex<'c> = AHashMap<&'c RelPath, AHashMap<u32, &'c str>>;

fn build_name_index(cache: &MapCache) -> NameIndex<'_> {
    let mut index: NameIndex<'_> = AHashMap::new();
    for (path, l1) in &cache.by_path {
        for sym in &l1.symbols {
            if is_function_like(sym.kind) {
                index
                    .entry(path)
                    .or_default()
                    .entry(sym.start_byte)
                    .or_insert(sym.name.as_str());
            }
        }
    }
    index
}

/// The function-like symbol name a resolved `Symbol` node points at. `File` / `Name` nodes
/// (file-scope callers, unresolved targets) have no function name.
fn name_at<'c>(index: &NameIndex<'c>, key: &NodeKey) -> Option<&'c str> {
    if let NodeKey::Symbol { path, start_byte } = key {
        return index.get(path).and_then(|by_byte| by_byte.get(start_byte)).copied();
    }
    None
}

/// The resolved callee names invoked from the definition(s) of `root_name` located in `path` —
/// the depth-0 seed for a `callees` walk when a `path` disambiguates overloaded roots.
fn root_path_callees<'c>(
    graph: &CodeGraph,
    name_index: &NameIndex<'c>,
    root_name: &str,
    path: &RelPath,
) -> Vec<String> {
    let mut set: AHashSet<&'c str> = AHashSet::new();
    for edge in &graph.edges {
        if edge.kind != EdgeKind::Calls {
            continue;
        }
        if let NodeKey::Symbol { path: from_path, .. } = &edge.from
            && from_path == path
            && name_at(name_index, &edge.from) == Some(root_name)
            && let Some(to) = name_at(name_index, &edge.to)
        {
            set.insert(to);
        }
    }
    let mut names: Vec<String> = set.into_iter().map(str::to_string).collect();
    names.sort_unstable();
    names
}

/// The calls lane of a [`CodeGraph`] projected to function-name granularity: the node model
/// `call_graph` exposes. Adjacency lists are sorted for deterministic BFS order.
struct CallProjection {
    /// callee name → caller names (function-scope callers only).
    callers_of: AHashMap<String, Vec<String>>,
    /// caller name → resolved callee names.
    callees_of: AHashMap<String, Vec<String>>,
    /// name → every function-like definition site (a node's `sites`).
    sites_of: AHashMap<String, Vec<CallGraphSite>>,
}

impl CallProjection {
    fn build(graph: &CodeGraph, cache: &MapCache, name_index: &NameIndex) -> Self {
        let mut sites_of: AHashMap<String, Vec<CallGraphSite>> = AHashMap::new();
        for (path, l1) in &cache.by_path {
            for sym in &l1.symbols {
                if is_function_like(sym.kind) {
                    sites_of.entry(sym.name.clone()).or_default().push(CallGraphSite {
                        path: path.clone(),
                        kind: kind_to_str(sym.kind).to_string(),
                        start_row: sym.start_row,
                        start_col: sym.start_col,
                    });
                }
            }
        }
        for sites in sites_of.values_mut() {
            sites.sort_by(|a, b| {
                a.path
                    .cmp(&b.path)
                    .then(a.start_row.cmp(&b.start_row))
                    .then(a.start_col.cmp(&b.start_col))
            });
            sites.dedup_by(|a, b| a.path == b.path && a.start_row == b.start_row && a.start_col == b.start_col);
        }

        let mut callers_set: AHashMap<String, AHashSet<String>> = AHashMap::new();
        let mut callees_set: AHashMap<String, AHashSet<String>> = AHashMap::new();
        for edge in &graph.edges {
            if edge.kind != EdgeKind::Calls {
                continue;
            }
            // A `Calls` edge runs enclosing-function → resolved-definition. Both endpoints must be
            // named function-like symbols; a file-scope caller (`File` node) or a virtual target is
            // dropped — the historical call_graph attributed only to function-like symbols.
            let (Some(from_name), Some(to_name)) = (name_at(name_index, &edge.from), name_at(name_index, &edge.to))
            else {
                continue;
            };
            callers_set
                .entry(to_name.to_string())
                .or_default()
                .insert(from_name.to_string());
            callees_set
                .entry(from_name.to_string())
                .or_default()
                .insert(to_name.to_string());
        }

        CallProjection {
            callers_of: sorted_adjacency(callers_set),
            callees_of: sorted_adjacency(callees_set),
            sites_of,
        }
    }

    /// Definition sites of a root name, narrowed to `path_filter` when the caller disambiguated.
    fn root_sites(&self, name: &str, path_filter: Option<&RelPath>) -> Vec<CallGraphSite> {
        let all = self.sites_of.get(name).cloned().unwrap_or_default();
        match path_filter {
            Some(p) => all.into_iter().filter(|s| &s.path == p).collect(),
            None => all,
        }
    }

    /// Every definition site of a non-root node's name (path filtering applies to the root only).
    fn node_sites(&self, name: &str) -> Vec<CallGraphSite> {
        self.sites_of.get(name).cloned().unwrap_or_default()
    }

    /// BFS upward: who calls into `root_name`?
    fn bfs_callers(
        &self,
        root_name: &str,
        path_filter: Option<&RelPath>,
        max_depth: u32,
        max_nodes: usize,
        build_truncated: bool,
    ) -> BfsOutcome {
        let mut walk = Bfs::new(root_name, self.root_sites(root_name, path_filter), max_nodes);
        let empty: Vec<String> = Vec::new();
        while let Some((current, depth)) = walk.frontier.pop_front() {
            if depth >= max_depth {
                walk.depth_gated = true;
                continue;
            }
            let current_idx = walk.index_of[&current];
            let parents = self.callers_of.get(&current).unwrap_or(&empty);
            let mut hit_cap = false;
            for parent in parents {
                // A caller→callee edge points from the parent (caller) to `current` (callee).
                if !walk.link(parent, current_idx, depth, |n| self.node_sites(n)) {
                    hit_cap = true;
                    break;
                }
            }
            if hit_cap {
                break;
            }
        }
        walk.finish(build_truncated)
    }

    /// BFS downward: what does `root_name` call?
    fn bfs_callees(
        &self,
        root_name: &str,
        path_filter: Option<&RelPath>,
        root_override: Option<&[String]>,
        max_depth: u32,
        max_nodes: usize,
        build_truncated: bool,
    ) -> BfsOutcome {
        let mut walk = Bfs::new(root_name, self.root_sites(root_name, path_filter), max_nodes);
        let empty: Vec<String> = Vec::new();
        while let Some((current, depth)) = walk.frontier.pop_front() {
            if depth >= max_depth {
                walk.depth_gated = true;
                continue;
            }
            let current_idx = walk.index_of[&current];
            let callees: &[String] = match (depth, root_override) {
                (0, Some(seed)) => seed,
                _ => self.callees_of.get(&current).map(Vec::as_slice).unwrap_or(&empty),
            };
            let mut hit_cap = false;
            for callee in callees {
                // A caller→callee edge points from `current` (caller) to the child (callee).
                if !walk.link_child(current_idx, callee, depth, |n| self.node_sites(n)) {
                    hit_cap = true;
                    break;
                }
            }
            if hit_cap {
                break;
            }
        }
        walk.finish(build_truncated)
    }
}

/// Shared BFS scaffolding: node vec, name→index map, frontier queue, and truncation flags. The
/// two directions differ only in which end of a discovered edge is the new node.
struct Bfs {
    nodes: Vec<CallGraphNode>,
    index_of: AHashMap<String, u32>,
    frontier: VecDeque<(String, u32)>,
    max_nodes: usize,
    truncated: bool,
    truncation_reason: Option<&'static str>,
    depth_gated: bool,
}

impl Bfs {
    fn new(root_name: &str, root_sites: Vec<CallGraphSite>, max_nodes: usize) -> Self {
        let mut index_of = AHashMap::new();
        index_of.insert(root_name.to_string(), 0u32);
        let mut frontier = VecDeque::new();
        frontier.push_back((root_name.to_string(), 0u32));
        Bfs {
            nodes: vec![CallGraphNode {
                name: root_name.to_string(),
                depth: 0,
                edges_to: Vec::new(),
                sites: root_sites,
            }],
            index_of,
            frontier,
            max_nodes,
            truncated: false,
            truncation_reason: None,
            depth_gated: false,
        }
    }

    /// Intern `name` as a node at `depth + 1` (or reuse an existing one), enqueueing it on first
    /// sight. Returns its index, or `None` when a new node would exceed `max_nodes`.
    fn intern(&mut self, name: &str, depth: u32, sites: impl FnOnce(&str) -> Vec<CallGraphSite>) -> Option<u32> {
        if let Some(&idx) = self.index_of.get(name) {
            return Some(idx);
        }
        if self.nodes.len() >= self.max_nodes {
            self.truncated = true;
            self.truncation_reason = Some("max_nodes");
            return None;
        }
        let idx = self.nodes.len() as u32;
        self.nodes.push(CallGraphNode {
            name: name.to_string(),
            depth: depth + 1,
            edges_to: Vec::new(),
            sites: sites(name),
        });
        self.index_of.insert(name.to_string(), idx);
        self.frontier.push_back((name.to_string(), depth + 1));
        Some(idx)
    }

    fn add_edge(&mut self, from_idx: u32, to_idx: u32) {
        let edges = &mut self.nodes[from_idx as usize].edges_to;
        if !edges.contains(&to_idx) {
            edges.push(to_idx);
        }
    }

    /// `callers` step: `parent` (a caller) links to `current` (the callee). Returns `false` when
    /// the node budget is exhausted (caller should stop).
    fn link(
        &mut self,
        parent: &str,
        current_idx: u32,
        depth: u32,
        sites: impl FnOnce(&str) -> Vec<CallGraphSite>,
    ) -> bool {
        let current_name = &self.nodes[current_idx as usize].name;
        if parent == current_name {
            self.add_edge(current_idx, current_idx);
            return true;
        }
        match self.intern(parent, depth, sites) {
            Some(parent_idx) => {
                self.add_edge(parent_idx, current_idx);
                true
            }
            None => false,
        }
    }

    /// `callees` step: `current` (a caller) links to `child` (the callee). Returns `false` when
    /// the node budget is exhausted.
    fn link_child(
        &mut self,
        current_idx: u32,
        child: &str,
        depth: u32,
        sites: impl FnOnce(&str) -> Vec<CallGraphSite>,
    ) -> bool {
        let current_name = &self.nodes[current_idx as usize].name;
        if child == current_name {
            self.add_edge(current_idx, current_idx);
            return true;
        }
        match self.intern(child, depth, sites) {
            Some(child_idx) => {
                self.add_edge(current_idx, child_idx);
                true
            }
            None => false,
        }
    }

    /// Resolve the final truncation flag. Precedence: an exhausted node budget (already recorded)
    /// wins; then a truncated underlying graph build (`scan_cap`); then a depth-gated frontier.
    fn finish(mut self, build_truncated: bool) -> BfsOutcome {
        if self.truncation_reason.is_none() && build_truncated {
            self.truncated = true;
            self.truncation_reason = Some("scan_cap");
        }
        if self.truncation_reason.is_none() && self.depth_gated {
            self.truncated = true;
            self.truncation_reason = Some("max_depth");
        }
        BfsOutcome {
            nodes: self.nodes,
            truncated: self.truncated,
            truncation_reason: self.truncation_reason,
        }
    }
}

/// Collapse a `name → set of names` adjacency into sorted, deduplicated lists for deterministic
/// BFS traversal order (the underlying `AHashSet` iteration order is not stable).
fn sorted_adjacency(map: AHashMap<String, AHashSet<String>>) -> AHashMap<String, Vec<String>> {
    map.into_iter()
        .map(|(key, set)| {
            let mut names: Vec<String> = set.into_iter().collect();
            names.sort_unstable();
            (key, names)
        })
        .collect()
}
