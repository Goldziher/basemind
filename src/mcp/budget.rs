//! Token budgeting for high-volume MCP tool responses.
//!
//! An agent can pass `max_tokens` to bound how many tokens a list-returning tool's
//! result list costs. The result list is already best-first (the tools rank as they
//! build), so budgeting is a keep-prefix-that-fits operation: walk the items in order,
//! sum each item's estimated token cost (serialized bytes / 4, the same factor the
//! telemetry estimator in [`super::savings`] uses), and drop the suffix once the running
//! total would exceed the budget.
//!
//! The budget bounds the RESULT LIST, not the whole response envelope — the surrounding
//! struct fields (`total`, `truncated`, cursors) are not counted. Truncation is always
//! explicit: the caller sets a `budgeted: bool` on the response and keeps `next_cursor`
//! working so the agent can page the dropped tail.

use serde::Serialize;

use super::savings::bytes_to_tokens;

/// Outcome of applying a token budget to an already-ranked result list.
pub(super) struct Budgeted<T> {
    /// The kept leading prefix of items whose cumulative token cost fits the budget.
    pub items: Vec<T>,
    /// True when at least one trailing item was dropped to fit the budget. The number
    /// dropped is recoverable as `original_len - items.len()` (asserted in the unit tests);
    /// callers only ever branch on the boolean, so it is not stored separately.
    pub budgeted: bool,
}

/// Keep the longest leading prefix of `items` whose cumulative estimated token cost
/// stays within `max_tokens`, estimating each item's cost as `serialized_bytes / 4`.
///
/// - `max_tokens = None` keeps everything (`budgeted = false`, `dropped = 0`) and never
///   serializes — the zero-overhead path for callers that didn't ask for a budget.
/// - `max_tokens = Some(0)` drops everything except — to guarantee forward progress for
///   pagination — the first item, which is always kept even when it alone exceeds the
///   budget. Without this an agent that set an impossibly small budget would get an empty
///   page with a cursor pointing at the same item forever.
///
/// Each item is serialized exactly once (its bytes feed the running total directly), so
/// there is no per-item re-serialize in the accumulation loop.
pub(super) fn apply_budget<T: Serialize>(items: Vec<T>, max_tokens: Option<u32>) -> Budgeted<T> {
    let Some(budget) = max_tokens.map(u64::from) else {
        return Budgeted { items, budgeted: false };
    };

    let total = items.len();
    let mut kept: Vec<T> = Vec::with_capacity(total);
    let mut used: u64 = 0;
    for item in items {
        let cost = match serde_json::to_vec(&item) {
            Ok(bytes) => bytes_to_tokens(bytes.len() as u64),
            Err(_) => 0,
        };
        if kept.is_empty() || used.saturating_add(cost) <= budget {
            used = used.saturating_add(cost);
            kept.push(item);
        } else {
            break;
        }
    }

    Budgeted {
        budgeted: kept.len() < total,
        items: kept,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    /// A fixed-size item: serializes to a predictable byte count so token math is exact.
    #[derive(Serialize)]
    struct Item {
        /// 10-char value → the serialized object is a stable width.
        v: String,
    }

    fn item(tag: char) -> Item {
        Item {
            v: std::iter::repeat_n(tag, 10).collect(),
        }
    }

    /// Serialized form of one `Item` is `{"v":"xxxxxxxxxx"}` = 18 bytes → 18/4 = 4 tokens.
    const ITEM_TOKENS: u64 = 4;

    #[test]
    fn none_budget_keeps_everything_without_serializing() {
        let items: Vec<Item> = ('a'..='e').map(item).collect();
        let input_len = 5;
        let out = apply_budget(items, None);
        assert_eq!(out.items.len(), 5);
        assert!(!out.budgeted);
        assert_eq!(input_len - out.items.len(), 0, "nothing dropped");
    }

    #[test]
    fn keeps_exact_prefix_that_fits_and_reports_dropped_count() {
        let items: Vec<Item> = ('a'..='e').map(item).collect();
        let input_len = items.len();
        let budget = ITEM_TOKENS * 2 + 1;
        let out = apply_budget(items, Some(budget as u32));
        assert_eq!(out.items.len(), 2, "exactly the 2-item prefix fits");
        assert_eq!(
            out.items.iter().map(|i| i.v.clone()).collect::<Vec<_>>(),
            vec!["aaaaaaaaaa".to_string(), "bbbbbbbbbb".to_string()],
            "kept items must be the leading prefix, in order"
        );
        assert!(out.budgeted);
        assert_eq!(input_len - out.items.len(), 3, "dropped count");
    }

    #[test]
    fn budget_exactly_at_boundary_keeps_all() {
        let items: Vec<Item> = ('a'..='c').map(item).collect();
        let out = apply_budget(items, Some((ITEM_TOKENS * 3) as u32));
        assert_eq!(out.items.len(), 3);
        assert!(!out.budgeted);
    }

    #[test]
    fn zero_budget_still_keeps_first_item_for_forward_progress() {
        let items: Vec<Item> = ('a'..='c').map(item).collect();
        let input_len = items.len();
        let out = apply_budget(items, Some(0));
        assert_eq!(out.items.len(), 1, "first item always admitted");
        assert_eq!(out.items[0].v, "aaaaaaaaaa");
        assert!(out.budgeted);
        assert_eq!(input_len - out.items.len(), 2, "dropped count");
    }

    #[test]
    fn empty_input_is_never_budgeted() {
        let out = apply_budget(Vec::<Item>::new(), Some(0));
        assert!(out.items.is_empty());
        assert!(!out.budgeted);
    }
}
