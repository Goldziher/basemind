//! `basemind code impact` (#121): the files that depend on a file, on the files defining a symbol,
//! or on the files a change touches, walked through the file-level import graph by depth; the files
//! the targets import; and for a symbol the call sites that reach it (graded by how each was
//! resolved) and the calls it makes.

use ahash::AHashSet;

use crate::engine::error::EngineError;
use crate::engine::freshness;
use crate::engine::import_graph::{DEPTH_MAX, Direction, ImportGraph, NodeId, Tiers, WalkLimits, import_graph};
use crate::engine::scope::{ChangeSpec, change_scope};
use crate::engine::timing::elapsed_us;
use crate::engine::types::code::kind_to_str;
use crate::engine::types::graph::{ImpactDefinition, ImpactHit, ImpactParams, ImpactResponse};
use crate::engine::workspace::Workspace;
use crate::path::RelPath;

use super::graph_report::{coverage, index_not_ready, notes, tiers};
use super::impact_calls::{downstream, upstream};

/// Definitions of one name a symbol target reads.
const SYMBOL_DEFINITIONS_MAX: usize = 256;

/// The files the walk starts from, and what named them.
struct Targets {
    kind: &'static str,
    paths: Vec<RelPath>,
    definitions: Vec<ImpactDefinition>,
    scope_files: Option<usize>,
}

impl Workspace {
    /// `basemind code impact`: the files that import the target files and the files they import,
    /// transitively to `depth`, and for a symbol its graded callers and calls.
    pub(crate) fn code_impact(&self, p: ImpactParams) -> Result<ImpactResponse, EngineError> {
        let started = std::time::Instant::now();
        let named = usize::from(p.path.is_some()) + usize::from(p.symbol.is_some()) + usize::from(p.diff.is_some());
        if named != 1 {
            return Err(shape("name exactly one target"));
        }
        if p.depth.is_some_and(|depth| depth == 0 || depth > DEPTH_MAX) {
            return Err(shape(&format!("`depth` is 1 to {DEPTH_MAX}")));
        }
        let limits = WalkLimits::new(p.depth);
        let tiers = tiers(p.include_type_imports, p.include_module_decls);
        let symbol = p.symbol.clone();
        let targets = if let Some(path) = p.path {
            file_target(self, path)?
        } else if let Some(name) = p.symbol {
            symbol_target(self, &name)?
        } else {
            diff_target(self, p.diff.as_deref().unwrap_or_default())?
        };
        let graph = import_graph(self)?;
        let mut response = answer(&graph, targets, limits, tiers);
        if let Some(name) = symbol {
            add_calls(self, &mut response, &name, limits.depth);
        }
        response.notice = self.lifecycle_notice();
        response.elapsed_us = elapsed_us(started);
        Ok(response)
    }
}

fn shape(problem: &str) -> EngineError {
    EngineError::invalid_argument(format!(
        "`code impact`: {problem}; pass a file `path`, a `symbol` name, or a `diff` (a revision, \"\" for HEAD, \
         `A..B` or `A...B`), with an optional `depth`"
    ))
}

fn file_target(ws: &Workspace, path: RelPath) -> Result<Targets, EngineError> {
    let store = ws.store_read();
    if !store.index.files.contains_key(&path) {
        return Err(crate::engine::files::not_indexed_in(
            &store,
            &ws.root,
            "code impact",
            &path,
        ));
    }
    Ok(Targets {
        kind: "file",
        paths: vec![path],
        definitions: Vec::new(),
        scope_files: None,
    })
}

/// Every file defining `name`, exactly.
fn symbol_target(ws: &Workspace, name: &str) -> Result<Targets, EngineError> {
    let store = ws.store_read();
    let Some(view) = store.read_index_view() else {
        drop(store);
        return Err(index_not_ready(ws, "code impact").unwrap_or_else(|| {
            EngineError::busy("the symbol lookup needs the index, which another basemind process holds: retry")
        }));
    };
    let cache = ws.cache.load_full();
    let mut definitions: Vec<ImpactDefinition> = view
        .symbols_by_name_lookup(name, SYMBOL_DEFINITIONS_MAX)
        .into_iter()
        .filter(|(found, _, path, _)| found == name && store.index.files.contains_key(path))
        .filter_map(|(_, kind, path, start_byte)| {
            let (_, l1) = freshness::indexed_outline(&store, &cache, &path)?;
            let symbol = l1
                .symbols
                .iter()
                .find(|symbol| symbol.start_byte == start_byte && symbol.name == name)?;
            Some(ImpactDefinition {
                path,
                kind: kind_to_str(kind),
                line: symbol.start_row + 1,
            })
        })
        .collect();
    if definitions.is_empty() {
        drop(view);
        drop(store);
        if let Some(refusal) = index_not_ready(ws, "code impact") {
            return Err(refusal);
        }
        return Err(EngineError::invalid_argument(format!(
            "`code impact`: no definition named `{name}` is indexed; find the exact name with `basemind code \
             symbols {name} --fuzzy`, or pass a file path"
        )));
    }
    definitions.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    definitions.dedup();
    let mut paths: Vec<RelPath> = definitions.iter().map(|definition| definition.path.clone()).collect();
    paths.dedup();
    Ok(Targets {
        kind: "symbol",
        paths,
        definitions,
        scope_files: None,
    })
}

