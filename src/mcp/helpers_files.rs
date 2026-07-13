//! `run_list_files` + `run_find_files` — kept in their own file so `tools.rs` and `helpers.rs`
//! stay under the 1000-line cap as the MCP surface grows. Both enumerate indexed paths and
//! share the same limit/cursor/token-budget pagination shape; `find_files` additionally scores
//! each candidate with `nucleo-matcher` before paginating.

use std::sync::atomic::Ordering;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::types::{
    FindFilesEntry, FindFilesParams, FindFilesResponse, ListFilesEntry, ListFilesParams, ListFilesResponse,
};

/// Body of the `list_files` MCP tool: enumerate indexed paths with optional substring
/// (`path_contains`) and `language` filters, then paginate.
pub(super) async fn run_list_files(state: &ServerState, params: ListFilesParams) -> Result<CallToolResult, McpError> {
    // The `list_files` shim delegates immediately, so this IS the first statement of the tool body:
    // the cache-ready wait below is inside the measured region.
    let started = std::time::Instant::now();
    state.await_cache_ready().await;
    let format = super::toon::ResponseFormat::parse(params.format.as_deref());
    let (limit, limit_clamped) = super::tools::effective_list_limit(params.limit);
    let generation = state.cache_generation.load(Ordering::Relaxed);

    let skip = match params.cursor.as_ref() {
        Some(c) => {
            let (offset, snapshot_id) = c.decode_in_memory()?;
            if snapshot_id != generation {
                return super::toon::format_result(
                    &ListFilesResponse {
                        total: 0,
                        returned: 0,
                        truncated: false,
                        limit_clamped,
                        budgeted: false,
                        files: Vec::new(),
                        next_cursor: None,
                        cursor_invalidated: true,
                        notice: state.lifecycle_notice(),
                        elapsed_us: super::helpers::elapsed_us(started),
                    },
                    format,
                );
            }
            offset as usize
        }
        None => 0,
    };
    let store = state.store.read().await;

    let path_finder = params
        .path_contains
        .as_ref()
        .map(|n| memchr::memmem::Finder::new(n.as_bytes()));
    let lang_filter = params.language.as_deref();

    let mut files: Vec<ListFilesEntry> = Vec::with_capacity(limit.min(256));
    let mut total: usize = 0;
    let mut seen: usize = 0;
    for (p, e) in &store.index.files {
        let path_ok = path_finder.as_ref().is_none_or(|f| f.find(p.as_bytes()).is_some());
        let lang_ok = lang_filter.is_none_or(|l| e.language == l);
        if !(path_ok && lang_ok) {
            continue;
        }
        if seen < skip {
            seen += 1;
            continue;
        }
        seen += 1;
        total += 1;
        if files.len() < limit {
            files.push(ListFilesEntry {
                path: p.clone(),
                language: e.language.clone(),
                size_bytes: e.size_bytes,
            });
        }
    }
    let truncated = total > limit;
    let budget = super::budget::apply_budget(files, params.max_tokens);
    let files = budget.items;
    let budgeted = budget.budgeted;
    let next_cursor = if total > files.len() {
        Some(super::cursor::Cursor::encode_in_memory(
            (skip + files.len()) as u64,
            generation,
        ))
    } else {
        None
    };

    super::toon::format_result(
        &ListFilesResponse {
            total,
            returned: files.len(),
            truncated,
            limit_clamped,
            budgeted,
            files,
            next_cursor,
            cursor_invalidated: false,
            notice: state.lifecycle_notice(),
            elapsed_us: super::helpers::elapsed_us(started),
        },
        format,
    )
}

/// Body of the `find_files` MCP tool: fuzzy subsequence match over indexed paths (fzf/fd-style),
/// ranked by `nucleo-matcher` score, with optional `path_prefix` / `language` pre-filters.
///
/// Sourced from `MapCache::by_path` (already sorted, in-RAM, no store lock needed) — the
/// `await_cache_ready()` call above guarantees it is populated before this runs. Paths that
/// aren't valid UTF-8 are skipped — `nucleo-matcher` scores `str`, not raw bytes.
pub(super) async fn run_find_files(state: &ServerState, params: FindFilesParams) -> Result<CallToolResult, McpError> {
    // The `find_files` shim delegates immediately, so this IS the first statement of the tool body:
    // the cache-ready wait below is inside the measured region.
    let started = std::time::Instant::now();
    state.await_cache_ready().await;
    let format = super::toon::ResponseFormat::parse(params.format.as_deref());
    let (limit, limit_clamped) = super::tools::effective_list_limit(params.limit);
    let generation = state.cache_generation.load(Ordering::Relaxed);

    let skip = match params.cursor.as_ref() {
        Some(c) => {
            let (offset, snapshot_id) = c.decode_in_memory()?;
            if snapshot_id != generation {
                return super::toon::format_result(
                    &FindFilesResponse {
                        total: 0,
                        returned: 0,
                        truncated: false,
                        limit_clamped,
                        budgeted: false,
                        files: Vec::new(),
                        next_cursor: None,
                        cursor_invalidated: true,
                        notice: state.lifecycle_notice(),
                        elapsed_us: super::helpers::elapsed_us(started),
                    },
                    format,
                );
            }
            offset as usize
        }
        None => 0,
    };

    let cache = state.cache.load_full();
    let prefix_filter = params.path_prefix.as_deref();
    let lang_filter = params.language.as_deref();

    let pattern = Pattern::parse(&params.query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let mut utf32_buf: Vec<char> = Vec::new();

    let mut scored: Vec<(u32, FindFilesEntry)> = Vec::new();
    for (p, l1) in cache.by_path.iter() {
        let lang_ok = lang_filter.is_none_or(|l| l1.language == l);
        if !lang_ok {
            continue;
        }
        let Some(path_str) = p.as_str() else {
            continue;
        };
        let prefix_ok = prefix_filter.is_none_or(|pre| path_str.starts_with(pre));
        if !prefix_ok {
            continue;
        }
        let haystack = Utf32Str::new(path_str, &mut utf32_buf);
        let Some(score) = pattern.score(haystack, &mut matcher) else {
            continue;
        };
        scored.push((
            score,
            FindFilesEntry {
                path: p.clone(),
                language: l1.language.clone(),
                size_bytes: l1.size_bytes,
                score,
            },
        ));
    }
    // ~keep: descending score, stable tie-break on path (ascending) so identically-scored
    // ~keep: entries have a deterministic order across calls/pages.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));

    // ~keep: `total` mirrors `list_files`'s convention: matches remaining from `skip` onward
    // ~keep: (not the grand total across all pages), so `total > limit` / `total > files.len()`
    // ~keep: read the same way.
    let total = scored.len().saturating_sub(skip);
    let page: Vec<FindFilesEntry> = scored
        .into_iter()
        .skip(skip)
        .take(limit)
        .map(|(_, entry)| entry)
        .collect();
    let truncated = total > limit;

    let budget = super::budget::apply_budget(page, params.max_tokens);
    let files = budget.items;
    let budgeted = budget.budgeted;
    let next_cursor = if total > files.len() {
        Some(super::cursor::Cursor::encode_in_memory(
            (skip + files.len()) as u64,
            generation,
        ))
    } else {
        None
    };

    super::toon::format_result(
        &FindFilesResponse {
            total,
            returned: files.len(),
            truncated,
            limit_clamped,
            budgeted,
            files,
            next_cursor,
            cursor_invalidated: false,
            notice: state.lifecycle_notice(),
            elapsed_us: super::helpers::elapsed_us(started),
        },
        format,
    )
}
