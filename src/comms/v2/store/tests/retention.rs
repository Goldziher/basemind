//! The sweep as only the store has it: bounded, run by changes, and past rows it cannot decode.

use super::*;

/// Rows of every keyspace when the store holds nothing but the seen records of `actors`
/// callers: one `seen` row and one `expiry` entry each.
fn only_seen(actors: usize) -> [usize; 12] {
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, actors, actors]
}

#[test]
fn sweep_removes_no_more_than_its_budget_and_says_when_more_is_left() {
    let harness = Harness::new();
    for n in 0..5 {
        let created = start(&harness, "a", &format!("topic {n}"), &["b"]);
        say(&harness, "a", &created.thread, "second");
    }
    harness.inbox(&caller("b"), InboxBody::default()).expect("inbox");
    harness
        .clock
        .advance(SEEN_TTL + SEEN_REFRESH_INTERVAL + Duration::from_secs(1));
    let before = harness.store().stored_rows();

    let first = harness.store().sweep_bounded(3).expect("sweep");

    assert_eq!(
        first,
        SweepReport {
            messages: 3,
            more: true,
            ..SweepReport::default()
        }
    );
    let after = harness.store().stored_rows();
    assert_eq!(after[1..4], [before[1] - 3, before[2] - 3, before[3] - 3]);
    assert_eq!(
        after[0], before[0],
        "no thread goes while the budget is spent on messages"
    );

    let mut removed = first;
    while removed.more {
        let next = harness.store().sweep_bounded(3).expect("sweep");
        assert!(
            next.messages + next.threads * 3 + next.replays + next.seen <= 3 + 2,
            "a sweep passes its budget by one thread at most: {next:?}"
        );
        removed = SweepReport {
            messages: removed.messages + next.messages,
            threads: removed.threads + next.threads,
            seen: removed.seen + next.seen,
            more: next.more,
            ..removed
        };
    }
    assert_eq!((removed.messages, removed.threads, removed.seen), (10, 5, 2));
    assert_eq!(harness.store().stored_rows(), only_seen(0));
    assert_eq!(harness.store().sweep(), Ok(SweepReport::default()));
}

#[test]
fn sweep_leaves_what_retention_still_keeps() {
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    harness
        .post(&keyed("a", "relay-1"), into(&created.thread, "second"))
        .expect("post");
    harness.inbox(&caller("b"), InboxBody::default()).expect("inbox");
    let before = harness.store().stored_rows();

    harness.clock.advance(REPLAY_TTL - Duration::from_secs(1));

    assert_eq!(harness.store().sweep(), Ok(SweepReport::default()));
    assert_eq!(harness.store().stored_rows(), before);
}

#[test]
fn sweep_removes_a_seen_record_with_the_marks_of_its_actor_only() {
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    harness.inbox(&caller("b.helper"), InboxBody::default()).expect("inbox");
    harness.clock.advance(SEEN_TTL);
    say(&harness, "a", &created.thread, "second");
    harness.inbox(&caller("b"), InboxBody::default()).expect("inbox");
    harness.clock.advance(SEEN_REFRESH_INTERVAL + Duration::from_secs(1));

    let report = harness.store().sweep().expect("sweep");

    assert_eq!((report.messages, report.seen, report.threads), (1, 1, 0));
    let rows = harness.store().stored_rows();
    assert_eq!(rows[6..8], [1, 1], "the marks of `b` stay, with their index entry");
    assert_eq!(harness.position(&created.thread, &caller("b").actor), Some((2, 2)));
    assert_eq!(
        harness.position(&created.thread, &caller("b.helper").actor),
        Some((2, 2)),
        "a returning actor starts where its principal has read to"
    );
}

