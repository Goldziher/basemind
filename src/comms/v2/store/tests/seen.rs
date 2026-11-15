//! When a call writes the caller's seen record, and that a read which need not write takes no
//! writer lock and runs no sweep.

use std::sync::mpsc;

use super::*;

/// How long a read may take before it is held to be waiting for the writer lock.
const LOCK_FREE_WITHIN: Duration = Duration::from_secs(10);

#[test]
fn first_call_records_the_caller_and_later_calls_rewrite_the_record_once_a_day() {
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    let store = harness.store();
    assert_eq!(store.recorded_seen("a"), Some(START_MICROS));
    assert_eq!(store.recorded_seen("b"), None);

    harness.clock.advance(Duration::from_secs(60));
    harness.inbox(&caller("b"), peek()).expect("b's first call");
    let first_call = START_MICROS + 60_000_000;
    assert_eq!(store.recorded_seen("b"), Some(first_call));

    harness.clock.advance(SEEN_REFRESH_INTERVAL - Duration::from_secs(1));
    harness.inbox(&caller("b"), peek()).expect("inbox");
    harness.inbox(&caller("b"), InboxBody::default()).expect("inbox");
    say(&harness, "b", &created.thread, "second");
    assert_eq!(store.recorded_seen("b"), Some(first_call), "reads and changes alike");

    harness.clock.advance(Duration::from_secs(1));
    harness.inbox(&caller("b"), peek()).expect("inbox");
    assert_eq!(
        store.recorded_seen("b"),
        Some(first_call + rules::ttl_micros(SEEN_REFRESH_INTERVAL))
    );
    assert_eq!(
        store.stored_rows()[8..],
        [2, 2 + 1],
        "one seen row and one expiry entry each"
    );
}

#[test]
fn seen_record_written_ahead_of_the_clock_is_rewritten_at_the_clock() {
    let harness = Harness::new();
    harness.clock.advance(Duration::from_secs(3600));
    start(&harness, "a", "topic", &["b"]);
    harness.rewind(Duration::from_secs(3600));

    harness
        .threads_query(&caller("a"), ThreadQuery::default())
        .expect("threads");

    assert_eq!(harness.store().recorded_seen("a"), Some(START_MICROS));
}

#[test]
fn read_by_a_caller_seen_within_the_interval_writes_nothing_and_runs_no_sweep() {
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    harness
        .post(&keyed("a", "relay-1"), into(&created.thread, "second"))
        .expect("post");
    harness.inbox(&caller("b"), peek()).expect("b's first call");
    harness.clock.advance(REPLAY_TTL + Duration::from_secs(1));
    let store = harness.store();
    let rows = store.stored_rows();
    assert_eq!(rows[7], 1, "an expired replay entry a sweep would remove");

    harness.inbox(&caller("b"), peek()).expect("peek");
    harness.inbox(&caller("b"), history()).expect("history");
    harness
        .inbox(
            &caller("b"),
            InboxBody {
                select: InboxSelect::Message(created.id.clone()),
                ..InboxBody::default()
            },
        )
        .expect("by id");
    harness
        .threads_query(&caller("b"), ThreadQuery::default())
        .expect("list");
    harness
        .threads_query(&caller("b"), ThreadQuery::Describe(created.thread.clone()))
        .expect("describe");

    assert_eq!(store.stored_rows(), rows, "no row was written and none was swept");
    assert_eq!(store.recorded_seen("b"), Some(START_MICROS));

    harness.clock.advance(SEEN_REFRESH_INTERVAL);
    harness.inbox(&caller("b"), peek()).expect("peek");
    assert_eq!(
        store.stored_rows()[7],
        0,
        "the read that rewrote the record swept as a change does"
    );
}

#[test]
fn read_by_a_caller_seen_within_the_interval_takes_no_writer_lock() {
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    harness.inbox(&caller("b"), peek()).expect("b's first call");
    harness.clock.advance(SEEN_REFRESH_INTERVAL - Duration::from_secs(1));
    let (done, finished) = mpsc::channel();

    let writer = harness.store().lock();
    let replies = std::thread::scope(|scope| {
        let (harness, thread) = (&harness, &created.thread);
        scope.spawn(move || {
            let peeked = harness.inbox(&caller("b"), peek());
            let listed = harness.threads_query(&caller("b"), ThreadQuery::Describe(thread.clone()));
            done.send((peeked, listed)).expect("the test is listening");
        });
        let replies = finished.recv_timeout(LOCK_FREE_WITHIN);
        drop(writer);
        replies
    });

    let (peeked, listed) = replies.expect("the reads returned while the writer lock was held");
    assert_eq!(bodies(&peeked.expect("peek")), ["first"]);
    assert_eq!(listed.expect("describe").threads[0].unread, 1);
}
