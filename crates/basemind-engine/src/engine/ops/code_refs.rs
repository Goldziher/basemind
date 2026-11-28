//! Body of `code_refs`: the usages of a name or a file. Each relation is a read of the index
//! (`crate::engine::refs`): call sites of a name, the callers of one definition, implementations,
//! importers (of a module name, or of a file through the import graph), the definition a reference
//! binds to, or the import cycles.

use super::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX};
use crate::engine::budget::{apply_budget, budget};
use crate::engine::cursor::Cursor;
use crate::engine::error::EngineError;
use crate::engine::files::IndexedFiles;
use crate::engine::refs::calls::{index_for_refs, run_find_callers, run_find_references};
use crate::engine::refs::graph::{GraphPage, run_cycles, run_graph_importers};
use crate::engine::refs::impls::run_find_implementations;
use crate::engine::refs::intel::run_goto_definition;
use crate::engine::refs::matching::Matching;
use crate::engine::refs::routes::{Endpoint, run_route_definition};
use crate::engine::stale::flag_listed;
use crate::engine::types::refs::{
    CodeRefsParams, CodeRefsResult, DependentsResponse, FindCallersParams, FindCallersResponse,
    FindImplementationsParams, FindReferencesParams, GotoDefinitionParams, ResolutionFilter,
};
use crate::engine::workspace::Workspace;

/// What `code_refs` lists about a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    Callers,
    Implementations,
    Importers,
    Definition,
    Cycles,
}

