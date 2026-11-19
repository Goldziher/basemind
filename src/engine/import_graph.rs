//! The file-level import graph (ADR-0047 decision 5), built at query time from the live view's
//! file-edge rows ([`IndexView::file_edges`]) and cached per workspace per `cache_generation`.
//!
//! A node is a file that imports or is imported through a resolved edge; a file the resolvers
//! reached no edge for is not a node. Node ids follow path order, so every answer built from them
//! (walk order, cycle members, the cycle list) is deterministic.
//!
//! Each edge carries the [`EdgeKinds`] of the imports behind it, and two tiers are left out of every
//! walk and the cycle search unless the caller's [`Tiers`] asks for them: type-only imports
//! (`import type`), which create cycles that do not exist at run time, and Rust `mod x;`
//! declarations, which say a parent module contains a child file, not that it uses it (with them, a
//! child's `use crate::…` closes a cycle through every parent above it). An edge holding a runtime
//! import is in the default tier whatever else it holds.
//!
//! The graph is as good as the resolvers. [`Coverage`] says, per language, how many files a
//! resolver read and how many imports it could not place, so a caller states what the graph knows:
//! a repository with no resolvable language has no edges, which is not the same as no cycles.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use ahash::AHashMap;
use parking_lot::Mutex;

use crate::engine::error::EngineError;
use crate::engine::workspace::Workspace;
use crate::index::keys::{EdgeKinds, FileEdges};
use crate::index::view::IndexView;
use crate::path::RelPath;

/// A node of the graph: an index into its path-sorted node list.
pub(crate) type NodeId = u32;

/// Importer depth a bounded walk takes when the caller names none.
pub(crate) const DEPTH_DEFAULT: u8 = 1;
/// Deepest a bounded walk goes.
pub(crate) const DEPTH_MAX: u8 = 5;
/// Nodes a bounded walk reports before it stops with [`Truncation::ScanCap`].
pub(crate) const NODE_CAP: usize = 5_000;
/// A graph larger than this is built for its caller but not kept on the workspace.
const CACHE_BYTES_MAX: usize = 128 * 1024 * 1024;

/// Which edges a graph answer follows. The default, [`Tiers::RUNTIME`], follows runtime imports
/// only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Tiers {
    /// Also follow type-only imports (the CLI's `--include-type-imports`).
    pub(crate) type_imports: bool,
    /// Also follow Rust `mod x;` declarations from a parent module to its child file (the CLI's
    /// `--include-module-decls`).
    pub(crate) module_decls: bool,
}

impl Tiers {
    /// Runtime imports only: the default of every graph answer.
    pub(crate) const RUNTIME: Self = Self {
        type_imports: false,
        module_decls: false,
    };
    /// Every edge.
    pub(crate) const ALL: Self = Self {
        type_imports: true,
        module_decls: true,
    };

    fn admits(self, edge: &Edge) -> bool {
        edge.kinds.runtime()
            || (self.type_imports && edge.kinds.contains(EdgeKinds::TYPE_ONLY))
            || (self.module_decls && edge.kinds.contains(EdgeKinds::MODULE_DECL))
    }
}

/// Which way a walk follows the edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    /// From a file to the files that import it.
    Importers,
    /// From a file to the files it imports.
    Dependencies,
}

/// One edge as seen from one of its ends: `node` is the other end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Edge {
    pub(crate) node: NodeId,
    /// Byte offset in the importer of the first import of the target (of a runtime import when
    /// there is one).
    pub(crate) local_start: u32,
    /// Every kind of import behind the edge.
    pub(crate) kinds: EdgeKinds,
}

/// Compressed adjacency: the edges of node `n` are `edges[offsets[n]..offsets[n + 1]]`, sorted by
/// the other end.
#[derive(Debug, Default)]
struct Csr {
    offsets: Vec<u32>,
    edges: Vec<Edge>,
}

impl Csr {
    fn of(&self, node: NodeId) -> &[Edge] {
        let at = node as usize;
        match (self.offsets.get(at), self.offsets.get(at + 1)) {
            (Some(&start), Some(&end)) => &self.edges[start as usize..end as usize],
            _ => &[],
        }
    }

