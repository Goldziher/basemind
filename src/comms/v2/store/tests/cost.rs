//! What one inbox read costs in message rows, at the shapes that made it unbounded: every
//! thread of a repository full of unread messages, and a thread full of the reader's own posts.

use super::*;
use crate::comms::v2::api::{DEFAULT_LIMIT, MAX_ACTIVE_THREADS_PER_REPO, MAX_LIMIT};
use crate::comms::v2::store::inbox::{ROWS_READ, SCAN_BUDGET};

/// Rows `read` walks: message rows and the bodies it loads.
fn rows_of<T>(read: impl FnOnce() -> T) -> (T, usize) {
    ROWS_READ.with(|rows| rows.set(0));
    let value = read();
    (value, ROWS_READ.with(std::cell::Cell::get))
}

#[test]
fn a_page_over_every_thread_of_a_full_repository_walks_a_bounded_number_of_rows() {
    let harness = Harness::new();
    let unread_each = DEFAULT_LIMIT as usize + 1;
    for n in 0..MAX_ACTIVE_THREADS_PER_REPO {
        let thread = start(&harness, "b", &format!("topic {n}"), &["a"]).thread;
        for m in 1..unread_each {
            say(&harness, "b", &thread, &format!("{n}.{m}"));
        }
    }

    let (reply, rows) = rows_of(|| harness.inbox(&caller("a"), InboxBody::default()).expect("inbox"));

    assert_eq!(reply.messages.len(), DEFAULT_LIMIT as usize);
    assert!(reply.more);
    let bound = MAX_ACTIVE_THREADS_PER_REPO + SCAN_BUDGET + DEFAULT_LIMIT as usize + 1;
    eprintln!("rows walked by one page over {MAX_ACTIVE_THREADS_PER_REPO} threads: {rows} (bound {bound})");
    assert!(rows <= bound, "{rows} rows walked, over {bound}");
}

#[test]
fn own_posts_past_the_scan_budget_cost_a_bounded_read_and_do_not_hide_what_follows() {
    let harness = Harness::new();
    let thread = start(&harness, "b", "topic", &["a"]).thread;
    harness
        .inbox(&caller("a"), InboxBody::default())
        .expect("a reads the first post");
    let own = 2 * SCAN_BUDGET + 7;
    for n in 0..own {
        say(&harness, "a", &thread, &format!("own {n}"));
    }
    say(&harness, "b", &thread, "after");

    let bound = 1 + SCAN_BUDGET + MAX_LIMIT as usize + 1;
    let mut reads = 0;
    let delivered = loop {
        reads += 1;
        let (reply, rows) = rows_of(|| harness.inbox(&caller("a"), InboxBody::default()).expect("inbox"));
        eprintln!("marking read {reads}: {rows} rows (bound {bound})");
        assert!(rows <= bound, "{rows} rows walked, over {bound}");
        if !reply.messages.is_empty() {
            break reply;
        }
        assert!(
            reply.more,
            "an empty read that stopped at its budget says there is more"
        );
        assert!(reads < 4, "the reads make progress");
    };
    assert_eq!(bodies(&delivered), ["after"]);
    let (again, rows) = rows_of(|| harness.inbox(&caller("a"), InboxBody::default()).expect("inbox"));
    assert!(again.messages.is_empty() && !again.more);
    assert!(rows <= 2, "a read with nothing unread walks nothing: {rows}");
}

#[test]
fn a_peek_past_own_posts_pages_on_to_what_follows() {
    let harness = Harness::new();
    let thread = start(&harness, "b", "topic", &["a"]).thread;
    harness
        .inbox(&caller("a"), InboxBody::default())
        .expect("a reads the first post");
    for n in 0..SCAN_BUDGET + 3 {
        say(&harness, "a", &thread, &format!("own {n}"));
    }
    say(&harness, "b", &thread, "after");

    let first = harness.inbox(&caller("a"), peek()).expect("peek");
    assert!(first.messages.is_empty() && first.more);
    let cursor = first.next_cursor.expect("a cursor past the walked posts");
    let second = harness
        .inbox(
            &caller("a"),
            InboxBody {
                cursor: Some(cursor),
                ..peek()
            },
        )
        .expect("peek on");
    assert_eq!(bodies(&second), ["after"]);
}
