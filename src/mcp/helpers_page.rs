//! Offset paging for the ranked-search tools (`code_search`, `docs_search`).
//!
//! A ranked retrieval has no natural resume key: page two is "the next best hits". The retrieval
//! bodies return a top-k, so a page is cut out of one top-k here, on the tool's own response,
//! instead of teaching every retrieval pipeline about cursors.
//!
//! That top-k has the same depth for every page of a query: fusion and rerank depth follow the
//! retrieval size, so retrievals of different sizes rank the same hits in different orders, and a
//! page cut from each would repeat or skip a hit.
//!
//! The depth is a hard bound on how far a ranking can be paged. One hit more than the depth is
//! retrieved to learn whether the ranking goes on past it, and a ranking that does is flagged
//! `truncated`, so a list of exactly [`RANKED_DEPTH`] hits is told from one cut there.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::Value;

use super::cursor::Cursor;

/// Hits per page when the caller passes no `limit`.
#[cfg_attr(
    not(any(feature = "code-search", feature = "documents")),
    allow(
        dead_code,
        reason = "compiled for its tests when neither ranked tool is in the build"
    )
)]
pub(super) const RANKED_LIMIT_DEFAULT: u32 = 10;
/// How many hits of a ranking can be paged through, whatever page is asked for.
pub(super) const RANKED_DEPTH: u32 = 100;
/// What a ranked tool asks its retrieval for: the depth, and one hit that only says whether the
/// ranking continues past it.
#[cfg_attr(
    not(any(feature = "code-search", feature = "documents")),
    allow(
        dead_code,
        reason = "compiled for its tests when neither ranked tool is in the build"
    )
)]
pub(super) const RANKED_FETCH: u32 = RANKED_DEPTH + 1;
/// `truncated_reason` of a ranking that goes on past [`RANKED_DEPTH`].
const REASON_DEPTH: &str = "depth";

/// Chunks a `docs_search` with a `keywords` / `entity` filter lets the filter see.
#[cfg_attr(
    not(feature = "documents"),
    allow(dead_code, reason = "compiled for its tests when `docs_search` is not in the build")
)]
pub(super) const FILTER_DEPTH: usize = 400;

/// What a filtered retrieval asks for: the page or [`FILTER_DEPTH`], whichever is deeper, and one
/// chunk that only says whether the ranking continues past it.
#[cfg_attr(
    not(feature = "documents"),
    allow(dead_code, reason = "compiled for its tests when `docs_search` is not in the build")
)]
pub(super) fn filtered_fetch(limit: usize) -> usize {
    limit.max(FILTER_DEPTH) + 1
}

/// Drop the probe chunk of a retrieval of `fetch` (see [`filtered_fetch`]) and say whether it was
/// there: only then did the filter not see the whole ranking. A vector search returns as many
/// chunks as it is asked for whenever the corpus has them, so "it returned the depth" alone would
/// flag every filtered search over a corpus that large.
#[cfg_attr(
    not(feature = "documents"),
    allow(dead_code, reason = "compiled for its tests when `docs_search` is not in the build")
)]
pub(super) fn cut_at_filter_depth<T>(retrieved: &mut Vec<T>, fetch: usize) -> bool {
    let goes_on = retrieved.len() >= fetch;
    retrieved.truncate(fetch.saturating_sub(1));
    goes_on
}

/// The cut a retrieval names on its own response, before [`page_ranked`] adds `depth` to one that
/// names none. One reason is reported, the one that hides the most: `filter_depth` (documents the
/// filter never saw, which no cursor reaches), then `rerank_top_k` (candidates the reranker
/// dropped), then `depth` (the ranking goes on past what can be paged).
#[cfg_attr(
    not(any(feature = "code-search", feature = "documents")),
    allow(
        dead_code,
        reason = "compiled for its tests when neither ranked tool is in the build"
    )
)]
pub(super) fn retrieval_cut(past_filter_depth: bool, reranker_dropped: bool) -> Option<&'static str> {
    if past_filter_depth {
        Some("filter_depth")
    } else if reranker_dropped {
        Some("rerank_top_k")
    } else {
        None
    }
}

