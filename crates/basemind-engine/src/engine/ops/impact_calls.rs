//! The symbol-level half of `basemind code impact` (#121): the call sites of a symbol target walked
//! transitively over proven edges, and the calls its definitions make. Each edge carries how it was
//! established, so a name-only match is never presented as a proven dependency.

use ahash::AHashSet;

use crate::engine::types::graph::{ImpactCallee, ImpactCaller, ImpactDefinition};
use crate::engine::types::refs::Resolution;
use crate::engine::workspace::Workspace;
use crate::path::RelPath;

use super::explain::{callees_of, callers_page, holding_callable};

/// Definitions whose callers the walk looks up at most; past it the callers are partial.
const EXAMINED_MAX: usize = 256;
/// Call sites read per definition.
const PAGE_LIMIT: usize = 200;
/// Callers listed at most.
const CALLERS_MAX: usize = 500;
/// Calls listed at most.
const CALLEES_MAX: usize = 200;

/// A definition whose callers are looked up: file, name and 1-based line.
type Node = (RelPath, String, u32);

/// The callers of the definitions, and why the list stops short when it does.
#[derive(Debug, Default)]
pub(super) struct Callers {
    pub rows: Vec<ImpactCaller>,
    pub(super) truncated: Option<&'static str>,
    pub(super) notes: Vec<String>,
}

/// The calls the definitions make, and what could not be read.
#[derive(Debug, Default)]
pub(super) struct Callees {
    pub(super) rows: Vec<ImpactCallee>,
    pub(super) truncated: bool,
    pub(super) notes: Vec<String>,
}

/// Whether the resolution proves the site calls the definition.
fn proven(resolution: Resolution) -> bool {
    matches!(resolution, Resolution::SameFile | Resolution::Imported)
}

/// Whether the site is a caller at all: a site binding to another definition, or to a local that
/// is no definition, is not.
fn is_caller(resolution: Resolution) -> bool {
    matches!(
        resolution,
        Resolution::SameFile | Resolution::Imported | Resolution::NameOnly | Resolution::PathMatch
    )
}

/// The call sites of `definitions`, all named `name`, then of the definitions holding a proven site, to `depth` hops.
/// A `name_only` or `path_match` site is listed with its grade and not followed.
pub(super) fn upstream(ws: &Workspace, definitions: &[ImpactDefinition], name: &str, depth: u8) -> Callers {
    let mut walk = Callers::default();
    let mut visited: AHashSet<(RelPath, u32)> = AHashSet::new();
    let mut frontier: Vec<Node> = Vec::new();
    for definition in definitions {
        if visited.insert((definition.path.clone(), definition.line)) {
            frontier.push((definition.path.clone(), name.to_string(), definition.line));
        }
    }
    let mut examined = 0;
    for level in 1..=depth {
        let mut next: Vec<Node> = Vec::new();
        for (path, name, line) in std::mem::take(&mut frontier) {
            if examined >= EXAMINED_MAX {
                walk.truncated = Some("scan_cap");
                return finish(walk);
            }
            examined += 1;
            let page = match callers_page(ws, &path, &name, line, PAGE_LIMIT, None) {
                Ok(page) => page,
                Err(error) => {
                    note(&mut walk.notes, format!("callers of `{name}` ({path}:{line}): {error}"));
                    continue;
                }
            };
            if let Some(reason) = &page.resolution_note {
                note(&mut walk.notes, format!("callers of `{name}`: {reason}"));
            }
            if page.next_cursor.is_some() || page.truncated || page.budgeted {
                walk.truncated = Some("limit");
                note(
                    &mut walk.notes,
                    format!("callers of `{name}`: more call sites than the {PAGE_LIMIT} read"),
                );
            }
            for hit in page.hits.into_iter().filter(|hit| is_caller(hit.resolution)) {
                if walk.rows.len() >= CALLERS_MAX {
                    walk.truncated = Some("limit");
                    return finish(walk);
                }
                let holder = holding_callable(ws, &hit.path, hit.line);
                let proof = proven(hit.resolution);
                if let Some((holder_name, holder_line)) = &holder
                    && proof
                    && visited.insert((hit.path.clone(), *holder_line))
                {
                    next.push((hit.path.clone(), holder_name.clone(), *holder_line));
                }
                walk.rows.push(ImpactCaller {
                    path: hit.path,
                    line: hit.line,
                    depth: level,
                    symbol: holder.map(|(holder_name, _)| holder_name),
                    calls: name.clone(),
                    resolution: hit.resolution.as_str(),
                    proven: proof,
                });
            }
        }
        frontier = next;
    }
    if walk.truncated.is_none() && !frontier.is_empty() {
        walk.truncated = beyond_depth(ws, frontier, examined);
    }
    finish(walk)
}

/// Why the walk stops short at its last level: `depth` when a boundary definition has a
/// caller the walk did not follow, `scan_cap` when the examined cap leaves that unchecked, and
/// nothing when no boundary definition has one. A lookup that fails counts as a caller.
fn beyond_depth(ws: &Workspace, frontier: Vec<Node>, mut examined: usize) -> Option<&'static str> {
    for (path, name, line) in frontier {
        if examined >= EXAMINED_MAX {
            return Some("scan_cap");
        }
        examined += 1;
        let followed = callers_page(ws, &path, &name, line, PAGE_LIMIT, None)
            .map(|page| page.hits.iter().any(|hit| is_caller(hit.resolution)))
            .unwrap_or(true);
        if followed {
            return Some("depth");
        }
    }
    None
}

fn finish(mut walk: Callers) -> Callers {
    walk.rows
        .sort_by(|a, b| (a.depth, &a.path, a.line).cmp(&(b.depth, &b.path, b.line)));
    walk.rows.dedup();
    walk
}

/// The calls the bodies of `definitions` make, in definition then call order. A call the resolution
/// facts bind is `proven`; one that is only a name is `name_only`.
pub(super) fn downstream(ws: &Workspace, definitions: &[ImpactDefinition], name: &str) -> Callees {
    let mut out = Callees::default();
    for definition in definitions {
        let callees = match callees_of(ws, &definition.path, name, definition.line) {
            Ok(callees) => callees,
            Err(reason) => {
                note(
                    &mut out.notes,
                    format!("calls of {}:{}: {reason}", definition.path, definition.line),
                );
                continue;
            }
        };
        for callee in callees {
            if out.rows.len() >= CALLEES_MAX {
                out.truncated = true;
                return out;
            }
            let resolution = match &callee.definition {
                Some(path) if *path == definition.path => Resolution::SameFile,
                Some(_) => Resolution::Imported,
                None => Resolution::NameOnly,
            };
            out.rows.push(ImpactCallee {
                from: definition.path.clone(),
                name: callee.name,
                path: callee.definition,
                resolution: resolution.as_str(),
                proven: proven(resolution),
            });
        }
    }
    out
}

fn note(notes: &mut Vec<String>, text: String) {
    if !notes.contains(&text) {
        notes.push(text);
    }
}
