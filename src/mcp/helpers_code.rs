//! The `code` domain dispatcher, plus the bodies of its `outline`, `symbols` and `dependents`
//! modes.
//!
//! [`run_code`] is what the `#[tool]` shim and the CLI both call: it validates the flat
//! [`CodeParams`] against the selected [`CodeMode`] and delegates to the per-mode body. The other
//! bodies stay where they already lived — `helpers_files` (`files`/`find`), `helpers_grep`
//! (`grep`), `helpers_calls` (`references`/`callers`), `helpers_impls` (`implementations`),
//! `helpers_intel` (`definition`), `helpers_compress` (`expand`), `helpers_code_search`
//! (`semantic`/`chunk`) — so consolidation moved the surface, not the queries.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{
    RefsSource, SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX, elapsed_us, json_result, kind_to_str, parse_kind,
    run_find_callers, run_find_files, run_find_implementations, run_find_references, run_list_files,
    run_workspace_grep,
};
use super::mode::{CodeMode, reject_unsupported};
use super::types::{
    CallView, DependentsResponse, DocView, FindCallersParams, FindFilesParams, FindReferencesParams,
    GotoDefinitionParams, ImportView, ListFilesParams, OutlineParams, OutlineResponse, SearchHitView, SearchResponse,
    SearchSymbolsParams, SymbolView, WorkspaceGrepParams,
};
use super::types_code::{CodeParams, GetChunkParams, SearchCodeParams};
use super::types_compress::ExpandParams;
use super::types_impls::FindImplementationsParams;
use crate::query;

/// `definition`'s column default: the start of the line.
const DEFAULT_COLUMN: u32 = 0;
/// `grep` ships one line of context on either side of a hit unless told otherwise.
const DEFAULT_INCLUDE_CONTEXT: bool = true;

/// Fail a mode that was given a field belonging to some other mode.
///
/// Inverted against `allowed` rather than listing every rejected field per mode: with thirteen
/// modes and twenty-four sibling fields, an explicit per-mode reject list is where a newly added
/// field silently becomes accept-everywhere.
fn reject_foreign_fields(mode: CodeMode, present: &[(&str, bool)], allowed: &[&str]) -> Result<(), McpError> {
    let foreign: Vec<(&str, bool)> = present
        .iter()
        .filter(|(field, _)| !allowed.contains(field))
        .copied()
        .collect();
    reject_unsupported(CodeMode::DOMAIN, mode.as_str(), &foreign)
}

/// Unwrap a field this mode cannot run without, naming the exact `mode`/field pair.
fn require_field<T>(mode: CodeMode, field: &str, value: Option<T>) -> Result<T, McpError> {
    value.ok_or_else(|| McpError::invalid_params(format!("`code` mode=\"{}\" requires `{field}`", mode.as_str()), None))
}

/// Reported when a retrieval mode's feature is missing from this build.
#[cfg(not(feature = "code-search"))]
fn not_enabled(mode: CodeMode, feature: &'static str) -> Result<CallToolResult, McpError> {
    Err(McpError::invalid_request(
        format!(
            "`code` mode=\"{}\" requires the `{feature}` feature, which is not compiled into this \
             basemind binary. Rebuild with `--features {feature}` (the published release binary \
             includes it).",
            mode.as_str()
        ),
        None,
    ))
}

