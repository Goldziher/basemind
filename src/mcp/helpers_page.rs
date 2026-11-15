//! Offset paging for the ranked-search tools (`code_search`, `docs_search`).
//!
//! A ranked retrieval has no natural resume key: page two is "the next best hits". The retrieval
//! bodies return a top-k, so a page is cut out of one top-k here, on the tool's own response,
//! instead of teaching every retrieval pipeline about cursors.
//!
//! That top-k has the same depth for every page of a query. Fusion and rerank depth follow the
//! retrieval size, so a retrieval sized to the page ranked the same hits differently on page two
//! than on page one, and a hit could be repeated or skipped between them.

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
/// How many hits a ranked tool retrieves, whatever page is asked for; paging stops here.
pub(super) const RANKED_DEPTH: u32 = 100;
/// Snapshot id carried by ranked cursors. A ranked page is recomputed from the query, so there is
/// no index generation to pin; the constant only tells these cursors apart from the others.
const RANKED_SNAPSHOT: u32 = 0x7261_6e6b;

/// The rank a ranked-search page starts at.
pub(super) fn ranked_offset(cursor: Option<&Cursor>) -> Result<u32, McpError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let (offset, snapshot) = cursor.decode_in_memory()?;
    if snapshot != RANKED_SNAPSHOT {
        return Err(McpError::invalid_params(
            "this `cursor` came from a different tool; pass the `next_cursor` of this tool's previous page".to_string(),
            None,
        ));
    }
    Ok(u32::try_from(offset).unwrap_or(RANKED_DEPTH))
}

/// Cut the page `[offset, offset + limit)` out of the ranked list at `list_key` (a retrieval of
/// [`RANKED_DEPTH`]), apply the token budget, and attach `next_cursor` when the list goes on.
///
/// A response with no structured content (a TOON-encoded one) is returned untouched: it is page
/// one and carries no cursor.
pub(super) fn page_ranked(
    mut result: CallToolResult,
    list_key: &str,
    offset: u32,
    limit: u32,
    max_tokens: Option<u32>,
) -> Result<CallToolResult, McpError> {
    let Some(Value::Object(mut body)) = result.structured_content.take() else {
        return Ok(result);
    };
    let Some(Value::Array(hits)) = body.remove(list_key) else {
        return Ok(CallToolResult::structured(Value::Object(body)));
    };
    let fetched = hits.len() as u32;
    let page: Vec<Value> = hits.into_iter().skip(offset as usize).take(limit as usize).collect();
    let budgeted = super::budget::apply_budget(page, max_tokens);
    let next = offset + budgeted.items.len() as u32;
    let more = !budgeted.items.is_empty() && next < fetched.min(RANKED_DEPTH);
    body.insert(list_key.to_string(), Value::Array(budgeted.items));
    if budgeted.budgeted {
        body.insert("budgeted".to_string(), Value::Bool(true));
    }
    if more {
        let cursor = Cursor::encode_in_memory(u64::from(next), RANKED_SNAPSHOT);
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

    fn ranked(count: usize) -> CallToolResult {
        let hits: Vec<Value> = (0..count).map(|rank| json!({ "rank": rank })).collect();
        CallToolResult::structured(json!({ "query": "q", "hits": hits }))
    }

    fn body(result: &CallToolResult) -> &Value {
        result.structured_content.as_ref().expect("structured")
    }

    #[test]
    fn a_page_with_ranks_left_in_the_retrieval_offers_a_cursor() {
        let page = page_ranked(ranked(25), "hits", 0, 10, None).expect("page");
        assert_eq!(body(&page)["hits"].as_array().expect("hits").len(), 10);
        assert!(body(&page).get("next_cursor").is_some());
    }

    /// The retrieval has the same depth for every page, so a page that uses it up is the last
    /// one. Sized to the page, a retrieval that came back full only meant "ask again, deeper".
    #[test]
    fn a_page_that_uses_up_the_retrieval_is_the_last_page() {
        let exact = page_ranked(ranked(10), "hits", 0, 10, None).expect("page");
        assert_eq!(body(&exact)["hits"].as_array().expect("hits").len(), 10);
        assert!(body(&exact).get("next_cursor").is_none());

        let short = page_ranked(ranked(4), "hits", 0, 10, None).expect("page");
        assert_eq!(body(&short)["hits"].as_array().expect("hits").len(), 4);
        assert!(body(&short).get("next_cursor").is_none());

        let deepest = page_ranked(ranked(RANKED_DEPTH as usize), "hits", RANKED_DEPTH - 10, 10, None).expect("page");
        assert_eq!(body(&deepest)["hits"].as_array().expect("hits").len(), 10);
        assert!(body(&deepest).get("next_cursor").is_none());
    }

    #[test]
    fn the_second_page_starts_where_the_first_stopped() {
        let first = page_ranked(ranked(13), "hits", 0, 10, None).expect("first");
        let cursor: Cursor = serde_json::from_value(body(&first)["next_cursor"].clone()).expect("cursor");
        let offset = ranked_offset(Some(&cursor)).expect("offset");
        assert_eq!(offset, 10);
        let second = page_ranked(ranked(13), "hits", offset, 10, None).expect("second");
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
        let page = page_ranked(ranked(10), "hits", 0, 10, Some(1)).expect("page");
        assert_eq!(body(&page)["hits"].as_array().expect("hits").len(), 1);
        assert_eq!(body(&page)["budgeted"], json!(true));
        assert!(body(&page).get("next_cursor").is_some());
    }

    #[test]
    fn a_cursor_from_another_tool_is_refused() {
        let foreign = Cursor::encode_in_memory(3, 7);
        assert!(ranked_offset(Some(&foreign)).is_err());
    }
}
