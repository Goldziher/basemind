//! The engine's operations: one synchronous `impl Workspace` method per verb tool, each in its own
//! file. Their arguments and answers are the tool's own types ([`super::types`]).

/// Symbols one `code_symbols` page holds when the caller passes no `limit`.
pub(crate) const SEARCH_LIMIT_DEFAULT: u32 = 100;
/// Upper bound on a `limit` that pages a symbol list.
pub(crate) const SEARCH_LIMIT_MAX: u32 = 1000;

/// Resolve a `limit` that pages a symbol, hit or note list: the page size (`default` when none was
/// asked, never above [`SEARCH_LIMIT_MAX`]) and whether the caller's `limit` was lowered to that
/// maximum, which the response reports as `limit_clamped` rather than capping silently.
pub(crate) fn search_limit(requested: Option<u32>, default: u32) -> (usize, bool) {
    let asked = requested.unwrap_or(default);
    (asked.min(SEARCH_LIMIT_MAX) as usize, asked > SEARCH_LIMIT_MAX)
}

#[cfg(test)]
mod search_limit_tests {
    use super::*;

    #[test]
    fn a_limit_above_the_maximum_is_applied_as_the_maximum_and_flagged() {
        assert_eq!(search_limit(Some(SEARCH_LIMIT_MAX + 1), SEARCH_LIMIT_DEFAULT), (1000, true));
        assert_eq!(search_limit(Some(u32::MAX), SEARCH_LIMIT_DEFAULT), (1000, true));
    }

    #[test]
    fn a_limit_at_or_below_the_maximum_is_kept_and_not_flagged() {
        assert_eq!(search_limit(Some(SEARCH_LIMIT_MAX), SEARCH_LIMIT_DEFAULT), (1000, false));
        assert_eq!(search_limit(Some(7), SEARCH_LIMIT_DEFAULT), (7, false));
    }

    #[test]
    fn no_limit_applies_the_default_and_is_not_flagged() {
        assert_eq!(search_limit(None, SEARCH_LIMIT_DEFAULT), (100, false));
        assert_eq!(search_limit(None, 500), (500, false));
    }
}

pub(crate) mod code_grep;
mod code_outline;
mod code_read;
mod code_refs;
mod code_search;
mod code_symbols;
mod cycles;
mod deadcode;
mod deadcode_files;
mod deadcode_lex;
mod deadcode_rust;
mod deadcode_symbols;
mod docs_search;
mod explain;
pub(crate) mod find_files;
mod graph_report;
mod impact;
mod impact_calls;
mod impact_order;
pub(crate) mod index;
mod memory;
mod migration;
pub(crate) mod overview;