/// Dispatch the single `code` tool onto the per-operation helper its `mode` selects.
///
/// Fields belonging to another mode are rejected rather than dropped: a silently ignored
/// `path_contains` on a `symbols` call reads to an agent as a successful scoped search.
pub(super) async fn run_code(state: &ServerState, params: CodeParams) -> Result<CallToolResult, McpError> {
    let CodeParams {
        mode,
        path,
        name,
        query: text_query,
        pattern,
        trait_name,
        module,
        kind,
        language,
        path_contains,
        path_prefix,
        line,
        column,
        l2,
        include_context,
        limit,
        max_tokens,
        format,
        cursor,
        lane,
        rerank,
        rerank_preset,
        rerank_top_k,
        chunk_id,
        byte_start,
    } = params;
    let present = [
        ("path", path.is_some()),
        ("name", name.is_some()),
        ("query", text_query.is_some()),
        ("pattern", pattern.is_some()),
        ("trait_name", trait_name.is_some()),
        ("module", module.is_some()),
        ("kind", kind.is_some()),
        ("language", language.is_some()),
        ("path_contains", path_contains.is_some()),
        ("path_prefix", path_prefix.is_some()),
        ("line", line.is_some()),
        ("column", column.is_some()),
        ("l2", l2.is_some()),
        ("include_context", include_context.is_some()),
        ("limit", limit.is_some()),
        ("max_tokens", max_tokens.is_some()),
        ("format", format.is_some()),
        ("cursor", cursor.is_some()),
        ("lane", lane.is_some()),
        ("rerank", rerank.is_some()),
        ("rerank_preset", rerank_preset.is_some()),
        ("rerank_top_k", rerank_top_k.is_some()),
        ("chunk_id", chunk_id.is_some()),
        ("byte_start", byte_start.is_some()),
    ];
    reject_foreign_fields(mode, &present, allowed_fields(mode))?;

    // Timed from here, matching the pre-consolidation shims: the cache-ready wait a cold server
    // pays is part of the reported `elapsed_us` (such responses also carry a lifecycle `notice`).
    let started = std::time::Instant::now();

    match mode {
        CodeMode::Outline => {
            run_outline(
                state,
                OutlineParams {
                    path: require_field(mode, "path", path)?,
                    l2: l2.unwrap_or(false),
                    max_tokens,
                    format,
                },
                started,
            )
            .await
        }
        CodeMode::Symbols => {
            run_search_symbols(
                state,
                SearchSymbolsParams {
                    needle: require_field(mode, "name", name)?,
                    kind,
                    limit,
                    max_tokens,
                    format,
                    cursor,
                },
                started,
            )
            .await
        }
        CodeMode::Grep => {
            state.await_cache_ready().await;
            run_workspace_grep(
                state,
                WorkspaceGrepParams {
                    pattern: require_field(mode, "pattern", pattern)?,
                    language,
                    path_contains,
                    limit,
                    max_tokens,
                    format,
                    include_context: include_context.unwrap_or(DEFAULT_INCLUDE_CONTEXT),
                    cursor,
                },
                started,
            )
        }
        CodeMode::Files => {
            run_list_files(
                state,
                ListFilesParams {
                    path_contains,
                    language,
                    limit,
                    max_tokens,
                    format,
                    cursor,
                },
            )
            .await
        }
        CodeMode::Find => {
            run_find_files(
                state,
                FindFilesParams {
                    query: require_field(mode, "query", text_query)?,
                    path_prefix,
                    language,
                    limit,
                    max_tokens,
                    format,
                    cursor,
                },
            )
            .await
        }
        CodeMode::Definition => {
            super::helpers_intel::run_goto_definition(
                state,
                GotoDefinitionParams {
                    path: require_field(mode, "path", path)?,
                    line: require_field(mode, "line", line)?,
                    column: column.unwrap_or(DEFAULT_COLUMN),
                },
            )
            .await
        }
        CodeMode::References => {
            let name = require_field(mode, "name", name)?;
            state.await_cache_ready().await;
            let store = state.shared.store.read().await;
            let idx = store.index_db.as_ref().cloned();
            drop(store);
            let cache = state.shared.cache.load_full();
            run_find_references(
                idx.as_ref(),
                FindReferencesParams {
                    name,
                    limit,
                    max_tokens,
                    format,
                    cursor,
                },
                &cache,
                state.lifecycle_notice(),
                started,
            )
        }
        CodeMode::Callers => {
            let callers = FindCallersParams {
                path: require_field(mode, "path", path)?,
                name: require_field(mode, "name", name)?,
                kind,
                limit,
                max_tokens,
                cursor,
            };
            run_callers(state, callers, started).await
        }
        CodeMode::Implementations => {
            let trait_name = require_field(mode, "trait_name", trait_name)?;
            state.await_cache_ready().await;
            let store = state.shared.store.read().await;
            let idx = store.index_db.as_ref().cloned();
            drop(store);
            let cache = state.shared.cache.load_full();
            run_find_implementations(
                idx.as_ref(),
                FindImplementationsParams {
                    trait_name,
                    language,
                    limit,
                    max_tokens,
                    cursor,
                },
                &cache,
                state.lifecycle_notice(),
                started,
            )
        }
        CodeMode::Dependents => run_dependents(state, require_field(mode, "module", module)?, started).await,
        CodeMode::Expand => {
            super::helpers_compress::run_expand(
                state,
                ExpandParams {
                    path: require_field(mode, "path", path)?,
                    name: require_field(mode, "name", name)?,
                    kind,
                },
            )
            .await
        }
        CodeMode::Semantic => {
            let search = SearchCodeParams {
                query: require_field(mode, "query", text_query)?,
                limit,
                max_tokens,
                format,
                mode: lane,
                reranker_enabled: rerank,
                reranker_preset: rerank_preset,
                reranker_top_k: rerank_top_k,
            };
            #[cfg(feature = "code-search")]
            {
                super::helpers_code_search::run_search_code(state, search).await
            }
            #[cfg(not(feature = "code-search"))]
            {
                let _ = search;
                not_enabled(mode, "code-search")
            }
        }
        CodeMode::Chunk => {
            let fetch = GetChunkParams {
                path: require_field(mode, "path", path)?,
                chunk_id,
                byte_start,
            };
            #[cfg(feature = "code-search")]
            {
                super::helpers_code_search::run_get_chunk(state, fetch).await
            }
            #[cfg(not(feature = "code-search"))]
            {
                let _ = fetch;
                not_enabled(mode, "code-search")
            }
        }
    }
}

