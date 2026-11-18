//! Bodies of the code verb tools `code_search` and `code_refs`. The other code tools are engine
//! operations (`src/engine/ops/`).
//!
//! Each `pub(super) async fn` here is what both the `#[tool]` shim in `tools.rs` and the matching
//! `basemind code …` command run. They translate the tool's wire arguments
//! ([`super::types_tools`]) into the per-operation shapes the query bodies take; those bodies stay
//! where they already lived: `helpers_calls`, `helpers_impls`,
//! `helpers_intel`, `helpers_code_search`. `code_read` is an engine op (`engine/ops/code_read.rs`).

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{
    SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX, json_result, run_find_callers, run_find_implementations,
    run_find_references,
};
use super::tool_contract::budget;
use super::types::{DependentsResponse, FindCallersParams, FindReferencesParams, GotoDefinitionParams};
use super::types_impls::FindImplementationsParams;
use super::types_tools::{CodeRefsParams, CodeSearchParams};

/// `code_search`: semantic / hybrid search returning pointers.
pub(super) async fn code_search(state: &ServerState, p: CodeSearchParams) -> Result<CallToolResult, McpError> {
    #[cfg(feature = "code-search")]
    {
        use super::helpers_page::{RANKED_FETCH, RANKED_LIMIT_DEFAULT, page_ranked, ranked_offset, ranked_retrieval};

        let generation = state.shared.index_generation.load(std::sync::atomic::Ordering::Relaxed);
        let offset = ranked_offset("code_search", p.cursor.as_ref(), generation)?;
        let limit = p.limit.unwrap_or(RANKED_LIMIT_DEFAULT);
        let ranking = serde_json::json!({ "query": &p.query, "lane": &p.lane });
        let search = super::types_code::SearchCodeParams {
            query: p.query,
            limit: Some(RANKED_FETCH),
            max_tokens: None,
            // The page is cut out of the structured response, which only the JSON encoding has, so
            // `[documents.output] format = "toon"` does not apply to this tool.
            format: Some("json".to_string()),
            mode: p.lane,
            reranker_enabled: None,
            reranker_preset: None,
            reranker_top_k: None,
        };
        let cache = &state.shared.ranked;
        let result = ranked_retrieval(cache, "code_search", &ranking, "hits", generation, || {
            super::helpers_code_search::run_search_code(state, search)
        })
        .await?;
        page_ranked(result, "hits", offset, limit, budget(p.max_tokens), generation)
    }
    #[cfg(not(feature = "code-search"))]
    {
        let _ = (state, p);
        Err(super::tool_contract::feature_missing("code_search", "code-search"))
    }
}

/// What `code_refs` lists about a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    Callers,
    Implementations,
    Importers,
    Definition,
}

impl Relation {
    fn parse(raw: Option<&str>) -> Result<Self, McpError> {
        match raw.unwrap_or("callers") {
            "callers" => Ok(Self::Callers),
            "implementations" => Ok(Self::Implementations),
            "importers" => Ok(Self::Importers),
            "definition" => Ok(Self::Definition),
            other => Err(McpError::invalid_params(
                format!(
                    "`code_refs`: unknown relation `{other}`; expected `callers`, `implementations`, \
                     `importers` or `definition`"
                ),
                None,
            )),
        }
    }
}

/// The argument shapes `code_refs` accepts, quoted in every shape error.
const REFS_SHAPES: &str = "accepted shapes: {name} (call sites of a name), {name, path} (callers of the definition in \
     that file; add `line` when several there share the name), {name, relation:\"implementations\"}, \
     {name, relation:\"importers\"}, {name, relation:\"definition\", path, line} (the definition a reference \
     binds to)";

