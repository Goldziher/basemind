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

/// Body of the `code` tool's `files` mode: enumerate indexed paths with optional substring
/// (`path_contains`) and `language` filters, then paginate.
pub(super) async fn run_list_files(state: &ServerState, params: ListFilesParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    state.await_cache_ready().await;
    let format = super::toon::ResponseFormat::parse(params.format.as_deref());
    let (limit, limit_clamped) = super::tools::effective_list_limit(params.limit);
    let generation = state.shared.cache_generation.load(Ordering::Relaxed);

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
    let store = state.shared.store.read().await;

    let path_finder = params
        .path_contains
        .as_ref()
        .map(|n| memchr::memmem::Finder::new(n.as_bytes()));
    let lang_filter = params.language.as_deref();

    let mut files: Vec<ListFilesEntry> = Vec::with_capacity(limit.min(256));
    let mut total: usize = 0;
    let mut seen: usize = 0;
    // Code map first, then the document tier (markdown, config, data, PDFs ...) in sorted order:
    // a file listing covers every indexed path, not just the code-mapped ones.
    let code = store
        .index
        .files
        .iter()
        .map(|(p, e)| (p, e.language.clone(), e.size_bytes));
    #[cfg(feature = "documents")]
    let docs = state
        .shared
        .cache
        .load_full()
        .doc_paths
        .iter()
        .map(|p| {
            let language = p.as_str().map_or_else(|| "document".to_string(), doc_language);
            (p.clone(), language, store.lookup_doc(p).map_or(0, |d| d.size_bytes))
        })
        .collect::<Vec<_>>();
    #[cfg(feature = "documents")]
    let docs = docs.iter().map(|(p, l, sz)| (p, l.clone(), *sz));
    #[cfg(not(feature = "documents"))]
    let docs = std::iter::empty::<(&crate::path::RelPath, String, u64)>();
    for (p, language, size_bytes) in code.chain(docs) {
        let path_ok = path_finder.as_ref().is_none_or(|f| f.find(p.as_bytes()).is_some());
        let lang_ok = lang_filter.is_none_or(|l| language == l);
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
                language,
                size_bytes,
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

/// Body of the `code` tool's `find` mode: fuzzy subsequence match over indexed paths (fzf/fd-style),
/// ranked by `nucleo-matcher` score, with optional `path_prefix` / `language` pre-filters.
///
/// Sourced from the `MapCache` file view (already sorted, in-RAM, no store lock needed) — the
/// `await_cache_ready()` call above guarantees it is populated before this runs. Paths that
/// aren't valid UTF-8 are skipped — `nucleo-matcher` scores `str`, not raw bytes.
pub(super) async fn run_find_files(state: &ServerState, params: FindFilesParams) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    state.await_cache_ready().await;
    let format = super::toon::ResponseFormat::parse(params.format.as_deref());
    let (limit, limit_clamped) = super::tools::effective_list_limit(params.limit);
    let generation = state.shared.cache_generation.load(Ordering::Relaxed);

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

    let cache = state.shared.cache.load_full();
    let prefix_filter = params.path_prefix.as_deref();
    let lang_filter = params.language.as_deref();

    let pattern = Pattern::parse(&params.query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let mut utf32_buf: Vec<char> = Vec::new();

    let mut scored: Vec<(u32, FindFilesEntry)> = Vec::new();
    // Language, size and path are all the fuzzy-file search needs, and all three live in the
    // symbol-free file view — so this whole scan reads no outline and touches no blob.
    for (p, meta) in cache.file_metas() {
        let lang_ok = lang_filter.is_none_or(|l| *meta.language == *l);
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
                language: meta.language.to_string(),
                size_bytes: meta.size_bytes,
                score,
            },
        ));
    }
    // The document tier (markdown, JSON, YAML, PDFs ...) is not code-mapped, but a file finder
    // must cover every indexed path. Docs carry no size in the cache view; it is filled from the
    // store for the returned page below.
    #[cfg(feature = "documents")]
    for p in cache.doc_paths.iter() {
        let Some(path_str) = p.as_str() else {
            continue;
        };
        let language = doc_language(path_str);
        if lang_filter.is_some_and(|l| l != language) || prefix_filter.is_some_and(|pre| !path_str.starts_with(pre)) {
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
                language,
                size_bytes: 0,
                score,
            },
        ));
    }
    rank_and_cut(&mut scored);

    // ~keep: `total` mirrors `list_files`'s convention: matches remaining from `skip` onward
    // ~keep: (not the grand total across all pages), so `total > limit` / `total > files.len()`
    // ~keep: read the same way.
    let total = scored.len().saturating_sub(skip);
    let mut page: Vec<FindFilesEntry> = scored
        .into_iter()
        .skip(skip)
        .take(limit)
        .map(|(_, entry)| entry)
        .collect();
    #[cfg(feature = "documents")]
    if page.iter().any(|e| cache.contains_doc(&e.path)) {
        let store = state.shared.store.read().await;
        for e in &mut page {
            if let Some(doc) = store.lookup_doc(&e.path) {
                e.size_bytes = doc.size_bytes;
            }
        }
    }
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