/// The ranked retrieval of `tool` for `arguments`: the list kept for it at `generation`, or what
/// `retrieve` returns, kept for the pages and repeats that follow. A degraded or empty answer is
/// not kept: it is the one most likely to be different a moment later.
#[cfg_attr(
    not(any(feature = "code-search", feature = "documents")),
    allow(
        dead_code,
        reason = "compiled for its tests when neither ranked tool is in the build"
    )
)]
pub(super) async fn ranked_retrieval<F>(
    cache: &super::ranked_cache::RankedCache,
    tool: &str,
    arguments: &Value,
    list_key: &str,
    generation: u32,
    retrieve: impl FnOnce() -> F,
) -> Result<CallToolResult, McpError>
where
    F: std::future::Future<Output = Result<CallToolResult, McpError>>,
{
    let started = std::time::Instant::now();
    let key = super::ranked_cache::RankedCache::key(tool, arguments);
    if let Some(mut body) = cache.get(&key, generation) {
        if let Some(elapsed) = body.get_mut("elapsed_us") {
            *elapsed = Value::from(super::helpers::elapsed_us(started));
        }
        return Ok(CallToolResult::structured(body));
    }
    let result = retrieve().await?;
    if let Some(body) = result.structured_content.as_ref() {
        let ranked = body[list_key].as_array().is_some_and(|hits| !hits.is_empty());
        let degraded = body["lanes_unavailable"]
            .as_array()
            .is_some_and(|lanes| !lanes.is_empty());
        if ranked && !degraded {
            cache.put(key, generation, body);
        }
    }
    Ok(result)
}

/// The rank a ranked-search page starts at. `generation` is the index generation the page is
/// cut from: a cursor issued before a rescan points into a ranking that no longer exists.
pub(super) fn ranked_offset(tool: &str, cursor: Option<&Cursor>, generation: u32) -> Result<u32, McpError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let (offset, snapshot) = cursor.decode_in_memory()?;
    if snapshot != generation {
        return Err(super::cursor_bind::stale_cursor(tool));
    }
    Ok(u32::try_from(offset).unwrap_or(RANKED_DEPTH))
}