    fn bytes(&self) -> usize {
        self.offsets.capacity() * size_of::<u32>() + self.edges.capacity() * size_of::<Edge>()
    }
}

/// What the resolvers of one language contributed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LanguageCoverage {
    /// The TSLP pack name.
    pub(crate) language: String,
    /// A resolver for the language is compiled in. Without one its files have no edges.
    pub(crate) resolver: bool,
    /// Indexed files of the language.
    pub(crate) files: u32,
    /// Files with an edge row: the resolver read at least one import of each.
    pub(crate) importers: u32,
    /// Resolved edges from the language's files, runtime and type-only.
    pub(crate) edges: u32,
    /// The part of `edges` that is type-only.
    pub(crate) type_only_edges: u32,
    /// The part of `edges` that is only a Rust `mod` declaration.
    pub(crate) module_decl_edges: u32,
    /// Imports naming a module outside the repository.
    pub(crate) external: u32,
    /// Imports naming a file of the repository that no indexed file matches.
    pub(crate) unresolved: u32,
}

/// What the graph knows, per language, sorted by language.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Coverage {
    pub(crate) languages: Vec<LanguageCoverage>,
}

impl Coverage {
    /// Languages with indexed files and no resolver, whose imports the graph cannot see.
    pub(crate) fn without_resolver(&self) -> impl Iterator<Item = &str> {
        self.languages
            .iter()
            .filter(|language| !language.resolver)
            .map(|language| language.language.as_str())
    }

    /// Whether any indexed file is in a language that has a resolver. When not, an empty graph
    /// means "not analysed", never "no edges".
    pub(crate) fn any_resolvable(&self) -> bool {
        self.languages
            .iter()
            .any(|language| language.resolver && language.files > 0)
    }
}

/// Why a bounded walk stopped before it ran out of edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Truncation {
    /// Files lie beyond the deepest level walked.
    Depth,
    /// The walk reported [`WalkLimits::node_cap`] files and more were reachable.
    ScanCap,
}

impl Truncation {
    /// The `truncated_reason` an answer carries.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Depth => "depth",
            Self::ScanCap => "scan_cap",
        }
    }
}

/// Bounds of a walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WalkLimits {
    pub(crate) depth: u8,
    pub(crate) node_cap: usize,
}

impl WalkLimits {
    /// The caller's `depth`, defaulted to [`DEPTH_DEFAULT`] and held to `1..=`[`DEPTH_MAX`], with
    /// the [`NODE_CAP`] node cap.
    pub(crate) fn new(depth: Option<u8>) -> Self {
        Self {
            depth: depth.unwrap_or(DEPTH_DEFAULT).clamp(1, DEPTH_MAX),
            node_cap: NODE_CAP,
        }
    }
}

/// A file a walk reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reached {
    pub(crate) node: NodeId,
    /// Edges from the nearest start: 1 for a direct importer or dependency.
    pub(crate) depth: u8,
    /// The node one level nearer the starts through which it was first reached.
    pub(crate) via: NodeId,
}

/// The files a bounded walk reached, by depth and then path; the starts are not among them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Walk {
    pub(crate) reached: Vec<Reached>,
    pub(crate) truncated: Option<Truncation>,
}

/// The file-level import graph of one index generation.
#[derive(Debug, Default)]
pub(crate) struct ImportGraph {
    /// Node id -> path, sorted.
    paths: Vec<RelPath>,
    forward: Csr,
    reverse: Csr,
    coverage: Coverage,
    probes_complete: bool,
}

