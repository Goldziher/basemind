//! Body of `code_symbols`: find definitions by name, best match first.
//!
//! The sweep runs over the resident term index (names only) and ranks as it goes: a name equal to
//! the needle, then names it prefixes, then names that contain it, then, on request, names it is a
//! fuzzy subsequence of. Only the returned page is decoded from the L1 cache. The old sweep
//! returned substring matches in file order, so the definition asked for by its exact name could
//! sit anywhere in the page, or past it.

use std::sync::atomic::Ordering;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX, elapsed_us, kind_to_str, parse_kind};
use super::types::{SearchHitView, SearchResponse, SearchSymbolsParams};
use crate::path::RelPath;

/// The literal ranks, best first. A fuzzy match ranks after all of them.
const LITERAL_RANKS: [&str; 3] = ["exact", "prefix", "substring"];
const FUZZY: &str = "fuzzy";

/// Fuzzy candidates held while ranking. A needle of two letters is a subsequence of most names in
/// a large corpus; past this many the total is reported as partial instead of holding them all.
const FUZZY_CANDIDATES_MAX: usize = 50_000;

/// A fuzzy match scoring below this share of the best one is noise, not a near-miss.
const FUZZY_SCORE_FLOOR_PCT: u32 = 85;

/// One symbol that matched, before its outline is decoded.
struct Candidate<'a> {
    path: &'a RelPath,
    symbol: usize,
    matched: &'static str,
}

/// Which literal rank `name` holds for `needle`; the caller has established it contains it.
fn literal_rank(name: &[u8], needle: &[u8]) -> usize {
    if name == needle {
        0
    } else if name.starts_with(needle) {
        1
    } else {
        2
    }
}

/// Body of `code_symbols`.
pub(super) async fn run_search_symbols(
    state: &ServerState,
    params: SearchSymbolsParams,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    state.await_cache_ready().await;
    let format = super::toon::ResponseFormat::parse(params.format.as_deref());
    let kind = params.kind.as_deref().map(parse_kind).transpose()?;
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    let generation = state.shared.cache_generation.load(Ordering::Relaxed);

    let empty = |cursor_invalidated: bool| SearchResponse {
        total: 0,
        truncated: false,
        truncated_reason: None,
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

    // A symbol name never holds the term index's separator, so such a needle matches nothing.
    let needle = params.needle.as_bytes();
    if needle.is_empty() || needle.contains(&0) {
        return super::toon::format_result(&empty(false), format);
    }
    let finder = memchr::memmem::Finder::new(needle);
    let want = skip.saturating_add(limit);
    let cache = state.shared.cache.load_full();

    // The sweep walks every symbol name of the corpus: off the reactor.
    let (page, total, total_is_partial) = super::index_route::blocking(|| {
        let mut literal: [Vec<Candidate<'_>>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        let mut literal_total = 0usize;
        for (path, terms) in cache.terms().iter() {
            for symbol in terms.matching_symbols(&finder, kind) {
                let rank = literal_rank(terms.name(symbol), needle);
                literal_total += 1;
                if literal[rank].len() < want {
                    literal[rank].push(Candidate {
                        path,
                        symbol,
                        matched: LITERAL_RANKS[rank],
                    });
                }
            }
        }

        let mut total_is_partial = false;
        let mut fuzzy: Vec<(u32, Candidate<'_>)> = Vec::new();
        if params.fuzzy {
            let pattern = Pattern::parse(&params.needle, CaseMatching::Ignore, Normalization::Smart);
            let mut matcher = Matcher::new(Config::DEFAULT);
            let mut utf32: Vec<char> = Vec::new();
            'sweep: for (path, terms) in cache.terms().iter() {
                for (symbol, name, symbol_kind) in terms.symbols() {
                    if kind.is_some_and(|k| symbol_kind != k) || finder.find(name).is_some() {
                        continue;
                    }
                    let Ok(name) = std::str::from_utf8(name) else {
                        continue;
                    };
                    let Some(score) = pattern.score(Utf32Str::new(name, &mut utf32), &mut matcher) else {
                        continue;
                    };
                    if fuzzy.len() >= FUZZY_CANDIDATES_MAX {
                        total_is_partial = true;
                        break 'sweep;
                    }
                    fuzzy.push((
                        score,
                        Candidate {
                            path,
                            symbol,
                            matched: FUZZY,
                        },
                    ));
                }
            }
            // Best score first; the sweep order (path, then outline position) breaks ties stably.
            fuzzy.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
            if let Some(&(top, _)) = fuzzy.first() {
                let floor = (u64::from(top) * u64::from(FUZZY_SCORE_FLOOR_PCT) / 100) as u32;
                let keep = fuzzy.partition_point(|(score, _)| *score >= floor);
                fuzzy.truncate(keep);
            }
        }
        let total = literal_total + fuzzy.len();

        let cache_ref = &cache;
        let page: Vec<SearchHitView> = literal
            .into_iter()
            .flatten()
            .chain(fuzzy.into_iter().map(|(_, candidate)| candidate))
            .skip(skip)
            .take(limit)
            .filter_map(|candidate| {
                let l1 = cache_ref.get(candidate.path)?;
                let symbol = l1.symbols.get(candidate.symbol)?;
                Some(SearchHitView {
                    path: candidate.path.clone(),
                    name: symbol.name.clone(),
                    kind: kind_to_str(symbol.kind),
                    line: symbol.start_row + 1,
                    column: symbol.start_col + 1,
                    signature: symbol.signature.clone(),
                    matched: candidate.matched,
                })
            })
            .collect();
        (page, total, total_is_partial)
    });

    let budget = super::budget::apply_budget(page, params.max_tokens);
    let results = budget.items;
    let returned_through = skip + results.len();
    let next_cursor = (total > returned_through && !results.is_empty())
        .then(|| super::cursor::Cursor::encode_in_memory(returned_through as u64, generation));
    super::toon::format_result(
        &SearchResponse {
            total,
            truncated: total_is_partial,
            truncated_reason: total_is_partial.then_some(super::types::SCAN_CAP),
            budgeted: budget.budgeted,
            results,
            next_cursor,
            cursor_invalidated: false,
            notice: state.lifecycle_notice(),
            elapsed_us: elapsed_us(started),
        },
        format,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_rank_orders_exact_before_prefix_before_substring() {
        assert_eq!(literal_rank(b"file_outline", b"file_outline"), 0);
        assert_eq!(literal_rank(b"file_outline_l2", b"file_outline"), 1);
        assert_eq!(literal_rank(b"my_file_outline", b"file_outline"), 2);
    }
}
