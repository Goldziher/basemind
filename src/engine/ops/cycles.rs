//! `basemind code cycles` (#135): the import cycles of the file-level graph, smallest first, each
//! with one import chain that closes it; or whether a proposed import would close one.

use crate::engine::error::EngineError;
use crate::engine::import_graph::{ImportGraph, NodeId, Tiers, import_graph};
use crate::engine::timing::elapsed_us;
use crate::engine::types::graph::{CycleCheck, CycleRow, CyclesList, CyclesParams, CyclesResponse};
use crate::engine::workspace::Workspace;
use crate::path::RelPath;

use super::graph_report::{ImportLines, coverage, notes, shortest_chain, tiers};

/// A cycle whose files are all Rust.
const RUST_NOTE: &str = "rust: modules of one crate may use each other; the compiler accepts this cycle (information)";
/// A cycle with a Go file.
const GO_NOTE: &str = "go: an import cycle does not compile, so the index is stale or the build is broken";

impl Workspace {
    /// `basemind code cycles`: every import cycle, or with `check_from` and `check_to` whether that
    /// import would close one.
    pub(crate) fn code_cycles(&self, p: CyclesParams) -> Result<CyclesResponse, EngineError> {
        let started = std::time::Instant::now();
        let tiers = tiers(p.include_type_imports, p.include_module_decls);
        match (p.check_from, p.check_to) {
            (Some(from), Some(to)) => {
                if p.path.is_some() || p.max_size.is_some() {
                    return Err(shape("a check takes no `path` or `max_size`"));
                }
                check(self, from, to, tiers, started).map(CyclesResponse::Check)
            }
            (None, None) => list(self, p.path.as_deref(), p.max_size, tiers, started).map(CyclesResponse::List),
            _ => Err(shape("`check_from` and `check_to` go together")),
        }
    }
}

fn shape(problem: &str) -> EngineError {
    EngineError::invalid_argument(format!(
        "`code cycles`: {problem}; pass nothing, `path` and/or `max_size`, or `check_from` with `check_to`"
    ))
}

fn list(
    ws: &Workspace,
    path: Option<&str>,
    max_size: Option<u32>,
    tiers: Tiers,
    started: std::time::Instant,
) -> Result<CyclesList, EngineError> {
    if max_size == Some(0) {
        return Err(shape("`max_size` is at least 1"));
    }
    let graph = import_graph(ws)?;
    let mut notes = notes(&graph);
    let scope = path
        .map(|path| path.trim_end_matches('/'))
        .filter(|path| !path.is_empty() && *path != ".");
    if let Some(scope) = scope
        && !(0..graph.len()).any(|node| under(graph.path(node_id(node)), scope))
    {
        notes.push(format!("no file of the import graph is at or under `{scope}`"));
    }
    let (cycles, over_max_size) = select(&graph, tiers, scope, max_size);
    let languages = languages_of(ws, &graph, &cycles);
    let mut lines = ImportLines::new(ws);
    let rows: Vec<CycleRow> = cycles
        .iter()
        .map(|members| CycleRow {
            size: members.len(),
            chain: lines.render(&graph, &closing_chain(&graph, members, tiers)),
            files: members.iter().map(|&node| graph.path(node).clone()).collect(),
            note: note(members.iter().map(|node| languages.get(node).map(String::as_str))),
        })
        .collect();
    Ok(CyclesList {
        cycle_count: rows.len(),
        files_in_cycles: rows.iter().map(|row| row.size).sum(),
        over_max_size,
        cycles: rows,
        coverage: coverage(&graph),
        notes,
        notice: ws.lifecycle_notice(),
        elapsed_us: elapsed_us(started),
    })
}

/// The cycles of `tiers` with a file under `scope`, at most `max_size` files each, smallest first
/// and then by first file; and how many `max_size` left out.
fn select(graph: &ImportGraph, tiers: Tiers, scope: Option<&str>, max_size: Option<u32>) -> (Vec<Vec<NodeId>>, usize) {
    let mut cycles: Vec<Vec<NodeId>> = graph
        .cycles(tiers)
        .into_iter()
        .filter(|members| scope.is_none_or(|scope| members.iter().any(|&node| under(graph.path(node), scope))))
        .collect();
    let before = cycles.len();
    if let Some(max) = max_size {
        cycles.retain(|members| members.len() <= max as usize);
    }
    let over = before - cycles.len();
    cycles.sort_by_key(|members| (members.len(), members[0]));
    (cycles, over)
}

