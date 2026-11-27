//! Offset paging for the ranked-search tools (`code_search`, `docs_search`).
//!
//! A ranked retrieval has no natural resume key: page two is "the next best hits". The retrieval
//! bodies return a top-k, so a page is cut out of one top-k here instead of teaching every
//! retrieval pipeline about cursors.
//!
//! That top-k has the same depth for every page of a query: fusion and rerank depth follow the
//! retrieval size, so retrievals of different sizes rank the same hits in different orders, and a
//! page cut from each would repeat or skip a hit.
//!
//! The depth is a hard bound on how far a ranking can be paged. One hit more than the depth is
//! retrieved to learn whether the ranking goes on past it, and a ranking that does is flagged
//! `truncated` once a page reads up to it, so a list of exactly [`RANKED_DEPTH`] hits is told from
//! one cut there. A page that stops short of the depth is paging, not truncation.

#![cfg_attr(
    not(any(feature = "code-search", feature = "documents")),
    allow(
        dead_code,
        reason = "compiled for its tests when neither ranked tool is in the build"
    )
)]

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::EngineError;
use super::cursor::{Cursor, stale_cursor};
use super::ranked_cache::RankedCache;

/// Hits per page when the caller passes no `limit`.
pub(crate) const RANKED_LIMIT_DEFAULT: u32 = 10;
/// How many hits of a ranking can be paged through, whatever page is asked for.
pub(crate) const RANKED_DEPTH: u32 = 100;
/// What a ranked tool asks its retrieval for: the depth, and one hit that only says whether the
/// ranking continues past it.
#[cfg_attr(
    not(feature = "code-search"),
    allow(dead_code, reason = "only `code_search` pages this way")
)]
pub(crate) const RANKED_FETCH: u32 = RANKED_DEPTH + 1;
/// `truncated_reason` of a ranking that goes on past [`RANKED_DEPTH`].
#[cfg_attr(
    not(feature = "code-search"),
    allow(dead_code, reason = "only `code_search` pages this way")
)]
pub(crate) const REASON_DEPTH: &str = "depth";

/// The ranking of `tool` for `arguments` (every argument that shapes it): the one kept for it at
/// `generation`, or what `retrieve` returns, kept for the pages and repeats that follow when
/// `cacheable` says so. A degraded or empty ranking should not be: it is the one most likely to be
/// different a moment later.
pub(crate) fn ranked_retrieval<R: Serialize + DeserializeOwned>(
    cache: &RankedCache,
    tool: &str,
    arguments: &serde_json::Value,
    generation: u32,
    retrieve: impl FnOnce() -> Result<R, EngineError>,
    cacheable: impl FnOnce(&R) -> bool,
) -> Result<R, EngineError> {
    let key = RankedCache::key(tool, arguments);
    if let Some(ranking) = cache
        .get(&key, generation)
        .and_then(|body| serde_json::from_value::<R>(body).ok())
    {
        return Ok(ranking);
    }
    let ranking = retrieve()?;
    if cacheable(&ranking)
        && let Ok(body) = serde_json::to_value(&ranking)
    {
        cache.put(key, generation, &body);
    }
    Ok(ranking)
}

/// The rank a ranked-search page starts at. `generation` is the index generation the page is
/// cut from: a cursor issued before a rescan points into a ranking that no longer exists.
pub(crate) fn ranked_offset(tool: &str, cursor: Option<&Cursor>, generation: u32) -> Result<u32, EngineError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let (offset, snapshot) = cursor.decode_in_memory()?;
    if snapshot != generation {
        return Err(stale_cursor(tool));
    }
    Ok(u32::try_from(offset).unwrap_or(RANKED_DEPTH))
}

/// One page cut out of a ranking.
#[cfg_attr(
    not(feature = "code-search"),
    allow(dead_code, reason = "only `code_search` pages this way")
)]
#[derive(Debug)]
pub struct RankedPage<T> {
    pub hits: Vec<T>,
    /// The token budget dropped trailing hits of the page.
    pub budgeted: bool,
    /// The page ends at [`RANKED_DEPTH`] and the ranking goes on past it.
    pub depth_cut: bool,
    pub next_cursor: Option<Cursor>,
}