/// A candidate must score at least this fraction of the best candidate's score to be returned.
/// nucleo accepts any subsequence, so a short query matches thousands of paths at a score a small
/// fraction of the best; returning them only fills the page with noise.
///
/// Calibrated with the `find` eval (`benchmarks/eval/gen_find.py`, 90 tasks over code and
/// documents, limit 10): precision rises monotonically with the floor (0.44 at 50, 0.62 at 85,
/// 0.70 at 95) while recall, hit@1 and MRR stay flat until 95; at 100 (ties with the top score
/// only) one correct hit is lost. 85 keeps a ten-point margin from that cliff.
const RELATIVE_SCORE_FLOOR_PCT: u32 = 85;

/// Order candidates by descending score (path ascending on ties, so pages are deterministic) and
/// drop everything scoring below [`RELATIVE_SCORE_FLOOR_PCT`] percent of the top score.
fn rank_and_cut(scored: &mut Vec<(u32, FindFilesEntry)>) {
    // ~keep: descending score, stable tie-break on path (ascending) so identically-scored
    // ~keep: entries have a deterministic order across calls/pages.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));
    if let Some(&(top, _)) = scored.first() {
        let floor = (u64::from(top) * u64::from(RELATIVE_SCORE_FLOOR_PCT) / 100) as u32;
        let keep = scored.partition_point(|(s, _)| *s >= floor);
        scored.truncate(keep);
    }
}

/// Language label for a document-tier path: the grammar name when one resolves (`markdown`,
/// `json`, `yaml` ...), else the lowercased extension (`pdf`, `docx`), else `document`.
#[cfg(feature = "documents")]
fn doc_language(path: &str) -> String {
    if let Some(l) = crate::lang::detect(std::path::Path::new(path)) {
        return l.to_string();
    }
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map_or_else(|| "document".to_string(), str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, score: u32) -> (u32, FindFilesEntry) {
        (
            score,
            FindFilesEntry {
                path: path.into(),
                language: "x".into(),
                size_bytes: 0,
                score,
            },
        )
    }

    #[test]
    fn cut_drops_far_below_top_and_orders_deterministically() {
        let mut v = vec![entry("b", 100), entry("a", 100), entry("z", 85), entry("low", 84)];
        rank_and_cut(&mut v);
        let paths: Vec<_> = v.iter().map(|(_, e)| e.path.to_string()).collect();
        assert_eq!(paths, ["a", "b", "z"], "85 is kept (>= floor), 84 is cut");
    }

    #[test]
    fn cut_keeps_everything_when_scores_are_close_and_handles_empty() {
        let mut v = vec![entry("a", 10), entry("b", 9)];
        rank_and_cut(&mut v);
        assert_eq!(v.len(), 2);
        let mut e: Vec<(u32, FindFilesEntry)> = Vec::new();
        rank_and_cut(&mut e);
        assert!(e.is_empty());
    }

    #[cfg(feature = "documents")]
    #[test]
    fn doc_language_labels() {
        assert_eq!(doc_language("docs/README.md"), "markdown");
        assert_eq!(doc_language("a/b.pdf"), "pdf");
        assert_eq!(doc_language("LICENSEFILE"), "document");
    }
}
