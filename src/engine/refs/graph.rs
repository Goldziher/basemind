//! `code_refs` over the file-level import graph ([`crate::engine::import_graph`]): `importers` with
//! `path` (the files that reach one file within `depth` hops, #121) and `cycles` (the import cycles,
//! or the one holding a file, #135). Both follow the default edge tier, [`Tiers::RUNTIME`]: no MCP
//! argument selects another (ADR-0047 decision 4).

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::Ordering;

use crate::engine::budget::apply_budget;
use crate::engine::cursor::Cursor;
use crate::engine::error::EngineError;
use crate::engine::files::not_indexed_in;
use crate::engine::import_graph::{Coverage, Direction, ImportGraph, NodeId, Tiers, WalkLimits, import_graph};
use crate::engine::ops::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX};
use crate::engine::types::refs::{CyclesResponse, GraphImportersResponse, ImportCycle};
use crate::engine::workspace::Workspace;
use crate::path::RelPath;

/// Languages a full build resolves imports for. One of them indexed without its resolver compiled
/// in is named in the coverage note; any other language has no imports the graph could hold.
const RESOLVABLE: [&str; 7] = ["go", "java", "javascript", "python", "rust", "tsx", "typescript"];

/// The language notes of [`CyclesResponse::notes`], by file extension.
const NOTES: [(&str, &str); 2] = [
    (
        ".go",
        "go: an import cycle does not compile, so the index is stale or the build is broken",
    ),
    (
        ".rs",
        "rust: modules of one crate may import each other; a cycle is legal and informational",
    ),
];

/// The arguments of a graph relation that pages.
pub(crate) struct GraphPage {
    pub(crate) limit: Option<u32>,
    pub(crate) cursor: Option<Cursor>,
    pub(crate) max_tokens: Option<u32>,
}

impl GraphPage {
    /// The offset `cursor` resumes at, refused when the graph it was cut from is gone.
    fn skip(&self, generation: u32) -> Result<usize, EngineError> {
        let Some(cursor) = self.cursor.as_ref() else {
            return Ok(0);
        };
        let (offset, snapshot) = cursor.decode_in_memory()?;
        if snapshot != generation {
            return Err(EngineError::invalid_argument(
                "`code_refs`: the index changed since this cursor was issued. Call again without `cursor`.",
            ));
        }
        Ok(usize::try_from(offset).unwrap_or(usize::MAX))
    }

    fn limit(&self) -> usize {
        self.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize
    }
}

/// The language of the indexed file `path`, or the error naming it unindexed.
fn language_of(ws: &Workspace, path: &RelPath) -> Result<String, EngineError> {
    let store = ws.store_read();
    store
        .lookup(path)
        .map(|entry| entry.language.clone())
        .ok_or_else(|| not_indexed_in(&store, &ws.root, "code_refs", path))
}

/// `code_refs {relation: "importers", path, depth?}`: the files reaching `path` within `depth`
/// import hops, nearest first, each depth in path order.
pub(crate) fn run_graph_importers(
    ws: &Workspace,
    path: &RelPath,
    depth: Option<u32>,
    page: GraphPage,
) -> Result<GraphImportersResponse, EngineError> {
    let depth = match depth {
        None => None,
        Some(depth @ 1..=5) => u8::try_from(depth).ok(),
        Some(other) => {
            return Err(EngineError::invalid_argument(format!(
                "`code_refs`: `depth` is 1 to 5 import hops, not {other}"
            )));
        }
    };
    let language = language_of(ws, path)?;
    let generation = ws.cache_generation.load(Ordering::Acquire);
    let skip = page.skip(generation)?;
    let graph = import_graph(ws)?;

    let target = graph.node(&path.to_str_lossy());
    let walk = target.map(|node| graph.walk(&[node], Direction::Importers, WalkLimits::new(depth), Tiers::RUNTIME));
    let reached = walk.as_ref().map_or(&[][..], |walk| walk.reached.as_slice());
    let total = reached.len();
    let listed: Vec<(u8, NodeId)> = reached
        .iter()
        .skip(skip)
        .take(page.limit())
        .map(|reached| (reached.depth, reached.node))
        .collect();
    let budgeted = apply_budget(
        listed.iter().map(|&(depth, node)| (depth, graph.path(node))).collect(),
        page.max_tokens,
    );
    let kept = budgeted.items.len();
    // The target reaches an importer exactly when the importer is in the target's component.
    let reaches = target.map(|node| graph.reachable(&[node], Direction::Dependencies, Tiers::RUNTIME));
    let mut by_depth: BTreeMap<u8, Vec<RelPath>> = BTreeMap::new();
    let mut in_cycle = Vec::new();
    for &(depth, node) in &listed[..kept] {
        let file = graph.path(node).clone();
        if reaches.as_ref().is_some_and(|mask| mask[node as usize]) {
            in_cycle.push(file.clone());
        }
        by_depth.entry(depth).or_default().push(file);
    }
    let truncation = walk.and_then(|walk| walk.truncated);
    let next = skip + kept;
    Ok(GraphImportersResponse {
        total,
        budgeted: budgeted.budgeted,
        by_depth,
        in_cycle,
        truncated: truncation.is_some(),
        truncated_reason: truncation.map(|cut| cut.reason()),
        coverage: file_coverage(&graph, &language, IMPORTERS_FALLBACK),
        next_cursor: (next < total).then(|| Cursor::encode_in_memory(next as u64, generation)),
        notice: ws.lifecycle_notice(),
    })
}

