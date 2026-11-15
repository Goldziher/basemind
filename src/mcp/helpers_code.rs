//! Bodies of the code verb tools: `code_outline`, `code_symbols`, `code_grep`, `code_search`,
//! `code_refs` and `find_files`.
//!
//! Each `pub(super) async fn` here is what both the `#[tool]` shim in `tools.rs` and the matching
//! `basemind code …` command run. They translate the tool's wire arguments
//! ([`super::types_tools`]) into the per-operation shapes the query bodies take; those bodies stay
//! where they already lived: `helpers_files`, `helpers_grep`, `helpers_calls`, `helpers_impls`,
//! `helpers_intel`, `helpers_code_search`. `code_read` is in `helpers_expand`.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{
    RefsSource, SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX, elapsed_us, json_result, kind_to_str, run_find_callers,
    run_find_files, run_find_implementations, run_find_references, run_list_files, run_workspace_grep,
};
use super::index_route::IndexRoute;
use super::tool_contract::{budget, not_indexed};
use super::types::{
    DependentsResponse, FindCallersParams, FindFilesParams, FindReferencesParams, GotoDefinitionParams,
    ListFilesParams, OutlineResponse, OutlineSymbol, SearchSymbolsParams, WorkspaceGrepParams,
};
use super::types_impls::FindImplementationsParams;
use super::types_tools::{
    CodeGrepParams, CodeOutlineParams, CodeRefsParams, CodeSearchParams, CodeSymbolsParams, FindFilesToolParams,
};

/// Symbols returned by one `code_outline` page when the caller passes no `limit`.
const OUTLINE_LIMIT_DEFAULT: u32 = 500;
/// Imports listed on the first `code_outline` page.
const OUTLINE_IMPORTS_MAX: usize = 200;

/// `code_grep`: content search over every indexed code file.
pub(super) async fn code_grep(state: &ServerState, p: CodeGrepParams) -> Result<CallToolResult, McpError> {
    if p.pattern.is_empty() {
        return Err(McpError::invalid_params(
            "`code_grep`: `pattern` is empty; pass the text to find.".to_string(),
            None,
        ));
    }
    let started = std::time::Instant::now();
    state.await_cache_ready().await;
    run_workspace_grep(
        state,
        WorkspaceGrepParams {
            pattern: p.pattern,
            language: p.language,
            path_contains: p.path,
            limit: p.limit,
            max_tokens: budget(p.max_tokens),
            format: None,
            include_context: p.context.unwrap_or(false),
            literal: !p.regex.unwrap_or(false),
            cursor: p.cursor,
        },
        started,
    )
    .await
}

/// `code_symbols`: find definitions by name.
pub(super) async fn code_symbols(state: &ServerState, p: CodeSymbolsParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    super::helpers_symbols::run_search_symbols(
        state,
        SearchSymbolsParams {
            needle: p.name,
            kind: p.kind,
            fuzzy: p.fuzzy.unwrap_or(false),
            limit: p.limit,
            max_tokens: budget(p.max_tokens),
            format: None,
            cursor: p.cursor,
        },
        started,
    )
    .await
}

/// `code_search`: semantic / hybrid search returning pointers.
pub(super) async fn code_search(state: &ServerState, p: CodeSearchParams) -> Result<CallToolResult, McpError> {
    #[cfg(feature = "code-search")]
    {
        use super::helpers_page::{RANKED_DEPTH, RANKED_LIMIT_DEFAULT, page_ranked, ranked_offset};

        let offset = ranked_offset(p.cursor.as_ref())?;
        let limit = p.limit.unwrap_or(RANKED_LIMIT_DEFAULT);
        let search = super::types_code::SearchCodeParams {
            query: p.query,
            limit: Some(RANKED_DEPTH),
            max_tokens: None,
            // The page is cut out of the structured response, which only the JSON encoding has, so
            // `[documents.output] format = "toon"` does not apply to this tool.
            format: Some("json".to_string()),
            mode: p.lane,
            reranker_enabled: None,
            reranker_preset: None,
            reranker_top_k: None,
        };
        let result = super::helpers_code_search::run_search_code(state, search).await?;
        page_ranked(result, "hits", offset, limit, budget(p.max_tokens))
    }
    #[cfg(not(feature = "code-search"))]
    {
        let _ = (state, p);
        Err(super::tool_contract::feature_missing("code_search", "code-search"))
    }
}