/// The sibling fields each mode accepts. Everything else present on the call is rejected by
/// [`reject_foreign_fields`], so a parameter an agent believed took effect never silently doesn't.
fn allowed_fields(mode: CodeMode) -> &'static [&'static str] {
    match mode {
        CodeMode::Outline => &["path", "l2", "max_tokens", "format"],
        CodeMode::Symbols => &["name", "kind", "limit", "max_tokens", "format", "cursor"],
        CodeMode::Grep => &[
            "pattern",
            "language",
            "path_contains",
            "limit",
            "max_tokens",
            "format",
            "include_context",
            "cursor",
        ],
        CodeMode::Files => &["path_contains", "language", "limit", "max_tokens", "format", "cursor"],
        CodeMode::Find => &[
            "query",
            "path_prefix",
            "language",
            "limit",
            "max_tokens",
            "format",
            "cursor",
        ],
        CodeMode::Definition => &["path", "line", "column"],
        CodeMode::References => &["name", "limit", "max_tokens", "format", "cursor"],
        CodeMode::Callers => &["path", "name", "kind", "limit", "max_tokens", "cursor"],
        CodeMode::Implementations => &["trait_name", "language", "limit", "max_tokens", "cursor"],
        CodeMode::Dependents => &["module"],
        CodeMode::Expand => &["path", "name", "kind"],
        CodeMode::Semantic => &[
            "query",
            "limit",
            "max_tokens",
            "format",
            "lane",
            "rerank",
            "rerank_preset",
            "rerank_top_k",
        ],
        CodeMode::Chunk => &["path", "chunk_id", "byte_start"],
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
        &state.shared.root,
        &cache,
        params,
        state.lifecycle_notice(),
        started,
    )
    .await
}

