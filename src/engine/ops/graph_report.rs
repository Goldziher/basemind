//! What the import-graph commands (`impact`, `cycles`) share: the tiers a request names, the
//! coverage and notes every answer carries, shortest import chains, and the line of each import in
//! a chain.

use std::collections::VecDeque;

use ahash::AHashMap;

use crate::engine::freshness;
use crate::engine::import_graph::{ImportGraph, NodeId, Tiers};
use crate::engine::types::graph::{GraphCoverage, LanguageCoverageRow};
use crate::engine::workspace::Workspace;
use crate::path::RelPath;

/// The note an answer carries while the resolve pass has not covered the whole index.
pub(super) const PROBES_INCOMPLETE: &str =
    "the resolve pass has not covered the whole index yet, so imports may be missing: run `basemind scan`";
/// The note of a Go graph (ADR-0047 decision 4).
const GO_BUILD_TAGS: &str = "go: build tags are ignored, so every non-test file of a package is an edge target";

/// The tiers a request's flags name.
pub(super) fn tiers(type_imports: bool, module_decls: bool) -> Tiers {
    Tiers {
        type_imports,
        module_decls,
    }
}

/// What `graph` knows, for an answer.
pub(super) fn coverage(graph: &ImportGraph) -> GraphCoverage {
    let known = graph.coverage();
    GraphCoverage {
        files: graph.len(),
        edges: graph.edge_count(),
        languages: known
            .languages
            .iter()
            .filter(|language| language.resolver)
            .map(|language| LanguageCoverageRow {
                language: language.language.clone(),
                files: language.files,
                importers: language.importers,
                edges: language.edges,
                type_only_edges: language.type_only_edges,
                module_decl_edges: language.module_decl_edges,
                external: language.external,
                unresolved: language.unresolved,
            })
            .collect(),
        without_resolver: known.without_resolver().map(str::to_string).collect(),
    }
}

/// What the graph cannot see that bears on every answer: an unfinished resolve pass, no resolvable
/// language at all ("no edges", never "no dependents" or "no cycles"), Go build tags.
pub(super) fn notes(graph: &ImportGraph) -> Vec<String> {
    let mut notes = Vec::new();
    if !graph.probes_complete() {
        notes.push(PROBES_INCOMPLETE.to_string());
    }
    let known = graph.coverage();
    if !known.any_resolvable() {
        let languages: Vec<&str> = known.without_resolver().collect();
        notes.push(if languages.is_empty() {
            "no edges: the index holds no file".to_string()
        } else {
            format!(
                "no edges: no resolver for {} in this build, so the graph is not analysed (not empty)",
                languages.join(", ")
            )
        });
    }
    if known
        .languages
        .iter()
        .any(|language| language.resolver && language.language == "go" && language.files > 0)
    {
        notes.push(GO_BUILD_TAGS.to_string());
    }
    notes
}

/// The shortest import chain from `from` to a file `done` accepts, through files `within` accepts,
/// following edges of `tiers`: `[from, …, last]`, where `last` is the accepted file. Breadth-first,
/// so the chain is a shortest one and the same on every call.
pub(super) fn shortest_chain(
    graph: &ImportGraph,
    from: NodeId,
    tiers: Tiers,
    within: impl Fn(NodeId) -> bool,
    done: impl Fn(NodeId) -> bool,
) -> Option<Vec<NodeId>> {
    let mut parent: AHashMap<NodeId, NodeId> = AHashMap::new();
    let mut queue = VecDeque::from([from]);
    while let Some(node) = queue.pop_front() {
        for edge in graph.dependencies(node, tiers) {
            if done(edge.node) {
                let mut chain = vec![edge.node, node];
                let mut at = node;
                while at != from {
                    at = parent[&at];
                    chain.push(at);
                }
                chain.reverse();
                return Some(chain);
            }
            if edge.node != from && within(edge.node) && !parent.contains_key(&edge.node) {
                parent.insert(edge.node, node);
                queue.push_back(edge.node);
            }
        }
    }
    None
}

/// Line numbers of imports, read from the working tree once per file. A file edited since the scan,
/// or one that cannot be read, has none: its offsets describe other bytes.
pub(super) struct ImportLines<'a> {
    ws: &'a Workspace,
    newlines: AHashMap<NodeId, Option<Vec<usize>>>,
}

impl<'a> ImportLines<'a> {
    pub(super) fn new(ws: &'a Workspace) -> Self {
        Self {
            ws,
            newlines: AHashMap::new(),
        }
    }

    /// The 1-based line of byte `offset` in `path`, when the file still holds its indexed content.
    fn line(&mut self, node: NodeId, path: &RelPath, offset: u32) -> Option<u32> {
        let ws = self.ws;
        let newlines = self.newlines.entry(node).or_insert_with(|| {
            let entry = ws.store_read().index.files.get(path).cloned()?;
            let file = crate::path::lexical_file(&ws.root, path).ok()?;
            let live = freshness::load(&file, (&entry).into(), ws.config.scan.max_file_bytes).ok()?;
            live.fresh.then(|| memchr::memchr_iter(b'\n', &live.bytes).collect())
        });
        let newlines = newlines.as_ref()?;
        let offset = offset as usize;
        u32::try_from(newlines.partition_point(|newline| *newline < offset) + 1).ok()
    }

    /// `chain` as `a.rs:12 -> b.rs:40 -> c.rs`: each file but the last with the line of its import
    /// of the next.
    pub(super) fn render(&mut self, graph: &ImportGraph, chain: &[NodeId]) -> String {
        let mut steps: Vec<String> = Vec::with_capacity(chain.len());
        for pair in chain.windows(2) {
            let (from, to) = (pair[0], pair[1]);
            let path = graph.path(from);
            let offset = graph
                .dependencies(from, Tiers::ALL)
                .find(|edge| edge.node == to)
                .map(|edge| edge.local_start);
            match offset.and_then(|offset| self.line(from, path, offset)) {
                Some(line) => steps.push(format!("{path}:{line}")),
                None => steps.push(path.to_string()),
            }
        }
        if let Some(&last) = chain.last() {
            steps.push(graph.path(last).to_string());
        }
        steps.join(" -> ")
    }
}

#[cfg(test)]
#[path = "graph_report_tests.rs"]
pub(super) mod tests;
