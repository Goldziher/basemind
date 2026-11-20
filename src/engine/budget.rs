//! Token budgeting for high-volume MCP tool responses.
//!
//! An agent can pass `max_tokens` to bound how many tokens a list-returning tool's
//! result list costs. The result list is already best-first (the tools rank as they
//! build), so budgeting is a keep-prefix-that-fits operation: walk the items in order,
//! sum each item's estimated token cost (serialized bytes / 4, the same factor the
//! telemetry estimator in `mcp::savings` uses), and drop the suffix once the running
//! total would exceed the budget.
//!
//! The budget bounds the RESULT LIST, not the whole response envelope — the surrounding
//! struct fields (`total`, `truncated`, cursors) are not counted. Truncation is always
//! explicit: the caller sets a `budgeted: bool` on the response and keeps `next_cursor`
//! working so the agent can page the dropped tail.

use serde::Serialize;

use crate::engine::tokens::bytes_to_tokens;

/// Token budget a list response is trimmed to when the caller passes no `max_tokens`. One unbudgeted
/// outline once returned 832 KB; a default bounds every list without the caller having to ask.
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 6_000;

/// The budget to apply: the caller's, or [`DEFAULT_MAX_TOKENS`].
pub(crate) fn budget(max_tokens: Option<u32>) -> Option<u32> {
    Some(max_tokens.unwrap_or(DEFAULT_MAX_TOKENS))
}