/// Cut the page `[offset, offset + limit)` out of `hits` (a retrieval of [`RANKED_FETCH`]), apply
/// the token budget, and offer `next_cursor` when the list goes on. A retrieval that came back with
/// more than [`RANKED_DEPTH`] hits is cut there, on every page. `past_depth` says the retrieval had
/// more than that before `hits` was merged down, so a ranking that goes on is still flagged.
#[cfg_attr(
    not(feature = "code-search"),
    allow(dead_code, reason = "only `code_search` pages this way")
)]
pub(crate) fn page_ranked<T: Serialize>(
    mut hits: Vec<T>,
    offset: u32,
    limit: u32,
    max_tokens: Option<u32>,
    generation: u32,
    past_depth: bool,
) -> RankedPage<T> {
    let goes_on = past_depth || hits.len() > RANKED_DEPTH as usize;
    hits.truncate(RANKED_DEPTH as usize);
    let fetched = hits.len() as u32;
    let page: Vec<T> = hits.into_iter().skip(offset as usize).take(limit as usize).collect();
    let budgeted = super::budget::apply_budget(page, max_tokens);
    let next = offset + budgeted.items.len() as u32;
    let more = !budgeted.items.is_empty() && next < fetched;
    RankedPage {
        hits: budgeted.items,
        budgeted: budgeted.budgeted,
        // A page that ends short of the depth is paging: only one that reaches it is cut there.
        depth_cut: goes_on && !more,
        next_cursor: more.then(|| Cursor::encode_in_memory(u64::from(next), generation)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GENERATION: u32 = 7;

    fn ranked(count: u32) -> Vec<u32> {
        (0..count).collect()
    }

    fn page(hits: Vec<u32>, offset: u32, limit: u32, max_tokens: Option<u32>) -> RankedPage<u32> {
        page_ranked(hits, offset, limit, max_tokens, GENERATION, false)
    }

    #[test]
    fn a_retrieval_merged_down_to_the_depth_still_says_it_goes_on() {
        let merged = page_ranked(ranked(RANKED_DEPTH), RANKED_DEPTH - 10, 10, None, GENERATION, true);
        assert_eq!(merged.hits.len(), 10);
        assert!(merged.depth_cut, "the retrieval had {RANKED_FETCH} hits before merging");
        assert!(merged.next_cursor.is_none());
    }

    #[test]
    fn a_page_with_ranks_left_in_the_retrieval_offers_a_cursor() {
        let first = page(ranked(25), 0, 10, None);
        assert_eq!(first.hits.len(), 10);
        assert!(first.next_cursor.is_some());
        assert!(!first.depth_cut);
    }

    #[test]
    fn a_page_that_uses_up_the_retrieval_is_the_last_page() {
        let exact = page(ranked(10), 0, 10, None);
        assert_eq!(exact.hits.len(), 10);
        assert!(exact.next_cursor.is_none());

        let short = page(ranked(4), 0, 10, None);
        assert_eq!(short.hits.len(), 4);
        assert!(short.next_cursor.is_none());
    }

    #[test]
    fn a_ranking_of_exactly_the_depth_is_whole_and_one_that_goes_on_says_so() {
        let exact = page(ranked(RANKED_DEPTH), RANKED_DEPTH - 10, 10, None);
        assert_eq!(exact.hits.len(), 10);
        assert!(exact.next_cursor.is_none());
        assert!(!exact.depth_cut, "exactly {RANKED_DEPTH} hits exist");

        let first = page(ranked(RANKED_DEPTH + 1), 0, 10, None);
        assert!(!first.depth_cut, "a limit cut is paging");
        assert!(first.next_cursor.is_some());

        let last = page(ranked(RANKED_DEPTH + 1), RANKED_DEPTH - 10, 10, None);
        assert_eq!(
            last.hits.last(),
            Some(&(RANKED_DEPTH - 1)),
            "the probe hit is never returned"
        );
        assert!(last.next_cursor.is_none(), "the depth is where paging stops");
        assert!(last.depth_cut);
    }

    #[test]
    fn a_later_page_and_a_repeat_do_not_run_the_retrieval_again() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cache = RankedCache::default();
        let runs = AtomicUsize::new(0);
        let retrieve = |count: u32| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(ranked(count))
        };
        let kept = |hits: &Vec<u32>| !hits.is_empty();
        let query = serde_json::json!({ "query": "retry", "lane": null });
        let other = serde_json::json!({ "query": "upload", "lane": null });
        let search = |arguments: &serde_json::Value, generation: u32, count: u32| {
            ranked_retrieval(&cache, "code_search", arguments, generation, || retrieve(count), kept).expect("ranking")
        };

        let first = search(&query, GENERATION, 25);
        let again = search(&query, GENERATION, 25);
        assert_eq!(
            runs.load(Ordering::Relaxed),
            1,
            "the second call is cut from the first's list"
        );
        assert_eq!(first, again);
        assert_eq!(page(again, 10, 10, None).hits[0], 10);

        search(&other, GENERATION, 3);
        assert_eq!(runs.load(Ordering::Relaxed), 2, "another query is another retrieval");

        search(&query, GENERATION + 1, 25);
        assert_eq!(runs.load(Ordering::Relaxed), 3, "a rescan invalidates the list");

        let empty = serde_json::json!({ "query": "nothing", "lane": null });
        for _ in 0..2 {
            search(&empty, GENERATION + 1, 0);
        }
        assert_eq!(runs.load(Ordering::Relaxed), 5, "an empty answer is asked for again");
    }

    #[test]
    fn the_second_page_starts_where_the_first_stopped() {
        let first = page(ranked(13), 0, 10, None);
        let offset = ranked_offset("code_search", first.next_cursor.as_ref(), GENERATION).expect("offset");
        assert_eq!(offset, 10);
        let second = page(ranked(13), offset, 10, None);
        assert_eq!(second.hits, [10, 11, 12]);
        assert!(second.next_cursor.is_none());
    }

    #[test]
    fn a_budget_cut_is_flagged_and_resumable() {
        let hits: Vec<String> = (0..10).map(|rank| format!("hit {rank} {}", "x".repeat(64))).collect();
        let cut = page_ranked(hits, 0, 10, Some(1), GENERATION, false);
        assert_eq!(cut.hits.len(), 1);
        assert!(cut.budgeted);
        assert!(cut.next_cursor.is_some());
    }

    #[test]
    fn a_cursor_cut_from_another_index_generation_is_refused() {
        let first = page(ranked(13), 0, 10, None);
        let cursor = first.next_cursor.expect("cursor");
        assert!(ranked_offset("docs_search", Some(&cursor), GENERATION).is_ok());
        let refused = ranked_offset("docs_search", Some(&cursor), GENERATION + 1).expect_err("rescanned");
        assert!(refused.to_string().contains("Call again without `cursor`"), "{refused}");
    }
}