/// The indexed files a change touches.
fn diff_target(ws: &Workspace, diff: &str) -> Result<Targets, EngineError> {
    let spec = ChangeSpec::parse(diff)?;
    if spec.importers {
        return Err(shape(
            "drop `+importers`: impact already walks the importers of the change",
        ));
    }
    let repo = ws
        .repo
        .as_ref()
        .ok_or_else(|| EngineError::unavailable("`code impact` with a diff needs a git repository"))?;
    let scope = change_scope(repo, &spec.base)?;
    let store = ws.store_read();
    let mut paths: Vec<RelPath> = scope
        .paths()
        .iter()
        .filter(|path| store.index.files.contains_key(*path))
        .cloned()
        .collect();
    paths.sort_unstable();
    Ok(Targets {
        kind: "diff",
        paths,
        definitions: Vec::new(),
        scope_files: Some(scope.len()),
    })
}

/// The call-level answer of a symbol target: its callers over proven edges and the calls it makes.
fn add_calls(ws: &Workspace, response: &mut ImpactResponse, name: &str, depth: u8) {
    let callers = upstream(ws, &response.definitions, name, depth);
    let callees = downstream(ws, &response.definitions, name);
    response.callers = callers.rows;
    response.callers_truncated_reason = callers.truncated;
    response.calls = callees.rows;
    response.notes.extend(callers.notes);
    response.notes.extend(callees.notes);
    if callees.truncated {
        response.notes.push("calls: more calls than are listed".to_string());
    }
}

/// The walk from `targets`, without the lifecycle notice and timing the caller adds.
fn answer(graph: &ImportGraph, targets: Targets, limits: WalkLimits, tiers: Tiers) -> ImpactResponse {
    let mut starts: Vec<NodeId> = Vec::new();
    let mut not_in_graph: Vec<RelPath> = Vec::new();
    for path in &targets.paths {
        match graph.node(&path.to_str_lossy()) {
            Some(node) => starts.push(node),
            None => not_in_graph.push(path.clone()),
        }
    }
    let walk = graph.walk(&starts, Direction::Importers, limits, tiers);
    let imports = graph.walk(&starts, Direction::Dependencies, limits, tiers);
    let in_cycle: AHashSet<NodeId> = graph.cycles(tiers).into_iter().flatten().collect();
    let hits = |walk: &crate::engine::import_graph::Walk| -> Vec<ImpactHit> {
        walk.reached
            .iter()
            .map(|hit| ImpactHit {
                path: graph.path(hit.node).clone(),
                depth: hit.depth,
                via: graph.path(hit.via).clone(),
                in_cycle: in_cycle.contains(&hit.node),
            })
            .collect()
    };
    let affected = hits(&walk);
    let dependencies = hits(&imports);
    let mut targets_in_cycles: Vec<RelPath> = starts
        .iter()
        .filter(|node| in_cycle.contains(node))
        .map(|&node| graph.path(node).clone())
        .collect();
    targets_in_cycles.sort_unstable();
    ImpactResponse {
        target: targets.kind,
        targets: targets.paths,
        definitions: targets.definitions,
        scope_files: targets.scope_files,
        targets_in_cycles,
        not_in_graph,
        depth: limits.depth,
        affected_files: affected.len(),
        affected,
        truncated: walk.truncated.is_some(),
        truncated_reason: walk.truncated.map(|cut| cut.reason()),
        callers: Vec::new(),
        callers_truncated_reason: None,
        dependencies,
        dependencies_truncated_reason: imports.truncated.map(|cut| cut.reason()),
        calls: Vec::new(),
        coverage: coverage(graph),
        notes: notes(graph),
        notice: None,
        elapsed_us: 0,
    }
}

#[cfg(test)]
#[path = "impact_tests.rs"]
mod tests;