#[test]
fn thread_id_of_a_swept_thread_can_be_used_again_without_old_rows() {
    let harness = Harness::with_ids(vec![7, 7]);
    let old = start(&harness, "a", "old", &["b"]);
    for _ in 0..3 {
        say(&harness, "a", &old.thread, "more");
    }
    harness.inbox(&caller("b"), InboxBody::default()).expect("inbox");
    harness
        .clock
        .advance(SEEN_TTL + SEEN_REFRESH_INTERVAL + Duration::from_secs(1));
    harness.sweep();
    assert_eq!(harness.store().stored_rows(), only_seen(0));

    let new = start(&harness, "c", "new", &["b"]);

    assert_eq!(new.thread, old.thread);
    assert_eq!(
        bodies(&harness.inbox(&caller("b"), history()).expect("inbox")),
        ["first"]
    );
    assert_eq!(harness.position(&new.thread, &caller("b").actor), Some((0, 0)));
}

#[test]
fn changes_sweep_as_they_go_so_a_store_without_a_daemon_tick_does_not_grow() {
    let harness = Harness::new();
    for n in 0..40 {
        start(&harness, "a", &format!("topic {n}"), &[]);
    }
    harness
        .clock
        .advance(SEEN_TTL + SEEN_REFRESH_INTERVAL + Duration::from_secs(1));
    let threads = || harness.store().stored_rows()[0];

    harness
        .inbox(&caller("z"), peek())
        .expect("a first call writes a seen record");
    assert_eq!(
        threads(),
        28,
        "64 records: 40 messages, then 12 threads with their member"
    );

    harness.clock.advance(Duration::from_secs(2));
    harness.inbox(&caller("z"), peek()).expect("a read that writes nothing");
    assert_eq!(threads(), 28, "a call that writes nothing runs no sweep");

    start(&harness, "z", "new", &[]);
    let rows = harness.store().stored_rows();
    assert_eq!(
        (rows[0], rows[10]),
        (1, 1),
        "the new thread, and the seen record of `z`"
    );
}

#[test]
fn sweep_removes_rows_it_cannot_decode_and_goes_on() {
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    let broken = start(&harness, "a", "broken", &[]);
    harness
        .post(&keyed("a", "relay-1"), into(&created.thread, "second"))
        .expect("post");
    let store = harness.store();
    let ks = &store.ks;
    ks.post_order
        .insert(0u64.to_be_bytes(), b"not an entry")
        .expect("write");
    ks.post_order
        .insert(keys::order(i64::MIN, 0), keys::message(&created.thread, 9))
        .expect("write");
    ks.messages
        .insert(keys::message(&created.thread, 3), b"not a message")
        .expect("write");
    ks.threads
        .insert(keys::thread(&broken.thread), b"not a thread")
        .expect("write");
    let no_timestamp = |kind: keys::Expiry| vec![kind as u8, 7];
    for key in [
        no_timestamp(keys::Expiry::Thread),
        keys::expiry(keys::Expiry::Thread, i64::MIN, b"x"),
        no_timestamp(keys::Expiry::Replay),
        no_timestamp(keys::Expiry::Seen),
        no_timestamp(keys::Expiry::OverCap),
    ] {
        ks.expiry.insert(key, []).expect("write");
    }
    harness
        .clock
        .advance(SEEN_TTL + SEEN_REFRESH_INTERVAL + Duration::from_secs(1));

    let first = store.sweep_bounded(1).expect("sweep");
    assert_eq!(
        first,
        SweepReport {
            skipped_unreadable: 1,
            more: true,
            ..SweepReport::default()
        },
        "an unreadable row counts against the budget like any other"
    );

    let rest = store.sweep().expect("sweep");
    assert_eq!(
        rest,
        SweepReport {
            messages: 3,
            threads: 1,
            replays: 1,
            seen: 1,
            skipped_unreadable: 8,
            more: false,
        }
    );
    let rows = store.stored_rows();
    assert_eq!(
        (rows[0], rows[1], rows[11]),
        (1, 0, 0),
        "the thread whose row does not decode stays, without its messages or an expiry entry"
    );
    assert_eq!(store.sweep(), Ok(SweepReport::default()));
}
