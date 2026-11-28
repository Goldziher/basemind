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

use super::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX};
use crate::engine::budget::{apply_budget, budget};
use crate::engine::changed::admits;
use crate::engine::cursor::Cursor;
use crate::engine::error::EngineError;
use crate::engine::filters::{Filters, RoleScope, grep_selects_no_file, parse_path};
use crate::engine::scope::ChangeScope;
use crate::engine::types::SCAN_CAP;
use crate::engine::types::code::{
    CodeSymbolsParams, SearchHitView, SearchResponse, SearchSymbolsParams, kind_to_str, parse_kind,
};
use crate::engine::workspace::Workspace;
use crate::path::RelPath;

/// Fuzzy candidates held while ranking. A needle of two letters is a subsequence of most names in
/// a large corpus; past this many the total is reported as partial instead of holding them all.
const FUZZY_CANDIDATES_MAX: usize = 50_000;

/// A fuzzy match scoring below this share of the best one is noise, not a near-miss.
const FUZZY_SCORE_FLOOR_PCT: u32 = 85;

/// One symbol that matched, before its outline is decoded.
struct Candidate<'a> {
    path: &'a RelPath,
    symbol: usize,
}

/// Where the next page starts in the ranking: at the first row the token budget dropped, else after
/// the whole window this page took, rows that could not be shown included. `offsets` holds each
/// shown row's place in the ranking; `kept` rows survived the budget.
fn resume_at(window_end: usize, offsets: &[usize], kept: usize) -> usize {
    offsets.get(kept).copied().unwrap_or(window_end)
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

impl Workspace {
    /// `code_symbols`: find definitions by name, best match first.
    pub(crate) fn code_symbols(&self, p: CodeSymbolsParams) -> Result<SearchResponse, EngineError> {
        if p.name.is_empty() {
            return Err(EngineError::invalid_argument(
                "`code_symbols`: `name` is empty; pass the name of the definition to find.",
            ));
        }
        let path = parse_path("code_symbols", p.path.as_deref())?;
        let scope = crate::engine::changed::resolve(self, "code_symbols", p.changed.as_deref())?;
        let mut response = search_symbols(
            self,
            SearchSymbolsParams {
                needle: p.name,
                kind: p.kind,
                path,
                language: p.language,
                roles: p.roles,
                fuzzy: p.fuzzy.unwrap_or(false),
                limit: p.limit,
                max_tokens: budget(p.max_tokens),
                cursor: p.cursor,
            },
            scope.as_ref(),
        )?;
        response.scope_files = scope.map(|scope| scope.len());
        Ok(response)
    }
}

/// Body of `code_symbols`.
fn search_symbols(
    ws: &Workspace,
    params: SearchSymbolsParams,
    scope: Option<&ChangeScope>,
) -> Result<SearchResponse, EngineError> {
    ws.wait_map_ready();
    let kind = params.kind.as_deref().map(parse_kind).transpose()?;
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    let generation = ws.cache_generation.load(Ordering::Relaxed);

    let empty = |cursor_invalidated: bool| SearchResponse {
        total: None,
        truncated: false,
        truncated_reason: None,
        budgeted: false,
        results: Vec::new(),
        stale: Vec::new(),
        next_cursor: None,
        cursor_invalidated,
        scope_files: None,
        notice: ws.lifecycle_notice(),
    };

    let skip = match params.cursor.as_ref() {
        Some(c) => {
            let (offset, snapshot_id) = c.decode_in_memory()?;
            if snapshot_id != generation {
                return Ok(empty(true));
            }
            offset as usize
        }
        None => 0,
    };

    // A symbol name never holds the term index's separator, so such a needle matches nothing.
    let needle = params.needle.as_bytes();
    if needle.is_empty() || needle.contains(&0) {
        return Ok(empty(false));
    }
    let finder = memchr::memmem::Finder::new(needle);
    let want = skip.saturating_add(limit);
    let cache = ws.cache.load_full();
    let roles = RoleScope::resolve(ws, "code_symbols", params.roles.as_deref())?;
    let filters = Filters {
        tool: "code_symbols",
        path: params.path.as_ref(),
        language: params.language.as_deref(),
        roles: roles.as_ref(),
    };
    let selected = |path: &RelPath| {
        admits(scope, path)
            && (filters.is_empty() || filters.passes(path.as_bytes(), cache.language_of(path).unwrap_or_default()))
    };

    // The sweep walks every symbol name of the corpus: off the reactor.
    let (page, offsets, window_end, total, total_is_partial) = {
        let mut literal: [Vec<Candidate<'_>>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        let mut literal_total = 0usize;
        for (path, terms) in cache.terms().iter().filter(|&(path, _)| selected(path)) {
            for symbol in terms.matching_symbols(&finder, kind) {
                let rank = literal_rank(terms.name(symbol), needle);
                literal_total += 1;
                if literal[rank].len() < want {
                    literal[rank].push(Candidate { path, symbol });
                }
            }
        }

        let mut total_is_partial = false;
        let mut fuzzy: Vec<(u32, Candidate<'_>)> = Vec::new();
        if params.fuzzy {
            let pattern = Pattern::parse(&params.needle, CaseMatching::Ignore, Normalization::Smart);
            let mut matcher = Matcher::new(Config::DEFAULT);
            let mut utf32: Vec<char> = Vec::new();
            'sweep: for (path, terms) in cache.terms().iter().filter(|&(path, _)| selected(path)) {
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
                    fuzzy.push((score, Candidate { path, symbol }));
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
        let window: Vec<Candidate<'_>> = literal
            .into_iter()
            .flatten()
            .chain(fuzzy.into_iter().map(|(_, candidate)| candidate))
            .skip(skip)
            .take(limit)
            .collect();
        let window_end = skip + window.len();
        // A candidate whose outline no longer loads is left out; each row keeps its place in the
        // ranking so the next page starts after the rows this one consumed, not after as many rows
        // as it shows.
        let (offsets, page): (Vec<usize>, Vec<SearchHitView>) = window
            .into_iter()
            .enumerate()
            .filter_map(|(i, candidate)| {
                let l1 = cache_ref.get(candidate.path)?;
                let symbol = l1.symbols.get(candidate.symbol)?;
                Some((
                    skip + i,
                    SearchHitView {
                        path: candidate.path.clone(),
                        line: symbol.start_row + 1,
                        kind: kind_to_str(symbol.kind),
                        name: symbol.name.clone(),
                        signature: symbol.signature.clone(),
                        stale: false,
                    },
                ))
            })
            .unzip();
        (page, offsets, window_end, total, total_is_partial)
    };

    if total == 0
        && let Some(refusal) = grep_selects_no_file(ws, &cache, &filters)
    {
        return Err(refusal);
    }
    let mut page = page;
    crate::engine::stale::follow_symbols(ws, &cache, &mut page);
    let budget = apply_budget(page, params.max_tokens);
    let results = budget.items;
    let returned_through = resume_at(window_end, &offsets, results.len());
    let next_cursor = (total > returned_through && returned_through > skip)
        .then(|| Cursor::encode_in_memory(returned_through as u64, generation));
    let mut stale: Vec<RelPath> = Vec::new();
    for hit in results.iter().filter(|hit| hit.stale) {
        if !stale.contains(&hit.path) {
            stale.push(hit.path.clone());
        }
    }
    Ok(SearchResponse {
        total: next_cursor.is_some().then_some(total),
        truncated: total_is_partial,
        truncated_reason: total_is_partial.then_some(SCAN_CAP),
        budgeted: budget.budgeted,
        results,
        stale,
        next_cursor,
        cursor_invalidated: false,
        scope_files: None,
        notice: ws.lifecycle_notice(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row that cannot be shown (its outline did not load) still took its place in the ranking:
    /// the next page starts after it, so no row is shown twice and none is skipped.
    #[test]
    fn the_next_page_starts_after_the_rows_this_one_consumed() {
        // Ranking rows 10..15 taken; row 12 could not be shown.
        let offsets = [10, 11, 13, 14];
        assert_eq!(resume_at(15, &offsets, 4), 15, "every shown row kept: after the window");
        assert_eq!(
            resume_at(15, &offsets, 2),
            13,
            "the budget dropped rows 13 and 14: resume at 13"
        );
        assert_eq!(
            resume_at(15, &[], 0),
            15,
            "no row could be shown: still past the window"
        );
    }

    #[test]
    fn literal_rank_orders_exact_before_prefix_before_substring() {
        assert_eq!(literal_rank(b"file_outline", b"file_outline"), 0);
        assert_eq!(literal_rank(b"file_outline_l2", b"file_outline"), 1);
        assert_eq!(literal_rank(b"my_file_outline", b"file_outline"), 2);
    }
}