impl ImportGraph {
    /// Build the graph from the file-edge rows of a view.
    ///
    /// `files` is every indexed file with its language, for [`Coverage`]; `has_resolver` says
    /// whether a language has a compiled-in resolver; `probes_complete` is the view's
    /// `import_probes_complete()`, without which the rows may describe only part of the index.
    pub(crate) fn build<'a>(
        rows: Vec<(RelPath, FileEdges)>,
        files: impl IntoIterator<Item = (&'a RelPath, &'a str)>,
        has_resolver: impl Fn(&str) -> bool,
        probes_complete: bool,
    ) -> Self {
        let mut by_language: AHashMap<&str, LanguageCoverage> = AHashMap::new();
        let mut language_of: AHashMap<&RelPath, &str> = AHashMap::new();
        for (path, language) in files {
            language_of.insert(path, language);
            by_language
                .entry(language)
                .or_insert_with(|| LanguageCoverage {
                    language: language.to_string(),
                    resolver: has_resolver(language),
                    ..LanguageCoverage::default()
                })
                .files += 1;
        }
        for (importer, edges) in &rows {
            let Some(coverage) = language_of
                .get(importer)
                .and_then(|language| by_language.get_mut(language))
            else {
                continue;
            };
            let only = |kind: EdgeKinds| {
                let count = edges.edges.iter().filter(|edge| edge.kinds == kind).count();
                u32::try_from(count).unwrap_or(u32::MAX)
            };
            coverage.importers += 1;
            coverage.edges += u32::try_from(edges.edges.len()).unwrap_or(u32::MAX);
            coverage.type_only_edges += only(EdgeKinds::TYPE_ONLY);
            coverage.module_decl_edges += only(EdgeKinds::MODULE_DECL);
            coverage.external += edges.external;
            coverage.unresolved += edges.unresolved;
        }
        let mut languages: Vec<LanguageCoverage> = by_language.into_values().collect();
        languages.sort_unstable_by(|a, b| a.language.cmp(&b.language));
        drop(language_of);

        let mut paths: Vec<RelPath> = rows
            .iter()
            .flat_map(|(importer, edges)| std::iter::once(importer).chain(edges.edges.iter().map(|edge| &edge.target)))
            .cloned()
            .collect();
        paths.sort_unstable();
        paths.dedup();
        paths.shrink_to_fit();
        let id = |path: &RelPath| -> NodeId {
            let at = paths.binary_search(path).unwrap_or_default();
            NodeId::try_from(at).unwrap_or(NodeId::MAX)
        };

        let mut forward_lists: Vec<(NodeId, Vec<Edge>)> = rows
            .iter()
            .map(|(importer, edges)| {
                let mut out: Vec<Edge> = edges
                    .edges
                    .iter()
                    .map(|edge| Edge {
                        node: id(&edge.target),
                        local_start: edge.local_start,
                        kinds: edge.kinds,
                    })
                    .collect();
                out.sort_unstable_by_key(|edge| edge.node);
                out.dedup_by(|later, kept| {
                    let same = later.node == kept.node;
                    if same {
                        kept.kinds = kept.kinds.union(later.kinds);
                    }
                    same
                });
                (id(importer), out)
            })
            .collect();
        drop(rows);
        forward_lists.sort_unstable_by_key(|(importer, _)| *importer);

        let nodes = paths.len();
        let mut forward = Csr {
            offsets: Vec::with_capacity(nodes + 1),
            edges: Vec::with_capacity(forward_lists.iter().map(|(_, edges)| edges.len()).sum()),
        };
        let mut lists = forward_lists.into_iter().peekable();
        for node in 0..nodes {
            forward.offsets.push(offset(forward.edges.len()));
            while let Some((_, edges)) = lists.next_if(|(importer, _)| *importer as usize == node) {
                forward.edges.extend(edges);
            }
        }
        forward.offsets.push(offset(forward.edges.len()));

        let reverse = reverse_of(&forward, nodes);
        Self {
            paths,
            forward,
            reverse,
            coverage: Coverage { languages },
            probes_complete,
        }
    }

    /// Files in the graph.
    pub(crate) fn len(&self) -> usize {
        self.paths.len()
    }

    /// Whether no file has a resolved edge.
    pub(crate) fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Resolved edges, runtime and type-only.
    pub(crate) fn edge_count(&self) -> usize {
        self.forward.edges.len()
    }

    /// The node of a repo-relative path, if the file has an edge.
    pub(crate) fn node(&self, path: &str) -> Option<NodeId> {
        self.paths
            .binary_search_by(|probe| probe.as_bytes().cmp(path.as_bytes()))
            .ok()
            .and_then(|at| NodeId::try_from(at).ok())
    }

    /// The path of `node`.
    pub(crate) fn path(&self, node: NodeId) -> &RelPath {
        &self.paths[node as usize]
    }

    /// What the graph knows, per language.
    pub(crate) fn coverage(&self) -> &Coverage {
        &self.coverage
    }

    /// Whether the resolve pass has run over the whole index at the current probe version. Until
    /// it has, an answer from the graph carries a notice or is refused.
    pub(crate) fn probes_complete(&self) -> bool {
        self.probes_complete
    }

    /// The files `node` imports, by path.
    pub(crate) fn dependencies(&self, node: NodeId, tiers: Tiers) -> impl Iterator<Item = &Edge> {
        self.forward.of(node).iter().filter(move |edge| tiers.admits(edge))
    }

    /// The files importing `node`, by path. `local_start` is the offset in the importer.
    pub(crate) fn importers(&self, node: NodeId, tiers: Tiers) -> impl Iterator<Item = &Edge> {
        self.reverse.of(node).iter().filter(move |edge| tiers.admits(edge))
    }

    /// Files importing `node`.
    pub(crate) fn in_degree(&self, node: NodeId, tiers: Tiers) -> usize {
        self.importers(node, tiers).count()
    }

    fn adjacent(&self, node: NodeId, direction: Direction, tiers: Tiers) -> impl Iterator<Item = &Edge> {
        let csr = match direction {
            Direction::Importers => &self.reverse,
            Direction::Dependencies => &self.forward,
        };
        csr.of(node).iter().filter(move |edge| tiers.admits(edge))
    }

    /// Breadth-first walk from `starts` in `direction`, at most `limits.depth` levels deep and
    /// `limits.node_cap` files wide. A file is reported once, at its nearest depth.
    pub(crate) fn walk(&self, starts: &[NodeId], direction: Direction, limits: WalkLimits, tiers: Tiers) -> Walk {
        let mut seen = vec![false; self.len()];
        let mut frontier: Vec<NodeId> = Vec::new();
        for &start in starts {
            if let Some(slot) = seen.get_mut(start as usize).filter(|slot| !**slot) {
                *slot = true;
                frontier.push(start);
            }
        }
        frontier.sort_unstable();
        let mut walk = Walk::default();
        for depth in 1..=limits.depth {
            let mut next: Vec<Reached> = Vec::new();
            for &from in &frontier {
                for edge in self.adjacent(from, direction, tiers) {
                    let slot = &mut seen[edge.node as usize];
                    if *slot {
                        continue;
                    }
                    if walk.reached.len() + next.len() >= limits.node_cap {
                        next.sort_unstable_by_key(|reached| reached.node);
                        walk.reached.extend(next);
                        walk.truncated = Some(Truncation::ScanCap);
                        return walk;
                    }
                    *slot = true;
                    next.push(Reached {
                        node: edge.node,
                        depth,
                        via: from,
                    });
                }
            }
            next.sort_unstable_by_key(|reached| reached.node);
            frontier = next.iter().map(|reached| reached.node).collect();
            walk.reached.extend(next);
            if frontier.is_empty() {
                return walk;
            }
        }
        let beyond = frontier.iter().any(|&from| {
            self.adjacent(from, direction, tiers)
                .any(|edge| !seen[edge.node as usize])
        });
        if beyond {
            walk.truncated = Some(Truncation::Depth);
        }
        walk
    }

    /// Every file reachable from `roots` in `direction`, the roots included, with no bound: a mask
    /// indexed by node.
    pub(crate) fn reachable(&self, roots: &[NodeId], direction: Direction, tiers: Tiers) -> Vec<bool> {
        let mut seen = vec![false; self.len()];
        let mut stack: Vec<NodeId> = Vec::new();
        for &root in roots {
            if let Some(slot) = seen.get_mut(root as usize).filter(|slot| !**slot) {
                *slot = true;
                stack.push(root);
            }
        }
        while let Some(node) = stack.pop() {
            for edge in self.adjacent(node, direction, tiers) {
                let slot = &mut seen[edge.node as usize];
                if !*slot {
                    *slot = true;
                    stack.push(edge.node);
                }
            }
        }
        seen
    }

    /// Whether a new import of `target` by `importer` would close a cycle: `target` already reaches
    /// `importer`, or they are one file.
    pub(crate) fn would_close_cycle(&self, importer: NodeId, target: NodeId, tiers: Tiers) -> bool {
        importer == target
            || self
                .reachable(&[target], Direction::Dependencies, tiers)
                .get(importer as usize)
                .copied()
                .unwrap_or(false)
    }

    /// The import cycles: every strongly connected component of more than one file, and every file
    /// importing itself. Members are in path order and cycles are ordered by their first member.
    /// Tarjan's algorithm, iterative, so a deep import chain cannot overflow the stack.
    pub(crate) fn cycles(&self, tiers: Tiers) -> Vec<Vec<NodeId>> {
        const UNVISITED: u32 = u32::MAX;
        let nodes = self.len();
        let mut index = vec![UNVISITED; nodes];
        let mut low = vec![0u32; nodes];
        let mut on_stack = vec![false; nodes];
        let mut stack: Vec<NodeId> = Vec::new();
        let mut calls: Vec<(NodeId, usize)> = Vec::new();
        let mut next_index = 0u32;
        let mut cycles: Vec<Vec<NodeId>> = Vec::new();

        for root in 0..nodes {
            if index[root] != UNVISITED {
                continue;
            }
            let root = NodeId::try_from(root).unwrap_or(NodeId::MAX);
            visit(root, &mut index, &mut low, &mut on_stack, &mut stack, &mut next_index);
            calls.push((root, 0));
            while let Some(top) = calls.last_mut() {
                let node = top.0;
                if let Some(edge) = self.forward.of(node).get(top.1) {
                    top.1 += 1;
                    if !tiers.admits(edge) {
                        continue;
                    }
                    let target = edge.node as usize;
                    if index[target] == UNVISITED {
                        visit(
                            edge.node,
                            &mut index,
                            &mut low,
                            &mut on_stack,
                            &mut stack,
                            &mut next_index,
                        );
                        calls.push((edge.node, 0));
                    } else if on_stack[target] {
                        low[node as usize] = low[node as usize].min(index[target]);
                    }
                    continue;
                }
                calls.pop();
                let at = node as usize;
                if low[at] == index[at] {
                    let mut component: Vec<NodeId> = Vec::new();
                    while let Some(member) = stack.pop() {
                        on_stack[member as usize] = false;
                        component.push(member);
                        if member == node {
                            break;
                        }
                    }
                    let self_loop = || self.dependencies(node, tiers).any(|edge| edge.node == node);
                    if component.len() > 1 || self_loop() {
                        component.sort_unstable();
                        cycles.push(component);
                    }
                }
                if let Some(&(parent, _)) = calls.last() {
                    low[parent as usize] = low[parent as usize].min(low[at]);
                }
            }
        }
        cycles.sort_unstable_by_key(|component| component[0]);
        cycles
    }

    /// Heap bytes the graph holds, for the cache budget and measurement.
    pub(crate) fn approx_bytes(&self) -> usize {
        let paths: usize = self
            .paths
            .iter()
            .map(|path| path.as_bytes().len() + size_of::<RelPath>())
            .sum();
        let coverage: usize = self
            .coverage
            .languages
            .iter()
            .map(|language| language.language.len() + size_of::<LanguageCoverage>())
            .sum();
        paths + self.forward.bytes() + self.reverse.bytes() + coverage
    }
}