/// `code_refs`: usages of a name: call sites, implementations, importers, or the definition a
/// reference binds to.
pub(super) async fn code_refs(state: &ServerState, p: CodeRefsParams) -> Result<CallToolResult, McpError> {
    let relation = Relation::parse(p.relation.as_deref())?;
    let substring = super::helpers_refs_match::Matching::parse(p.match_mode.as_deref())?
        == super::helpers_refs_match::Matching::Substring;
    let shape_error = |problem: &str| McpError::invalid_params(format!("`code_refs`: {problem}; {REFS_SHAPES}"), None);
    if relation != Relation::Definition && p.column.is_some() {
        return Err(shape_error("`column` applies only to relation `definition`"));
    }
    if p.line.is_some() && (matches!(relation, Relation::Implementations | Relation::Importers) || p.path.is_none()) {
        return Err(shape_error(
            "`line` needs `path`: it picks one of several same-named definitions, or locates the reference \
             for relation `definition`",
        ));
    }
    match relation {
        Relation::Callers => match p.path {
            Some(path) => {
                let callers = FindCallersParams {
                    path,
                    name: p.name,
                    substring,
                    kind: None,
                    line: p.line,
                    limit: p.limit,
                    max_tokens: budget(p.max_tokens),
                    cursor: p.cursor,
                };
                run_callers(state, callers).await
            }
            None => {
                state.await_cache_ready().await;
                let idx = super::helpers_calls::index_for_refs(&*state.store_read().await)?;
                let cache = state.shared.cache.load_full();
                run_find_references(
                    &*idx,
                    FindReferencesParams {
                        name: p.name,
                        substring,
                        limit: p.limit,
                        max_tokens: budget(p.max_tokens),
                        format: None,
                        cursor: p.cursor,
                    },
                    || state.lifecycle_notice(),
                    |hits| {
                        super::blocking::blocking(|| super::helpers_stale::flag_call_sites(state, &cache, hits));
                    },
                )
                .await
            }
        },
        Relation::Implementations => {
            if p.path.is_some() {
                return Err(shape_error("`path` does not apply to relation `implementations`"));
            }
            state.await_cache_ready().await;
            let idx = super::helpers_calls::index_for_refs(&*state.store_read().await)?;
            let cache = state.shared.cache.load_full();
            run_find_implementations(
                &*idx,
                FindImplementationsParams {
                    trait_name: p.name,
                    substring,
                    language: None,
                    limit: p.limit,
                    max_tokens: budget(p.max_tokens),
                    cursor: p.cursor,
                },
                &cache,
                || state.lifecycle_notice(),
            )
            .await
        }
        Relation::Importers => {
            if p.path.is_some() {
                return Err(shape_error("`path` does not apply to relation `importers`"));
            }
            if p.match_mode.is_some() {
                return Err(shape_error(
                    "relation `importers` matches `name` against import paths and takes no `match`",
                ));
            }
            run_dependents(state, p.name, p.limit, p.cursor, budget(p.max_tokens)).await
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
            let column = match p.column {
                Some(0) => return Err(shape_error("`column` is 1-based")),
                Some(column) => Some(column - 1),
                None => None,
            };
            let reference = GotoDefinitionParams {
                path,
                line,
                column,
                name: p.name,
            };
            super::helpers_intel::run_goto_definition(state, reference).await
        }
    }
}

/// Body of the `callers` mode: the name scan and the resolved-reference join over this session's
/// store ([`run_find_callers`]).
async fn run_callers(state: &ServerState, params: FindCallersParams) -> Result<CallToolResult, McpError> {
    state.await_cache_ready().await;
    let store = state.store_read().await;
    let idx = super::helpers_calls::index_for_refs(&store)?;
    let cache = state.shared.cache.load_full();
    run_find_callers(
        &store,
        &*idx,
        super::tool_contract::IndexedFiles::of(&state.shared),
        &cache,
        params,
        || state.lifecycle_notice(),
    )
    .await
}

/// Body of `code_refs` relation `importers`: a heuristic reverse lookup over every file's imports.
///
/// Streams the corpus instead of consulting a pre-flattened imports index. That index was a second
/// full copy of every import held for the process lifetime, to save one pass over data this tool
/// alone reads; the pass is now over outlines that the L1 cache serves and immediately drops.
async fn run_dependents(
    state: &ServerState,
    module: String,
    limit: Option<u32>,
    cursor: Option<super::cursor::Cursor>,
    max_tokens: Option<u32>,
) -> Result<CallToolResult, McpError> {
    use std::sync::atomic::Ordering;

    state.await_cache_ready().await;
    let generation = state.shared.cache_generation.load(Ordering::Relaxed);
    let skip = match cursor.as_ref() {
        Some(cursor) => {
            let (offset, snapshot) = cursor.decode_in_memory()?;
            if snapshot != generation {
                return Err(McpError::invalid_params(
                    "`code_refs`: the index changed since this cursor was issued. Call again without `cursor`."
                        .to_string(),
                    None,
                ));
            }
            offset as usize
        }
        None => 0,
    };
    let limit = limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    if module.is_empty() {
        return Err(McpError::invalid_params(
            "`code_refs`: `name` is empty; pass the module or import path to look for.".to_string(),
            None,
        ));
    }
    let finder = memchr::memmem::Finder::new(module.as_bytes());
    let mut paths: Vec<crate::path::RelPath> = Vec::new();
    let cache = state.shared.cache.load_full();
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
    let budgeted = super::budget::apply_budget(page, max_tokens);
    let next = skip + budgeted.items.len();
    json_result(&DependentsResponse {
        total,
        budgeted: budgeted.budgeted,
        paths: budgeted.items,
        next_cursor: (next < total).then(|| super::cursor::Cursor::encode_in_memory(next as u64, generation)),
        notice: state.lifecycle_notice(),
    })
}
