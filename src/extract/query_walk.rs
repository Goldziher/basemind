//! Bounded walks of a compiled extraction query over a parsed tree.
//!
//! The parse is bounded by the parse timeout, but a tree that parsed in time can still make the
//! query walk run for a long time (deeply nested machine-generated code, a minified bundle within
//! `max_file_bytes`), on a scan worker that checks for cancellation only between files. Every walk
//! is bounded in time by the same timeout, and in memory by a cap on in-progress matches.

use std::cell::RefCell;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, QueryCursorOptions, QueryCursorState, QueryMatch};

use super::ExtractError;

/// In-progress matches one walk may hold. Past it tree-sitter drops the oldest in-progress match,
/// so the result can miss a match; the walk reports that and the outline is marked as partial.
/// Generous: ordinary files hold a few dozen at once, so only a pathological tree reaches it.
const MATCH_LIMIT: u32 = 1 << 16;

thread_local! {
    // One cursor per thread, reused across files: building one allocates. ~keep
    static QUERY_CURSOR: RefCell<QueryCursor> = RefCell::new(bounded_cursor(MATCH_LIMIT));
}

fn bounded_cursor(limit: u32) -> QueryCursor {
    let mut cursor = QueryCursor::new();
    cursor.set_match_limit(limit);
    cursor
}

/// Run `on_match` on every match of `query` under `root`, with the per-thread cursor. Returns
/// whether the match limit dropped any match. A walk still running after the parse timeout stops
/// and is an [`ExtractError::ParseTimeout`], so the file keeps its previous rows and is retried on
/// the next scan instead of being recorded with an outline cut short.
///
/// Must not be called from inside `on_match`: the per-thread cursor is borrowed for the walk.
pub(super) fn for_each_match<'t>(
    query: &Query,
    root: Node<'t>,
    source: &[u8],
    on_match: impl FnMut(&QueryMatch<'_, 't>),
) -> Result<bool, ExtractError> {
    QUERY_CURSOR.with(|cell| {
        walk(
            &mut cell.borrow_mut(),
            query,
            root,
            source,
            crate::lang::parse_timeout(),
            on_match,
        )
    })
}

fn walk<'t>(
    cursor: &mut QueryCursor,
    query: &Query,
    root: Node<'t>,
    source: &[u8],
    timeout: Duration,
    mut on_match: impl FnMut(&QueryMatch<'_, 't>),
) -> Result<bool, ExtractError> {
    let started = Instant::now();
    let mut timed_out = false;
    let mut progress = |_: &QueryCursorState| -> ControlFlow<()> {
        if started.elapsed() > timeout {
            timed_out = true;
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let options = QueryCursorOptions::new().progress_callback(&mut progress);
    let mut matches = cursor.matches_with_options(query, root, source, options);
    while let Some(m) = matches.next() {
        on_match(m);
    }
    drop(matches);
    if timed_out {
        return Err(ExtractError::ParseTimeout(timeout));
    }
    Ok(cursor.did_exceed_match_limit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_tree(source: &[u8]) -> tree_sitter::Tree {
        crate::lang::with_parser("rust", |p| p.parse(source, None))
            .expect("rust grammar")
            .expect("parse")
    }

    fn nested_fns(depth: usize) -> Vec<u8> {
        let mut src = String::new();
        for i in 0..depth {
            src.push_str(&format!("fn f{i}() {{ "));
        }
        src.push_str(&"}".repeat(depth));
        src.into_bytes()
    }

    fn fn_query() -> Query {
        let lang = crate::lang::language("rust").expect("rust grammar");
        Query::new(
            &lang,
            "(function_item name: (identifier) @name body: (block) @body) @fn",
        )
        .expect("query")
    }

    /// A walk still running past the timeout stops and reports a parse timeout, instead of
    /// returning the matches it reached as if they were all of them.
    #[test]
    fn a_walk_past_the_timeout_is_a_parse_timeout() {
        let src = nested_fns(400);
        let tree = rust_tree(&src);
        let query = fn_query();
        let mut cursor = bounded_cursor(MATCH_LIMIT);
        let mut seen = 0usize;
        let dropped = walk(
            &mut cursor,
            &query,
            tree.root_node(),
            &src,
            Duration::from_secs(60),
            |_| seen += 1,
        );
        assert!(!dropped.expect("walk"), "nothing dropped under a generous limit");
        assert_eq!(seen, 400, "control: every nested fn matches");

        let timed_out = walk(&mut cursor, &query, tree.root_node(), &src, Duration::ZERO, |_| {});
        assert!(matches!(timed_out, Err(ExtractError::ParseTimeout(_))), "{timed_out:?}");
    }

    /// A pattern waiting for a sibling that never comes keeps one in-progress match per open block;
    /// past the limit some are dropped and the walk says so.
    #[test]
    fn a_walk_over_the_match_limit_reports_dropped_matches() {
        let src = nested_fns(64);
        let tree = rust_tree(&src);
        let lang = crate::lang::language("rust").expect("rust grammar");
        let query = Query::new(&lang, "(block (function_item) @f (let_declaration) @l)").expect("query");
        let mut cursor = bounded_cursor(MATCH_LIMIT);
        let dropped = walk(
            &mut cursor,
            &query,
            tree.root_node(),
            &src,
            Duration::from_secs(60),
            |_| {},
        );
        assert!(
            !dropped.expect("walk"),
            "control: nothing dropped under a generous limit"
        );
        let mut cursor = bounded_cursor(8);
        let dropped = walk(
            &mut cursor,
            &query,
            tree.root_node(),
            &src,
            Duration::from_secs(60),
            |_| {},
        );
        assert!(dropped.expect("walk"), "the limit dropped in-progress matches");
    }
}