/// Number `node` in Tarjan's visit order and push it on the component stack.
fn visit(
    node: NodeId,
    index: &mut [u32],
    low: &mut [u32],
    on_stack: &mut [bool],
    stack: &mut Vec<NodeId>,
    next_index: &mut u32,
) {
    let at = node as usize;
    index[at] = *next_index;
    low[at] = *next_index;
    *next_index += 1;
    on_stack[at] = true;
    stack.push(node);
}

fn offset(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// The reverse adjacency of `forward`, by a counting sort: each node's importers come out in node
/// order because importers are visited in node order.
fn reverse_of(forward: &Csr, nodes: usize) -> Csr {
    let mut counts = vec![0u32; nodes + 1];
    for edge in &forward.edges {
        counts[edge.node as usize + 1] += 1;
    }
    for at in 1..counts.len() {
        counts[at] += counts[at - 1];
    }
    let offsets = counts.clone();
    let mut fill = counts;
    let placeholder = Edge {
        node: 0,
        local_start: 0,
        kinds: EdgeKinds::IMPORT,
    };
    let mut edges = vec![placeholder; forward.edges.len()];
    for importer in 0..nodes {
        let importer_id = NodeId::try_from(importer).unwrap_or(NodeId::MAX);
        for edge in forward.of(importer_id) {
            let slot = &mut fill[edge.node as usize];
            edges[*slot as usize] = Edge {
                node: importer_id,
                ..*edge
            };
            *slot += 1;
        }
    }
    Csr { offsets, edges }
}

/// The workspace's graph, kept for one `cache_generation`.
#[derive(Default)]
pub(crate) struct GraphCache {
    slot: Mutex<Option<(u32, Arc<ImportGraph>)>>,
}

impl GraphCache {
    /// The graph of `generation`: the kept one, or `build`'s, which is kept when it fits
    /// [`CACHE_BYTES_MAX`] and no newer generation's graph is kept already. The lock is not held
    /// while `build` runs, so a build never waits on another caller's.
    pub(crate) fn get_or_build<E>(
        &self,
        generation: u32,
        build: impl FnOnce() -> Result<ImportGraph, E>,
    ) -> Result<Arc<ImportGraph>, E> {
        if let Some((_, graph)) = self.slot.lock().as_ref().filter(|(kept, _)| *kept == generation) {
            return Ok(Arc::clone(graph));
        }
        let graph = Arc::new(build()?);
        if graph.approx_bytes() <= CACHE_BYTES_MAX {
            let mut slot = self.slot.lock();
            if slot.as_ref().is_none_or(|(kept, _)| *kept <= generation) {
                *slot = Some((generation, Arc::clone(&graph)));
            }
        }
        Ok(graph)
    }
}

/// The import graph of `ws`'s live view at its current `cache_generation`, from the cache or built.
///
/// Takes the store read lock while building, so the caller must not hold it.
pub(crate) fn import_graph(ws: &Workspace) -> Result<Arc<ImportGraph>, EngineError> {
    let generation = ws.cache_generation.load(Ordering::Acquire);
    ws.import_graph.get_or_build(generation, || {
        let store = ws.store_read();
        let view = index_view(&store)?;
        let files = store
            .index
            .files
            .iter()
            .map(|(path, entry)| (path, entry.language.as_str()));
        Ok(ImportGraph::build(
            view.file_edges(),
            files,
            has_resolver,
            view.import_probes_complete(),
        ))
    })
}

/// Whether a resolver that writes file-edge rows is compiled in for `language`.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
fn has_resolver(language: &str) -> bool {
    crate::intel::module_resolver::ModuleResolver::supports(language)
        || crate::intel::resolver::SpecifierResolver::supports(language)
}

/// No resolver is compiled in without a code-intel feature: no file has edges.
#[cfg(not(any(feature = "code-intel-js", feature = "code-intel-stack")))]
fn has_resolver(_language: &str) -> bool {
    false
}

/// The index view the graph reads, or the error naming the fix when this session holds none.
fn index_view(store: &crate::store::Store) -> Result<Arc<dyn IndexView>, EngineError> {
    if let Some(view) = store.read_index_view() {
        return Ok(view);
    }
    if store.rescan_owed() {
        return Err(EngineError::index_unavailable(
            "the import graph needs the index, which must be rebuilt: run `basemind scan` (MCP: `index_rescan`)",
        ));
    }
    Err(EngineError::busy(
        "the import graph needs the index, which another basemind process holds or the daemon released: \
         retry, or run `index_rescan` to open it again",
    ))
}

#[cfg(test)]
#[path = "import_graph_tests.rs"]
mod tests;