/// `code_refs {relation: "cycles", path?}`: the import cycles, smallest first and then by first
/// file, or with `path` the one holding that file.
pub(crate) fn run_cycles(
    ws: &Workspace,
    path: Option<&RelPath>,
    page: GraphPage,
) -> Result<CyclesResponse, EngineError> {
    let language = path.map(|path| language_of(ws, path)).transpose()?;
    let generation = ws.cache_generation.load(Ordering::Acquire);
    let skip = page.skip(generation)?;
    let graph = import_graph(ws)?;

    let mut components = graph.cycles(Tiers::RUNTIME);
    if let Some(path) = path {
        let node = graph.node(&path.to_str_lossy());
        components.retain(|members| node.is_some_and(|node| members.binary_search(&node).is_ok()));
    }
    components.sort_by_key(Vec::len);
    let total = components.len();
    let cycles: Vec<ImportCycle> = components
        .iter()
        .skip(skip)
        .take(page.limit())
        .map(|members| cycle(&graph, members))
        .collect();
    let budgeted = apply_budget(cycles, page.max_tokens);
    let next = skip + budgeted.items.len();
    let notes = NOTES
        .iter()
        .filter(|(extension, _)| {
            budgeted.items.iter().any(|cycle| {
                cycle
                    .files
                    .iter()
                    .any(|file| file.as_bytes().ends_with(extension.as_bytes()))
            })
        })
        .map(|(_, note)| *note)
        .collect();
    let coverage = match language {
        Some(language) => file_coverage(&graph, &language, ""),
        None => graph_coverage(&graph),
    };
    Ok(CyclesResponse {
        total,
        budgeted: budgeted.budgeted,
        cycles: budgeted.items,
        notes,
        coverage,
        next_cursor: (next < total).then(|| Cursor::encode_in_memory(next as u64, generation)),
        notice: ws.lifecycle_notice(),
    })
}

/// A cycle's files and, when its shortest loop from the first file misses some, that loop.
fn cycle(graph: &ImportGraph, members: &[NodeId]) -> ImportCycle {
    let loop_ = shortest_loop(graph, members);
    let paths = |nodes: &[NodeId]| nodes.iter().map(|&node| graph.path(node).clone()).collect::<Vec<_>>();
    if loop_.len() == members.len() {
        ImportCycle {
            files: paths(&loop_),
            chain: Vec::new(),
        }
    } else {
        ImportCycle {
            files: paths(members),
            chain: paths(&loop_),
        }
    }
}

/// The shortest import loop from `members[0]` back to itself inside the component, by a
/// breadth-first walk over its runtime edges: the first file, then each file the previous imports.
fn shortest_loop(graph: &ImportGraph, members: &[NodeId]) -> Vec<NodeId> {
    let start = members[0];
    let mut parent: ahash::AHashMap<NodeId, NodeId> = ahash::AHashMap::new();
    let mut queue = VecDeque::from([start]);
    while let Some(node) = queue.pop_front() {
        for edge in graph.dependencies(node, Tiers::RUNTIME) {
            if edge.node == start {
                let mut loop_ = vec![node];
                let mut at = node;
                while at != start {
                    at = parent[&at];
                    loop_.push(at);
                }
                loop_.reverse();
                return loop_;
            }
            if members.binary_search(&edge.node).is_ok() && !parent.contains_key(&edge.node) {
                parent.insert(edge.node, node);
                queue.push_back(edge.node);
            }
        }
    }
    vec![start]
}

/// What a caller asking for importers can use when the graph cannot answer.
const IMPORTERS_FALLBACK: &str = "; {name, relation:\"importers\"} matches import text instead";

/// The coverage note of an answer about one file of `language`; `fallback` follows the missing
/// resolver's clause.
fn file_coverage(graph: &ImportGraph, language: &str, fallback: &str) -> Option<String> {
    let resolved = graph
        .coverage()
        .languages
        .iter()
        .any(|covered| covered.language == language && covered.resolver);
    let mut notes = Vec::new();
    if !resolved {
        notes.push(format!(
            "no import resolver for {language} in this build: its imports are unknown{fallback}"
        ));
    }
    if !graph.probes_complete() {
        notes.push(INCOMPLETE.to_string());
    }
    (!notes.is_empty()).then(|| notes.join("; "))
}

/// The coverage note of an answer about the whole graph.
fn graph_coverage(graph: &ImportGraph) -> Option<String> {
    let coverage: &Coverage = graph.coverage();
    let mut notes = Vec::new();
    if !coverage.any_resolvable() {
        notes.push("no indexed file is in a language this build resolves imports for: no cycle is known".to_string());
    } else {
        let missing: Vec<&str> = coverage
            .without_resolver()
            .filter(|language| RESOLVABLE.contains(language))
            .collect();
        if !missing.is_empty() {
            notes.push(format!(
                "no import resolver for {} in this build: those files are left out",
                missing.join(", ")
            ));
        }
    }
    if !graph.probes_complete() {
        notes.push(INCOMPLETE.to_string());
    }
    (!notes.is_empty()).then(|| notes.join("; "))
}

/// The coverage note while the resolve pass has not covered the whole index.
const INCOMPLETE: &str = "import resolution has not finished over the index: edges may be missing";