/// Project an L1 filemap onto the outline response's symbol + import views.
fn l1_views(l1: &crate::extract::FileMapL1) -> (Vec<SymbolView>, Vec<ImportView>) {
    let symbols = l1
        .symbols
        .iter()
        .map(|s| SymbolView {
            name: s.name.clone(),
            kind: kind_to_str(s.kind),
            start_row: s.start_row,
            start_col: s.start_col,
            start_byte: s.start_byte,
            end_byte: s.end_byte,
            signature: s.signature.clone(),
        })
        .collect();
    let imports = l1
        .imports
        .iter()
        .map(|i| ImportView {
            module: i.module.clone(),
            raw: i.raw.clone(),
            start_byte: i.start_byte,
        })
        .collect();
    (symbols, imports)
}

/// Assemble the L1 half of an outline response. `calls` / `docs` are filled in afterwards on the
/// `l2` path.
fn outline_response(path: &crate::path::RelPath, l1: &crate::extract::FileMapL1) -> OutlineResponse {
    let (symbols, imports) = l1_views(l1);
    OutlineResponse {
        path: path.clone(),
        language: l1.language.clone(),
        size_bytes: l1.size_bytes,
        had_errors: l1.had_errors,
        error_count: l1.error_count,
        budgeted: false,
        symbols,
        imports,
        calls: None,
        docs: None,
        l2_status: None,
        notice: None,
        elapsed_us: 0,
    }
}

/// Body of the `outline` mode: a file's structure from the in-RAM map when it is warm, else from
/// the content-addressed blob, plus the L2 call/doc sidecar when `l2` was requested.
async fn run_outline(
    state: &ServerState,
    params: OutlineParams,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let mut response = if params.l2 {
        let store = state.shared.store.read().await;
        let l1 = query::file_outline(&store, &params.path)
            .map_err(|e| McpError::invalid_params(format!("file_outline({}): {e}", params.path), None))?;
        let mut r = outline_response(&params.path, &l1);
        let entry = store
            .lookup(&params.path)
            .ok_or_else(|| McpError::internal_error("file not indexed after outline succeeded", None))?;
        match store.read_l2_by_hex(&entry.hash_hex) {
            Ok(Some(l2)) => {
                r.calls = Some(
                    l2.calls
                        .iter()
                        .map(|c| CallView {
                            callee: c.callee.clone(),
                            start_byte: c.start_byte,
                        })
                        .collect(),
                );
                r.docs = Some(
                    l2.docs
                        .iter()
                        .map(|d| DocView {
                            text: d.text.clone(),
                            start_byte: d.start_byte,
                        })
                        .collect(),
                );
            }
            Ok(None) => {
                r.l2_status = Some("missing — run `basemind code outline <path> --l2` to materialize");
            }
            Err(e) => {
                r.l2_status = Some("error");
                return Err(McpError::internal_error(format!("read_l2: {e}"), None));
            }
        }
        r
    } else {
        let cache = state.shared.cache.load();
        if let Some(l1) = cache.get(&params.path) {
            outline_response(&params.path, &l1)
        } else {
            let store = state.shared.store.read().await;
            let l1 = query::file_outline(&store, &params.path)
                .map_err(|e| McpError::invalid_params(format!("file_outline({}): {e}", params.path), None))?;
            outline_response(&params.path, &l1)
        }
    };
    response.notice = state.lifecycle_notice();

    if params.max_tokens.is_some() {
        let budgeted = super::budget::apply_budget(std::mem::take(&mut response.symbols), params.max_tokens);
        response.symbols = budgeted.items;
        response.budgeted = budgeted.budgeted;
    }
    response.elapsed_us = elapsed_us(started);
    super::toon::format_result(&response, super::toon::ResponseFormat::parse(params.format.as_deref()))
}