/// Cut the page `[offset, offset + limit)` out of the ranked list at `list_key` (a retrieval of
/// [`RANKED_FETCH`]), apply the token budget, and attach `next_cursor` when the list goes on.
///
/// A retrieval that came back with more than [`RANKED_DEPTH`] hits is flagged `truncated` with
/// reason `depth` on every page, unless the body already names a tighter cut of its own.
///
/// A response with no structured content (a TOON-encoded one) is returned untouched: it is page
/// one and carries no cursor.
pub(super) fn page_ranked(
    mut result: CallToolResult,
    list_key: &str,
    offset: u32,
    limit: u32,
    max_tokens: Option<u32>,
    generation: u32,
) -> Result<CallToolResult, McpError> {
    let Some(Value::Object(mut body)) = result.structured_content.take() else {
        return Ok(result);
    };
    let Some(Value::Array(mut hits)) = body.remove(list_key) else {
        return Ok(CallToolResult::structured(Value::Object(body)));
    };
    let goes_on = hits.len() > RANKED_DEPTH as usize;
    hits.truncate(RANKED_DEPTH as usize);
    let fetched = hits.len() as u32;
    let page: Vec<Value> = hits.into_iter().skip(offset as usize).take(limit as usize).collect();
    let budgeted = super::budget::apply_budget(page, max_tokens);
    let next = offset + budgeted.items.len() as u32;
    let more = !budgeted.items.is_empty() && next < fetched;
    body.insert(list_key.to_string(), Value::Array(budgeted.items));
    if budgeted.budgeted {
        body.insert("budgeted".to_string(), Value::Bool(true));
    }
    if goes_on && body.get("truncated") != Some(&Value::Bool(true)) {
        body.insert("truncated".to_string(), Value::Bool(true));
        body.insert("truncated_reason".to_string(), Value::String(REASON_DEPTH.to_string()));
    }
    if more {
        let cursor = Cursor::encode_in_memory(u64::from(next), generation);
        body.insert(
            "next_cursor".to_string(),
            serde_json::to_value(cursor).unwrap_or(Value::Null),
        );
    }
    Ok(CallToolResult::structured(Value::Object(body)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const GENERATION: u32 = 7;

    fn ranked(count: usize) -> CallToolResult {
        let hits: Vec<Value> = (0..count).map(|rank| json!({ "rank": rank })).collect();
        CallToolResult::structured(json!({ "query": "q", "hits": hits }))
    }

    fn body(result: &CallToolResult) -> &Value {
        result.structured_content.as_ref().expect("structured")
    }

    fn page(result: CallToolResult, offset: u32, limit: u32, max_tokens: Option<u32>) -> CallToolResult {
        page_ranked(result, "hits", offset, limit, max_tokens, GENERATION).expect("page")
    }

    #[test]
    fn a_page_with_ranks_left_in_the_retrieval_offers_a_cursor() {
        let first = page(ranked(25), 0, 10, None);
        assert_eq!(body(&first)["hits"].as_array().expect("hits").len(), 10);
        assert!(body(&first).get("next_cursor").is_some());
        assert!(body(&first).get("truncated").is_none());
    }

    #[test]
    fn a_page_that_uses_up_the_retrieval_is_the_last_page() {
        let exact = page(ranked(10), 0, 10, None);
        assert_eq!(body(&exact)["hits"].as_array().expect("hits").len(), 10);
        assert!(body(&exact).get("next_cursor").is_none());

        let short = page(ranked(4), 0, 10, None);
        assert_eq!(body(&short)["hits"].as_array().expect("hits").len(), 4);
        assert!(body(&short).get("next_cursor").is_none());
    }

    #[test]
    fn a_ranking_of_exactly_the_depth_is_whole_and_one_that_goes_on_says_so() {
        let depth = RANKED_DEPTH as usize;
        let exact = page(ranked(depth), RANKED_DEPTH - 10, 10, None);
        assert_eq!(body(&exact)["hits"].as_array().expect("hits").len(), 10);
        assert!(body(&exact).get("next_cursor").is_none());
        assert!(body(&exact).get("truncated").is_none(), "exactly {depth} hits exist");

        let first = page(ranked(depth + 1), 0, 10, None);
        assert_eq!(body(&first)["truncated"], json!(true), "flagged from the first page");
        assert_eq!(body(&first)["truncated_reason"], json!("depth"));
        assert!(body(&first).get("next_cursor").is_some());

        let last = page(ranked(depth + 1), RANKED_DEPTH - 10, 10, None);
        let ranks: Vec<u64> = body(&last)["hits"]
            .as_array()
            .expect("hits")
            .iter()
            .map(|hit| hit["rank"].as_u64().expect("rank"))
            .collect();
        assert_eq!(
            ranks.last(),
            Some(&(u64::from(RANKED_DEPTH) - 1)),
            "the probe hit is never returned"
        );
        assert!(
            body(&last).get("next_cursor").is_none(),
            "the depth is where paging stops"
        );
        assert_eq!(body(&last)["truncated"], json!(true));
        assert_eq!(body(&last)["truncated_reason"], json!("depth"));
    }

    #[test]
    fn a_filter_is_flagged_only_when_the_ranking_goes_on_past_its_depth() {
        assert_eq!(filtered_fetch(10), FILTER_DEPTH + 1);
        assert_eq!(filtered_fetch(FILTER_DEPTH + 50), FILTER_DEPTH + 51);
        let fetch = filtered_fetch(10);

        let mut exactly_the_depth: Vec<usize> = (0..FILTER_DEPTH).collect();
        assert!(
            !cut_at_filter_depth(&mut exactly_the_depth, fetch),
            "a corpus of exactly the depth was seen whole"
        );
        assert_eq!(exactly_the_depth.len(), FILTER_DEPTH);

        let mut deeper: Vec<usize> = (0..fetch).collect();
        assert!(cut_at_filter_depth(&mut deeper, fetch));
        assert_eq!(
            deeper.len(),
            FILTER_DEPTH,
            "the probe chunk is never filtered or returned"
        );

        let mut small: Vec<usize> = (0..3).collect();
        assert!(!cut_at_filter_depth(&mut small, fetch));
        assert_eq!(small.len(), 3);
    }

    /// Which of `filter_depth`, `rerank_top_k` and `depth` a response names.
    #[test]
    fn one_cut_is_named_and_it_is_the_one_that_hides_the_most() {
        let named = |past_filter_depth, reranker_dropped, hits: usize| {
            let mut response = json!({ "hits": (0..hits).map(|rank| json!({ "rank": rank })).collect::<Vec<_>>() });
            if let Some(reason) = retrieval_cut(past_filter_depth, reranker_dropped) {
                response["truncated"] = json!(true);
                response["truncated_reason"] = json!(reason);
            }
            let paged = page(CallToolResult::structured(response), 0, 10, None);
            body(&paged).get("truncated_reason").cloned()
        };
        let past_depth = RANKED_FETCH as usize;
        assert_eq!(named(false, false, 5), None);
        assert_eq!(named(false, false, past_depth), Some(json!("depth")));
        assert_eq!(named(false, true, 5), Some(json!("rerank_top_k")));
        assert_eq!(named(false, true, past_depth), Some(json!("rerank_top_k")));
        assert_eq!(named(true, false, 5), Some(json!("filter_depth")));
        assert_eq!(named(true, true, past_depth), Some(json!("filter_depth")));
    }

    #[tokio::test]
    async fn a_later_page_and_a_repeat_do_not_run_the_retrieval_again() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cache = crate::mcp::ranked_cache::RankedCache::default();
        let runs = AtomicUsize::new(0);
        let retrieve = |count: usize| {
            runs.fetch_add(1, Ordering::Relaxed);
            async move { Ok(ranked(count)) }
        };
        let query = json!({ "query": "retry", "lane": null });
        let other = json!({ "query": "upload", "lane": null });

        let first = ranked_retrieval(&cache, "code_search", &query, "hits", GENERATION, || retrieve(25))
            .await
            .expect("retrieval");
        let again = ranked_retrieval(&cache, "code_search", &query, "hits", GENERATION, || retrieve(25))
            .await
            .expect("cached");
        assert_eq!(
            runs.load(Ordering::Relaxed),
            1,
            "the second call is cut from the first's list"
        );
        assert_eq!(body(&first)["hits"], body(&again)["hits"]);
        let second_page = page(again, 10, 10, None);
        assert_eq!(body(&second_page)["hits"][0]["rank"], json!(10));

        ranked_retrieval(&cache, "code_search", &other, "hits", GENERATION, || retrieve(3))
            .await
            .expect("retrieval");
        assert_eq!(runs.load(Ordering::Relaxed), 2, "another query is another retrieval");

        ranked_retrieval(&cache, "code_search", &query, "hits", GENERATION + 1, || retrieve(25))
            .await
            .expect("retrieval");
        assert_eq!(runs.load(Ordering::Relaxed), 3, "a rescan invalidates the list");

        let empty = json!({ "query": "nothing", "lane": null });
        for _ in 0..2 {
            ranked_retrieval(&cache, "code_search", &empty, "hits", GENERATION + 1, || retrieve(0))
                .await
                .expect("retrieval");
        }
        assert_eq!(runs.load(Ordering::Relaxed), 5, "an empty answer is asked for again");
    }

    #[test]
    fn a_cut_the_retrieval_already_named_is_kept() {
        let hits: Vec<Value> = (0..=RANKED_DEPTH).map(|rank| json!({ "rank": rank })).collect();
        let result =
            CallToolResult::structured(json!({ "hits": hits, "truncated": true, "truncated_reason": "filter_depth" }));
        let paged = page(result, 0, 10, None);
        assert_eq!(body(&paged)["truncated_reason"], json!("filter_depth"));
    }

    #[test]
    fn the_second_page_starts_where_the_first_stopped() {
        let first = page(ranked(13), 0, 10, None);
        let cursor: Cursor = serde_json::from_value(body(&first)["next_cursor"].clone()).expect("cursor");
        let offset = ranked_offset("code_search", Some(&cursor), GENERATION).expect("offset");
        assert_eq!(offset, 10);
        let second = page(ranked(13), offset, 10, None);
        let ranks: Vec<u64> = body(&second)["hits"]
            .as_array()
            .expect("hits")
            .iter()
            .map(|hit| hit["rank"].as_u64().expect("rank"))
            .collect();
        assert_eq!(ranks, [10, 11, 12]);
        assert!(body(&second).get("next_cursor").is_none());
    }

    #[test]
    fn a_budget_cut_is_flagged_and_resumable() {
        let cut = page(ranked(10), 0, 10, Some(1));
        assert_eq!(body(&cut)["hits"].as_array().expect("hits").len(), 1);
        assert_eq!(body(&cut)["budgeted"], json!(true));
        assert!(body(&cut).get("next_cursor").is_some());
    }

    #[test]
    fn a_cursor_cut_from_another_index_generation_is_refused() {
        let first = page(ranked(13), 0, 10, None);
        let cursor: Cursor = serde_json::from_value(body(&first)["next_cursor"].clone()).expect("cursor");
        assert!(ranked_offset("docs_search", Some(&cursor), GENERATION).is_ok());
        let refused = ranked_offset("docs_search", Some(&cursor), GENERATION + 1).expect_err("rescanned");
        assert!(
            refused.message.contains("Call again without `cursor`"),
            "{}",
            refused.message
        );
    }
}