impl Relation {
    fn parse(raw: Option<&str>) -> Result<Self, EngineError> {
        match raw.unwrap_or("callers") {
            "callers" => Ok(Self::Callers),
            "implementations" => Ok(Self::Implementations),
            "importers" => Ok(Self::Importers),
            "definition" => Ok(Self::Definition),
            "cycles" => Ok(Self::Cycles),
            other => Err(EngineError::invalid_argument(format!(
                "`code_refs`: unknown relation `{other}`; expected `callers`, `implementations`, \
                 `importers`, `definition` or `cycles`"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Callers => "callers",
            Self::Implementations => "implementations",
            Self::Importers => "importers",
            Self::Definition => "definition",
            Self::Cycles => "cycles",
        }
    }
}

/// The argument shapes `code_refs` accepts, quoted in every shape error.
const REFS_SHAPES: &str = "accepted shapes: {name} (call sites of a name), {name, path} (callers of the definition in \
     that file; add `line` when several there share the name), {name, relation:\"implementations\"}, \
     {name, relation:\"importers\"} (files whose imports name it), {path, relation:\"importers\", depth?} (files \
     importing that file, `depth` hops deep), {name, relation:\"definition\", path, line} (the definition a \
     reference binds to), {relation:\"cycles\", path?} (import cycles, or the one holding that file); a \
     `METHOD /path` name is a route: its callers are the HTTP calls matching it, its `definition` the route an \
     HTTP call on `line` reaches";

impl Workspace {
    /// `code_refs`: usages of a name: call sites, implementations, importers, or the definition a
    /// reference binds to; or a file's importers and the import cycles.
    pub fn code_refs(&self, p: CodeRefsParams) -> Result<CodeRefsResult, EngineError> {
        code_refs(self, p)
    }
}

fn code_refs(ws: &Workspace, p: CodeRefsParams) -> Result<CodeRefsResult, EngineError> {
    let relation = Relation::parse(p.relation.as_deref())?;
    let substring = Matching::parse(p.match_mode.as_deref())? == Matching::Substring;
    let shape_error = |problem: &str| EngineError::invalid_argument(format!("`code_refs`: {problem}; {REFS_SHAPES}"));
    if relation != Relation::Definition && p.column.is_some() {
        return Err(shape_error("`column` applies only to relation `definition`"));
    }
    if p.depth.is_some() && !(relation == Relation::Importers && p.path.is_some()) {
        return Err(shape_error("`depth` applies only to relation `importers` with `path`"));
    }
    if p.line.is_some()
        && (matches!(
            relation,
            Relation::Implementations | Relation::Importers | Relation::Cycles
        ) || p.path.is_none())
    {
        return Err(shape_error(
            "`line` needs `path`: it picks one of several same-named definitions, or locates the reference \
             for relation `definition`",
        ));
    }
    if relation != Relation::Callers && p.resolution.is_some() {
        return Err(shape_error(
            "`resolution` filters call sites and applies only to relation `callers`",
        ));
    }
    if relation != Relation::Callers && p.changed.is_some() {
        return Err(shape_error("`changed` applies to call sites, relation `callers`, only"));
    }
    let resolution = p
        .resolution
        .as_deref()
        .map(ResolutionFilter::parse)
        .transpose()
        .map_err(|problem| EngineError::invalid_argument(format!("`code_refs`: {problem}")))?;
    let page = |p: &CodeRefsParams| GraphPage {
        limit: p.limit,
        cursor: p.cursor.clone(),
        max_tokens: budget(p.max_tokens),
    };
    match (relation, &p.path) {
        (Relation::Cycles, path) => {
            if p.name.is_some() || p.match_mode.is_some() {
                return Err(shape_error(
                    "relation `cycles` takes no `name` or `match`; `path` narrows it to one file's cycle",
                ));
            }
            return run_cycles(ws, path.as_ref(), page(&p)).map(CodeRefsResult::Cycles);
        }
        (Relation::Importers, Some(path)) => {
            if p.name.is_some() || p.match_mode.is_some() {
                return Err(shape_error(
                    "relation `importers` takes `name` (matched against import text) or `path` (a file, \
                     through the import graph), not both, and no `match`",
                ));
            }
            return run_graph_importers(ws, path, p.depth, page(&p)).map(CodeRefsResult::GraphImporters);
        }
        _ => {}
    }
    let Some(name) = p.name else {
        return Err(shape_error(&format!("relation `{}` needs `name`", relation.as_str())));
    };
    match relation {
        Relation::Callers => {
            // Resolved before the map cache or the store lock is taken: `+importers` builds the
            // import graph under the store read lock.
            let scope = crate::engine::changed::resolve(ws, "code_refs", p.changed.as_deref())?;
            let scope_files = scope.as_ref().map(|scope| scope.len());
            let call_sites = CallSites {
                name,
                path: p.path,
                line: p.line,
                substring,
                limit: p.limit,
                cursor: p.cursor,
                max_tokens: budget(p.max_tokens),
                resolution,
                scope,
            };
            run_call_sites(ws, call_sites, scope_files)
        }
        Relation::Implementations => {
            if p.path.is_some() {
                return Err(shape_error("`path` does not apply to relation `implementations`"));
            }
            ws.wait_map_ready();
            let idx = index_for_refs(&ws.store_read())?;
            let cache = ws.cache.load_full();
            run_find_implementations(
                &*idx,
                FindImplementationsParams {
                    trait_name: name,
                    substring,
                    language: None,
                    limit: p.limit,
                    max_tokens: budget(p.max_tokens),
                    cursor: p.cursor,
                },
                &cache,
                || ws.lifecycle_notice(),
            )
            .map(|mut implementations| {
                flag_listed(ws, &cache, &mut implementations.hits);
                CodeRefsResult::Implementations(implementations)
            })
        }
        Relation::Importers => {
            if p.match_mode.is_some() {
                return Err(shape_error(
                    "relation `importers` matches `name` against import paths and takes no `match`",
                ));
            }
            run_dependents(ws, name, p.limit, p.cursor, budget(p.max_tokens)).map(CodeRefsResult::Importers)
        }
        Relation::Definition => {
            let (Some(path), Some(line)) = (p.path, p.line) else {
                return Err(shape_error("relation `definition` needs `path` and `line`"));
            };
            if p.limit.is_some() || p.cursor.is_some() || p.max_tokens.is_some() || p.match_mode.is_some() {
                return Err(shape_error(
                    "relation `definition` returns one location and takes no `match`, `limit`, `cursor` or \
                     `max_tokens`",
                ));
            }
            if let Some(call) = Endpoint::parse(&name) {
                if p.column.is_some() {
                    return Err(shape_error(
                        "an HTTP call (`METHOD /path` name) is picked by `line` and `name`; drop `column`",
                    ));
                }
                return run_route_definition(ws, path, line, &call).map(CodeRefsResult::Definition);
            }
            let column = match p.column {
                Some(0) => return Err(shape_error("`column` is 1-based")),
                Some(column) => Some(column - 1),
                None => None,
            };
            let reference = GotoDefinitionParams {
                path,
                line,
                column,
                name,
            };
            run_goto_definition(ws, reference).map(CodeRefsResult::Definition)
        }
        Relation::Cycles => unreachable!("answered above"),
    }
}

/// The arguments of relation `callers`: the call sites of a name, or with `path` the callers of the
/// definition there.
struct CallSites {
    name: String,
    path: Option<crate::path::RelPath>,
    line: Option<u32>,
    substring: bool,
    limit: Option<u32>,
    cursor: Option<Cursor>,
    max_tokens: Option<u32>,
    resolution: Option<ResolutionFilter>,
    scope: Option<crate::engine::scope::ChangeScope>,
}

/// Relation `callers`, with the `changed` scope's size.
fn run_call_sites(ws: &Workspace, p: CallSites, scope_files: Option<usize>) -> Result<CodeRefsResult, EngineError> {
    let mut answer = match p.path {
        Some(path) => {
            let callers = FindCallersParams {
                path,
                name: p.name,
                substring: p.substring,
                kind: None,
                line: p.line,
                limit: p.limit,
                max_tokens: p.max_tokens,
                cursor: p.cursor,
                resolution: p.resolution,
                scope: p.scope,
            };
            CodeRefsResult::Callers(run_callers(ws, callers)?)
        }
        None => {
            ws.wait_map_ready();
            let idx = index_for_refs(&ws.store_read())?;
            let cache = ws.cache.load_full();
            let references = run_find_references(
                &*idx,
                IndexedFiles::of(ws),
                &cache,
                FindReferencesParams {
                    name: p.name,
                    substring: p.substring,
                    limit: p.limit,
                    max_tokens: p.max_tokens,
                    cursor: p.cursor,
                    resolution: p.resolution,
                    scope: p.scope,
                },
                || ws.lifecycle_notice(),
                |hits| flag_listed(ws, &cache, hits),
            )?;
            CodeRefsResult::References(references)
        }
    };
    match &mut answer {
        CodeRefsResult::Callers(callers) => callers.scope_files = scope_files,
        CodeRefsResult::References(references) => references.scope_files = scope_files,
        _ => unreachable!("relation `callers` answers call sites"),
    }
    Ok(answer)
}

/// Body of the `callers` mode: the name scan and the resolved-reference join over this session's
/// store ([`run_find_callers`]).
fn run_callers(ws: &Workspace, params: FindCallersParams) -> Result<FindCallersResponse, EngineError> {
    ws.wait_map_ready();
    let cache = ws.cache.load_full();
    let mut callers = {
        let store = ws.store_read();
        let idx = index_for_refs(&store)?;
        run_find_callers(&store, &*idx, IndexedFiles::of(ws), &cache, params, || {
            ws.lifecycle_notice()
        })?
    };
    flag_listed(ws, &cache, &mut callers.hits);
    Ok(callers)
}

/// Body of `code_refs` relation `importers`: a heuristic reverse lookup over every file's imports.
///
/// Streams the corpus instead of consulting a pre-flattened imports index. That index was a second
/// full copy of every import held for the process lifetime, to save one pass over data this tool
/// alone reads; the pass is now over outlines that the L1 cache serves and immediately drops.
fn run_dependents(
    ws: &Workspace,
    module: String,
    limit: Option<u32>,
    cursor: Option<Cursor>,
    max_tokens: Option<u32>,
) -> Result<DependentsResponse, EngineError> {
    use std::sync::atomic::Ordering;

    ws.wait_map_ready();
    let generation = ws.cache_generation.load(Ordering::Relaxed);
    let skip = match cursor.as_ref() {
        Some(cursor) => {
            let (offset, snapshot) = cursor.decode_in_memory()?;
            if snapshot != generation {
                return Err(EngineError::invalid_argument(
                    "`code_refs`: the index changed since this cursor was issued. Call again without `cursor`.",
                ));
            }
            offset as usize
        }
        None => 0,
    };
    let limit = limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    if module.is_empty() {
        return Err(EngineError::invalid_argument(
            "`code_refs`: `name` is empty; pass the module or import path to look for.",
        ));
    }
    let finder = memchr::memmem::Finder::new(module.as_bytes());
    let mut paths: Vec<crate::path::RelPath> = Vec::new();
    let cache = ws.cache.load_full();
    if module.as_bytes().contains(&0) {
        cache.for_each(|path, l1| {
            if crate::extract::l3::imports_mention(&module, &finder, &l1.imports) {
                paths.push(path.clone());
            }
        });
        paths.sort_unstable();
    } else {
        // Path order, so `paths` comes out sorted.
        paths.extend(
            cache
                .terms()
                .iter()
                .filter(|(_, t)| t.mentions_import(&finder))
                .map(|(p, _)| p.clone()),
        );
    }
    let total = paths.len();
    let page: Vec<crate::path::RelPath> = paths.into_iter().skip(skip).take(limit).collect();
    let budgeted = apply_budget(page, max_tokens);
    let next = skip + budgeted.items.len();
    let next_cursor = (next < total).then(|| Cursor::encode_in_memory(next as u64, generation));
    Ok(DependentsResponse {
        total: next_cursor.is_some().then_some(total),
        budgeted: budgeted.budgeted,
        paths: budgeted.items,
        next_cursor,
        notice: ws.lifecycle_notice(),
    })
}