/// One chain closing the cycle `members` (path-sorted): the shortest from its first file back to
/// itself through the component.
fn closing_chain(graph: &ImportGraph, members: &[NodeId], tiers: Tiers) -> Vec<NodeId> {
    let first = members[0];
    shortest_chain(
        graph,
        first,
        tiers,
        |node| members.binary_search(&node).is_ok(),
        |node| node == first,
    )
    .unwrap_or_else(|| vec![first, first])
}

/// Whether `path` is `scope` or a file under the directory `scope`.
fn under(path: &RelPath, scope: &str) -> bool {
    let path = path.as_bytes();
    let scope = scope.as_bytes();
    path == scope || (path.starts_with(scope) && path.get(scope.len()) == Some(&b'/'))
}

fn node_id(at: usize) -> NodeId {
    NodeId::try_from(at).unwrap_or(NodeId::MAX)
}

/// The language of every member of `cycles`, from the file map.
fn languages_of(ws: &Workspace, graph: &ImportGraph, cycles: &[Vec<NodeId>]) -> ahash::AHashMap<NodeId, String> {
    let store = ws.store_read();
    cycles
        .iter()
        .flatten()
        .filter_map(|&node| {
            let entry = store.index.files.get(graph.path(node))?;
            Some((node, entry.language.clone()))
        })
        .collect()
}

/// What a cycle of files in these languages means: legal among Rust modules, a broken build in Go.
fn note<'a>(languages: impl Iterator<Item = Option<&'a str>>) -> Option<&'static str> {
    let mut all_rust = true;
    for language in languages {
        if language == Some("go") {
            return Some(GO_NOTE);
        }
        all_rust &= language == Some("rust");
    }
    all_rust.then_some(RUST_NOTE)
}

fn check(
    ws: &Workspace,
    from: RelPath,
    to: RelPath,
    tiers: Tiers,
    started: std::time::Instant,
) -> Result<CycleCheck, EngineError> {
    let graph = import_graph(ws)?;
    {
        let store = ws.store_read();
        for path in [&from, &to] {
            if !store.index.files.contains_key(path) {
                return Err(crate::engine::files::not_indexed_in(
                    &store,
                    &ws.root,
                    "code cycles",
                    path,
                ));
            }
        }
    }
    let mut notes = notes(&graph);
    let closing = if from == to {
        Some(vec![])
    } else {
        match (graph.node(&from.to_str_lossy()), graph.node(&to.to_str_lossy())) {
            (Some(importer), Some(target)) => would_close(&graph, importer, target, tiers),
            (_, None) => {
                notes.push(format!("`{to}` imports no file of the repository the graph resolved"));
                None
            }
            (None, Some(_)) => {
                notes.push(format!("no resolved import reaches `{from}`"));
                None
            }
        }
    };
    let closing_chain = closing.map(|chain| {
        if chain.is_empty() {
            from.to_string()
        } else {
            ImportLines::new(ws).render(&graph, &chain)
        }
    });
    Ok(CycleCheck {
        would_close_cycle: closing_chain.is_some(),
        from,
        to,
        closing_chain,
        coverage: coverage(&graph),
        notes,
        notice: ws.lifecycle_notice(),
        elapsed_us: elapsed_us(started),
    })
}

/// The chain `target -> … -> importer` that a new import of `target` by `importer` would close.
fn would_close(graph: &ImportGraph, importer: NodeId, target: NodeId, tiers: Tiers) -> Option<Vec<NodeId>> {
    if !graph.would_close_cycle(importer, target, tiers) {
        return None;
    }
    shortest_chain(graph, target, tiers, |_| true, |node| node == importer)
}

#[cfg(test)]
#[path = "cycles_tests.rs"]
mod tests;