/// `find_files`: fuzzy file-name search, or a plain listing when `query` is omitted.
pub(super) async fn find_files(state: &ServerState, p: FindFilesToolParams) -> Result<CallToolResult, McpError> {
    match p.query.filter(|query| !query.trim().is_empty()) {
        Some(query) => {
            run_find_files(
                state,
                FindFilesParams {
                    query,
                    path_prefix: p.path_prefix,
                    language: p.language,
                    limit: p.limit,
                    max_tokens: budget(p.max_tokens),
                    format: None,
                    cursor: p.cursor,
                },
            )
            .await
        }
        None => {
            run_list_files(
                state,
                ListFilesParams {
                    path_contains: None,
                    path_prefix: p.path_prefix,
                    language: p.language,
                    limit: p.limit,
                    max_tokens: budget(p.max_tokens),
                    format: None,
                    cursor: p.cursor,
                },
            )
            .await
        }
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
    let started = std::time::Instant::now();
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
                run_callers(state, callers, started).await
            }
            None => {
                state.await_cache_ready().await;
                let route = IndexRoute::resolve(state).await;
                let cache = state.shared.cache.load_full();
                run_find_references(
                    &route,
                    FindReferencesParams {
                        name: p.name,
                        substring,
                        limit: p.limit,
                        max_tokens: budget(p.max_tokens),
                        format: None,
                        cursor: p.cursor,
                    },
                    &cache,
                    || state.lifecycle_notice(),
                    started,
                )
                .await
            }
        },
        Relation::Implementations => {
            if p.path.is_some() {
                return Err(shape_error("`path` does not apply to relation `implementations`"));
            }
            state.await_cache_ready().await;
            let route = IndexRoute::resolve(state).await;
            let cache = state.shared.cache.load_full();
            run_find_implementations(
                &route,
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
                started,
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
            run_dependents(state, p.name, p.limit, p.cursor, budget(p.max_tokens), started).await
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

/// Body of the `callers` mode. Resolves where the resolved-reference join runs before handing the
/// scan to [`run_find_callers`]: a daemon-hosted connection resolves in-process through the pool's
/// read-write index, a `daemon_writer` serve forwards to the daemon, and everything else reads its
/// own store.
async fn run_callers(
    state: &ServerState,
    params: FindCallersParams,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    state.await_cache_ready().await;
    let route = IndexRoute::resolve(state).await;
    let store = state.shared.store.read().await;
    let cache = state.shared.cache.load_full();
    #[cfg(all(feature = "comms", any(unix, windows)))]
    let refs = if let Some(host) = &state.shared.host {
        // ~keep Daemon-hosted: resolve in-process through the pool's read-write index (fixes the
        // ~keep Seam-B degradation) instead of dialing the daemon over its own socket.
        RefsSource::Host {
            host: std::sync::Arc::clone(host),
            root: state.shared.root.clone(),
        }
    } else if state.shared.daemon_writer {
        let client = super::helpers_comms::resolve_comms_client(state, None).await?;
        RefsSource::Daemon {
            client,
            root: state.shared.root.clone(),
        }
    } else {
        RefsSource::Local(&store)
    };
    #[cfg(not(all(feature = "comms", any(unix, windows))))]
    let refs = RefsSource::Local(&store);
    run_find_callers(
        &store,
        refs,
        &route,
        super::tool_contract::IndexedFiles::of(state),
        &cache,
        params,
        || state.lifecycle_notice(),
        started,
    )
    .await
}

/// 1-based line of the byte at `offset`, from the sorted offsets of every newline in the file.
fn line_of(newlines: &[usize], offset: usize) -> u32 {
    newlines.partition_point(|newline| *newline < offset) as u32 + 1
}

/// `code_outline`: one file's symbols with their 1-based line spans, plus its imports.
///
/// `end_line` is not in the index (a symbol records its byte span and start row), so it is derived
/// from the working-tree file. When that file no longer holds the indexed content the outline is
/// extracted from the file as it is and flagged `stale`: the indexed spans laid over other bytes
/// would end on the wrong lines.
pub(super) async fn code_outline(state: &ServerState, p: CodeOutlineParams) -> Result<CallToolResult, McpError> {
    use std::sync::atomic::Ordering;

    let started = std::time::Instant::now();
    let generation = state.shared.cache_generation.load(Ordering::Relaxed);
    // Path-keyed: served from the in-RAM map when it is warm, else from the one blob. Waiting for
    // the map here would make a one-shot CLI call decode the whole corpus to outline one file.
    let cache = state.shared.cache.load_full();
    let l1 = match cache.get(&p.path) {
        Some(l1) => l1,
        None => {
            let store = state.shared.store.read().await;
            std::sync::Arc::new(
                crate::query::file_outline(&store, &p.path).map_err(|_| not_indexed(state, "code_outline", &p.path))?,
            )
        }
    };

    let skip = match p.cursor.as_ref() {
        Some(cursor) => {
            let (offset, snapshot) = cursor.decode_in_memory()?;
            if snapshot != generation {
                return Err(McpError::invalid_params(
                    "`code_outline`: the index changed since this cursor was issued. Call again without `cursor`."
                        .to_string(),
                    None,
                ));
            }
            offset as usize
        }
        None => 0,
    };
    let limit = p.limit.unwrap_or(OUTLINE_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;

    let max_bytes = state.shared.config.scan.max_file_bytes;
    let (file, entry) = {
        let store = state.shared.store.read().await;
        let entry = store
            .lookup(&p.path)
            .cloned()
            .ok_or_else(|| super::tool_contract::not_indexed_in(&store, &state.shared.root, "code_outline", &p.path))?;
        (
            super::tool_contract::indexed_file(state, &store, "code_outline", &p.path)?,
            entry,
        )
    };
    let (l1, source, stale) = super::index_route::blocking(|| {
        let live = super::freshness::load(&file, (&entry).into(), max_bytes)
            .map_err(|reason| super::freshness::unloadable("code_outline", &p.path, reason, max_bytes))?;
        let l1 = if live.fresh {
            l1
        } else {
            std::sync::Arc::new(super::freshness::reparse(state, "code_outline", &p.path, &live.bytes)?)
        };
        Ok::<_, McpError>((l1, live.bytes, !live.fresh))
    })?;
    let newlines: Vec<usize> = memchr::memchr_iter(b'\n', &source).collect();

    let total = l1.symbols.len();
    let page: Vec<OutlineSymbol> = l1
        .symbols
        .iter()
        .skip(skip)
        .take(limit)
        .map(|symbol| {
            let line = symbol.start_row + 1;
            let end_line = line_of(&newlines, (symbol.end_byte as usize).min(source.len())).max(line);
            OutlineSymbol {
                name: symbol.name.clone(),
                kind: kind_to_str(symbol.kind),
                line,
                end_line,
                signature: symbol.signature.clone(),
            }
        })
        .collect();
    let budgeted = super::budget::apply_budget(page, budget(p.max_tokens));
    let symbols = budgeted.items;
    let next = skip + symbols.len();
    let imports: Vec<String> = if skip == 0 {
        l1.imports
            .iter()
            .take(OUTLINE_IMPORTS_MAX)
            .map(|import| import.module.clone().unwrap_or_else(|| import.raw.clone()))
            .collect()
    } else {
        Vec::new()
    };

    let imports_omitted = if skip == 0 {
        l1.imports.len().saturating_sub(OUTLINE_IMPORTS_MAX)
    } else {
        0
    };

    json_result(&OutlineResponse {
        path: p.path,
        language: l1.language.clone(),
        size_bytes: l1.size_bytes,
        had_errors: l1.had_errors,
        total,
        budgeted: budgeted.budgeted,
        stale,
        symbols,
        truncated: imports_omitted > 0,
        truncated_reason: (imports_omitted > 0).then_some("imports"),
        omitted: imports_omitted,
        imports,
        next_cursor: (next < total).then(|| super::cursor::Cursor::encode_in_memory(next as u64, generation)),
        notice: state.lifecycle_notice(),
        elapsed_us: elapsed_us(started),
    })
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
    started: std::time::Instant,
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
        module,
        total,
        budgeted: budgeted.budgeted,
        paths: budgeted.items,
        next_cursor: (next < total).then(|| super::cursor::Cursor::encode_in_memory(next as u64, generation)),
        notice: state.lifecycle_notice(),
        elapsed_us: elapsed_us(started),
    })
}