/// Body of the `symbols` mode: a case-sensitive substring sweep of every indexed symbol name in the
/// in-RAM map, bounded by [`search_max_total`](super::tools::search_max_total).
async fn run_search_symbols(
    state: &ServerState,
    params: SearchSymbolsParams,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    use std::sync::atomic::Ordering;

    state.await_cache_ready().await;
    let format = super::toon::ResponseFormat::parse(params.format.as_deref());
    let kind = params.kind.as_deref().map(parse_kind).transpose()?;
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    let generation = state.shared.cache_generation.load(Ordering::Relaxed);

    let empty = |cursor_invalidated: bool| SearchResponse {
        total: 0,
        total_is_partial: false,
        truncated: false,
        budgeted: false,
        results: Vec::new(),
        next_cursor: None,
        cursor_invalidated,
        notice: state.lifecycle_notice(),
        elapsed_us: elapsed_us(started),
    };

    let skip = match params.cursor.as_ref() {
        Some(c) => {
            let (offset, snapshot_id) = c.decode_in_memory()?;
            if snapshot_id != generation {
                return super::toon::format_result(&empty(true), format);
            }
            offset as usize
        }
        None => 0,
    };

    if params.needle.is_empty() {
        return super::toon::format_result(&empty(false), format);
    }
    let finder = memchr::memmem::Finder::new(params.needle.as_bytes());
    let max_total = super::tools::search_max_total(limit);
    let mut results: Vec<SearchHitView> = Vec::with_capacity(limit);
    let mut total: usize = 0;
    let mut seen: usize = 0;
    let mut total_is_partial = false;
    let cache = state.shared.cache.load_full();
    // Streamed rather than iterated over a resident whole-corpus map: each hit is projected into
    // an owned `SearchHitView` and the outline is dropped, so the live set is one chunk however
    // large the repo is. `max_total` still cuts the scan at the same point it always did.
    cache.for_each_while(|path, l1| {
        for sym in &l1.symbols {
            if finder.find(sym.name.as_bytes()).is_none() {
                continue;
            }
            if let Some(k) = kind
                && sym.kind != k
            {
                continue;
            }
            if seen < skip {
                seen += 1;
                continue;
            }
            seen += 1;
            total += 1;
            if results.len() < limit {
                results.push(SearchHitView {
                    path: path.clone(),
                    name: sym.name.clone(),
                    kind: kind_to_str(sym.kind),
                    start_row: sym.start_row,
                    start_col: sym.start_col,
                    signature: sym.signature.clone(),
                });
            }
            if total >= max_total {
                total_is_partial = true;
                return false;
            }
        }
        true
    });
    let truncated = total > limit || total_is_partial;
    let budget = super::budget::apply_budget(results, params.max_tokens);
    let results = budget.items;
    let budgeted = budget.budgeted;
    let next_cursor = if total > results.len() {
        Some(super::cursor::Cursor::encode_in_memory(
            (skip + results.len()) as u64,
            generation,
        ))
    } else {
        None
    };
    super::toon::format_result(
        &SearchResponse {
            total,
            total_is_partial,
            truncated,
            budgeted,
            results,
            next_cursor,
            cursor_invalidated: false,
            notice: state.lifecycle_notice(),
            elapsed_us: elapsed_us(started),
        },
        format,
    )
}

/// Body of the `dependents` mode: a heuristic reverse lookup over every file's imports.
///
/// Streams the corpus instead of consulting a pre-flattened imports index. That index was a second
/// full copy of every import held for the process lifetime, to save one pass over data this tool
/// alone reads; the pass is now over outlines that the L1 cache serves and immediately drops.
async fn run_dependents(
    state: &ServerState,
    module: String,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    state.await_cache_ready().await;
    let finder = memchr::memmem::Finder::new(module.as_bytes());
    let mut paths: Vec<crate::path::RelPath> = Vec::new();
    // The stream is in path order, so `paths` comes out sorted — the order `dependents_of`
    // produced with an explicit sort.
    state.shared.cache.load().for_each(|path, l1| {
        if crate::extract::l3::imports_mention(&module, &finder, &l1.imports) {
            paths.push(path.clone());
        }
    });
    json_result(&DependentsResponse {
        module,
        paths,
        notice: state.lifecycle_notice(),
        elapsed_us: elapsed_us(started),
    })
}