/// Outcome of applying a token budget to an already-ranked result list.
pub(crate) struct Budgeted<T> {
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
pub(crate) fn apply_budget<T: Serialize>(items: Vec<T>, max_tokens: Option<u32>) -> Budgeted<T> {
    keep_prefix(items, max_tokens, |item| {
        serde_json::to_vec(item).map_or(0, |bytes| bytes_to_tokens(bytes.len() as u64))
    })
}

/// [`apply_budget`] for a list an answer writes as a TOON array (`code_outline` symbols, a
/// `find_files` list; ADR-0053): each item costs its bytes as TOON writes it
/// ([`crate::wire::toon_element_bytes`]), a table row where the item is flat.
pub(crate) fn apply_budget_toon<T: Serialize>(items: Vec<T>, max_tokens: Option<u32>) -> Budgeted<T> {
    keep_prefix(items, max_tokens, |item| {
        serde_json::to_value(item).map_or(0, |value| crate::wire::toon_element_bytes(&value).div_ceil(4))
    })
}

/// [`apply_budget_by_path`] for `code_grep` rows, which the text answer writes as a plain block
/// (ADR-0053): a hit costs its rows as written, plus its file's `# <path>` header the first time a
/// kept hit is in that file, and its symbol's `## <symbol>` header the first time a kept hit is in
/// that file and symbol (none for the rows outside every symbol that open a file).
pub(crate) fn apply_budget_grep_rows(
    items: Vec<super::types::grep::GrepHit>,
    max_tokens: Option<u32>,
) -> Budgeted<super::types::grep::GrepHit> {
    let mut seen = GrepSeen::default();
    keep_prefix(items, max_tokens, |hit| grep_hit_bytes(hit, &mut seen).div_ceil(4))
}

/// The files and (file, symbol) groups a `code_grep` block has written a header for so far.
#[derive(Default)]
pub(crate) struct GrepSeen {
    files: ahash::AHashSet<crate::path::RelPath>,
    groups: ahash::AHashSet<(crate::path::RelPath, String)>,
}

/// Bytes `hit` adds to a `code_grep` row block after the hits `seen` holds: its rows, and the
/// headers it is the first to need.
pub(crate) fn grep_hit_bytes(hit: &super::types::grep::GrepHit, seen: &mut GrepSeen) -> u64 {
    let symbol = hit.symbol.as_deref().unwrap_or("");
    let opens_file = seen.files.insert(hit.path.clone());
    let file_bytes = if opens_file {
        crate::wire::header(&hit.path.to_str_lossy()).len() as u64 + 3
    } else {
        0
    };
    let new_group = seen.groups.insert((hit.path.clone(), symbol.to_owned()));
    let group_bytes = match (new_group, opens_file, symbol.is_empty()) {
        (false, _, _) | (true, true, true) => 0,
        (true, false, true) => 3,
        (true, _, false) => crate::wire::header(symbol).len() as u64 + 4,
    };
    hit.plain_bytes() + file_bytes + group_bytes
}

/// Keep the longest leading prefix of `items` whose summed `cost` fits `max_tokens`; the first item
/// is always kept so a page makes progress. `cost` is called once per item considered, in order.
pub(crate) fn keep_prefix<T>(items: Vec<T>, max_tokens: Option<u32>, mut cost: impl FnMut(&T) -> u64) -> Budgeted<T> {
    let Some(budget) = max_tokens.map(u64::from) else {
        return Budgeted { items, budgeted: false };
    };

    let total = items.len();
    let mut kept: Vec<T> = Vec::with_capacity(total);
    let mut used: u64 = 0;
    for item in items {
        let cost = cost(&item);
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

/// [`apply_budget`] for rows written under their file ([`super::by_path`]). A row costs what it
/// writes: its own fields with its column, plus its file's key the first time a kept row is in that
/// file, and its group's key the first time a kept row is in that file and group, since each key is
/// written once and not on the row.
pub(crate) fn apply_budget_by_path<T: super::by_path::Located>(items: Vec<T>, max_tokens: Option<u32>) -> Budgeted<T> {
    let mut files: ahash::AHashSet<crate::path::RelPath> = ahash::AHashSet::new();
    let mut groups: ahash::AHashSet<(crate::path::RelPath, String)> = ahash::AHashSet::new();
    keep_prefix(items, max_tokens, |row| {
        let mut bytes = Vec::new();
        let row_bytes = match row.serialize_row(&mut serde_json::Serializer::new(&mut bytes), true) {
            Ok(()) => bytes.len() as u64 + 1,
            Err(_) => 0,
        };
        let key_bytes = if files.insert(row.located().clone()) {
            // `"<path>":[` and `],`: the quoted key plus four bytes of punctuation.
            serde_json::to_vec(&row.located().to_str_lossy()).map_or(0, |key| key.len() as u64 + 4)
        } else {
            0
        };
        let group_bytes = match row.group() {
            // `"<group>":[` and `],`, as for a file.
            Some(group) if groups.insert((row.located().clone(), group.to_owned())) => {
                serde_json::to_vec(group).map_or(0, |key| key.len() as u64 + 4)
            }
            _ => 0,
        };
        // Rounded up, so the costs of many small rows do not sum below what they write.
        (row_bytes + key_bytes + group_bytes).div_ceil(4)
    })
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

    /// A row under a file and a group, writing `"x"`.
    struct Grouped(&'static str);

    impl crate::engine::by_path::Located for Grouped {
        fn located(&self) -> &crate::path::RelPath {
            static PATH: std::sync::LazyLock<crate::path::RelPath> =
                std::sync::LazyLock::new(|| crate::path::RelPath::from(b"a.rs".as_slice()));
            &PATH
        }

        fn line(&self) -> u32 {
            1
        }

        fn group(&self) -> Option<&str> {
            Some(self.0)
        }

        fn serialize_row<S: serde::Serializer>(&self, serializer: S, _column: bool) -> Result<S::Ok, S::Error> {
            serializer.serialize_str("x")
        }
    }

    #[test]
    fn a_group_key_is_charged_once_per_file_and_group() {
        const GROUP: &str = "a_thirty_byte_function_name_xx";
        const OTHER: &str = "another_thirty_byte_function_n";
        // `"x",` 4 bytes; `"a.rs":[` + `],` 10; a 30-byte group key 36: the first row costs
        // ceil(50 / 4) = 13 tokens, a row in a second group ceil(40 / 4) = 10, a row in a group
        // already written ceil(4 / 4) = 1.
        let rows = || vec![Grouped(GROUP), Grouped(OTHER), Grouped(GROUP)];
        let out = apply_budget_by_path(rows(), Some(22));
        assert_eq!(out.items.len(), 1, "the second group's key does not fit");
        assert!(out.budgeted);
        let out = apply_budget_by_path(rows(), Some(24));
        assert_eq!(out.items.len(), 3, "the third row's group is already written");
        assert!(!out.budgeted);
    }

    #[test]
    fn empty_input_is_never_budgeted() {
        let out = apply_budget(Vec::<Item>::new(), Some(0));
        assert!(out.items.is_empty());
        assert!(!out.budgeted);
    }

    #[test]
    fn the_grep_budget_charges_the_bytes_the_block_writes() {
        use crate::engine::types::grep::{GrepHit, GrepHits};

        let hit = |path: &str, line: u32, symbol: Option<&str>, before: &[&str], after: &[&str]| GrepHit {
            path: crate::path::RelPath::from(path),
            line,
            column: 3,
            write_column: line == 40,
            text: format!("text of {line} \"q\" ü"),
            symbol: symbol.map(str::to_string),
            context_before: before.iter().map(|s| (*s).to_string()).collect(),
            context_after: after.iter().map(|s| (*s).to_string()).collect(),
            start_byte: 0,
            stale: false,
            file_lines: 1000,
        };
        let hits = vec![
            hit("a b/x.rs", 9, None, &["b1", ""], &["a1"]),
            hit("a b/x.rs", 40, Some("main"), &[], &[]),
            hit("a b/x.rs", 100, None, &[], &["tail"]),
            hit(" lead.rs", 1, Some("\"q\""), &[], &[]),
        ];
        let mut seen = GrepSeen::default();
        let charged: u64 = hits.iter().map(|h| grep_hit_bytes(h, &mut seen)).sum();
        let value = serde_json::json!({"hits": serde_json::to_value(GrepHits::Rows(hits)).expect("rows")});
        let text = crate::wire::encode("code_grep", &value);
        assert_eq!(
            charged,
            text.len() as u64 + 1,
            "every row and header with its line break:\n{text}"
        );
    }
}
