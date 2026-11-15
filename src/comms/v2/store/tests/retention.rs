//! The sweep: bounded, in expiry order, and never needed for a read to be right.

use super::*;

/// How far ahead a wrong clock reads in the tests about one: five years.
const AHEAD: Duration = Duration::from_secs(5 * 365 * 24 * 60 * 60);

/// Rows of every keyspace when the store holds nothing but the seen records of `agents`
/// callers: one `seen` row and one `expiry` entry each.
fn only_seen(agents: usize) -> [usize; 10] {
    [0, 0, 0, 0, 0, 0, 0, 0, agents, agents]
}

#[test]
fn sweep_removes_no_more_than_its_budget_and_says_when_more_is_left() {
    let harness = Harness::new();
    for n in 0..5 {
        let created = start(&harness, "a", &format!("topic {n}"), &[]);
        say(&harness, "a", &created.thread, "second");
    }
    harness
        .clock
        .advance(MESSAGE_TTL + SEEN_REFRESH_INTERVAL + Duration::from_secs(1));
    let before = harness.store().stored_rows();

    let first = harness.store().sweep_bounded(3).expect("sweep");

    assert_eq!(
        first,
        SweepReport {
            messages: 3,
            threads: 0,
            replays: 0,
            seen: 0,
            skipped_unreadable: 0,
            more: true,
        }
    );
    let after = harness.store().stored_rows();
    assert_eq!(after[1], before[1] - 3, "three message rows");
    assert_eq!(after[2], before[2] - 3, "three bodies");
    assert_eq!(after[3], before[3] - 3, "three post order entries");
    assert_eq!(after[0], before[0], "no thread goes while a message of it may remain");

    let mut removed = first;
    let mut sweeps = 1;
    while removed.more {
        let next = harness.store().sweep_bounded(3).expect("sweep");
        assert!(
            next.messages + next.threads * 2 + next.replays + next.seen <= 3 + 1,
            "{next:?}"
        );
        removed = SweepReport {
            messages: removed.messages + next.messages,
            threads: removed.threads + next.threads,
            replays: removed.replays + next.replays,
            seen: removed.seen + next.seen,
            skipped_unreadable: removed.skipped_unreadable + next.skipped_unreadable,
            more: next.more,
        };
        sweeps += 1;
    }
    assert_eq!((removed.messages, removed.threads, removed.seen), (10, 5, 1));
    assert!(sweeps >= 6, "{sweeps} sweeps of three records each");
    assert_eq!(harness.store().stored_rows(), only_seen(0));
    assert_eq!(harness.store().sweep(), Ok(SweepReport::default()));
}

#[test]
fn expired_records_are_invisible_before_any_sweep_removes_them() {
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    harness.clock.advance(MESSAGE_TTL + Duration::from_secs(1));
    let rows = harness.store().stored_rows();
    let reader_only = |body: InboxBody| harness.inbox(&caller("b"), body);

    assert_eq!(harness.position(&created.thread, &caller("b").actor), None);
    assert_eq!(
        reader_only(InboxBody {
            select: InboxSelect::Message(created.id.clone()),
            ..InboxBody::default()
        }),
        Err(CommsError::MessageNotFound { id: created.id })
    );
    assert_eq!(rows[0], 1, "the thread row is still stored");
    assert_eq!(rows[1], 1, "the message row is still stored");
}

#[test]
fn sweep_leaves_what_has_not_expired() {
    let harness = Harness::new();
    let old = start(&harness, "a", "old", &["b"]);
    harness.clock.advance(MESSAGE_TTL - Duration::from_secs(60));
    let kept = say(&harness, "a", &old.thread, "kept");
    let young = start(&harness, "a", "young", &["b"]);
    harness.clock.advance(Duration::from_secs(120));

    let report = harness.store().sweep().expect("sweep");

    assert_eq!((report.messages, report.threads, report.more), (1, 0, false));
    let reply = harness.inbox(&caller("b"), InboxBody::default()).expect("inbox");
    assert_eq!(
        reply
            .messages
            .iter()
            .map(|message| message.id.clone())
            .collect::<Vec<_>>(),
        [kept.id, young.id]
    );
    assert_eq!(harness.position(&old.thread, &caller("b").actor), Some((2, 2)));
}

#[test]
fn sweep_removes_expired_replay_entries_and_seen_records() {
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    harness
        .post(&keyed("a", "relay-1"), into(&created.thread, "second"))
        .expect("post");
    harness
        .inbox(&keyed("b", "relay-2"), InboxBody::default())
        .expect("inbox");
    assert_eq!(harness.store().stored_rows()[7], 2, "two replay entries");

    harness.clock.advance(REPLAY_TTL + Duration::from_secs(1));
    let report = harness.store().sweep().expect("sweep");
    assert_eq!((report.replays, report.seen, report.messages), (2, 0, 0));
    assert_eq!(harness.store().stored_rows()[7], 0);

    harness.clock.advance(SEEN_TTL + SEEN_REFRESH_INTERVAL);
    let report = harness.store().sweep().expect("sweep");
    assert_eq!((report.messages, report.threads, report.seen), (2, 1, 2));
    assert_eq!(harness.store().stored_rows(), only_seen(0));
}

#[test]
fn replay_key_used_again_after_it_expired_leaves_one_entry_and_one_expiry() {
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    let relayed = keyed("a", "relay-1");
    harness.post(&relayed, into(&created.thread, "second")).expect("post");
    let rows = harness.store().stored_rows();

    harness.clock.advance(REPLAY_TTL + Duration::from_secs(1));
    harness.post(&relayed, into(&created.thread, "third")).expect("post");

    let after = harness.store().stored_rows();
    assert_eq!(after[7], rows[7], "the expired entry was replaced");
    assert_eq!(after[9], rows[9], "and so was its place in the expiry index");
}

#[test]
fn thread_id_of_a_swept_thread_can_be_used_again_without_old_rows() {
    let harness = Harness::with_ids(vec![4, 4]);
    let old = start(&harness, "a", "old", &["b"]);
    say(&harness, "a", &old.thread, "second");
    harness.clock.advance(MESSAGE_TTL + Duration::from_secs(1));
    while harness.store().sweep_bounded(1).expect("sweep").more {}

    let fresh = start(&harness, "c", "fresh", &["d"]);

    assert_eq!(fresh.thread, old.thread);
    assert_eq!(harness.position(&fresh.thread, &caller("b").actor), None);
    assert_eq!(
        bodies(&harness.inbox(&caller("d"), history()).expect("inbox")),
        ["first"]
    );
    assert_eq!(
        harness.inbox(&caller("b"), history()).expect("inbox").messages,
        Vec::new()
    );
}

#[test]
fn changes_sweep_as_they_go_so_a_store_without_a_daemon_tick_does_not_grow() {
    let harness = Harness::new();
    for n in 0..4 {
        start(&harness, "a", &format!("topic {n}"), &[]);
    }
    harness.clock.advance(MESSAGE_TTL + SEEN_TTL);

    harness
        .threads_query(&caller("z"), ThreadQuery::default())
        .expect("threads");

    assert_eq!(harness.store().stored_rows(), only_seen(1));
}

#[test]
fn sweep_removes_index_rows_it_cannot_decode_and_goes_on() {
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    harness
        .post(&keyed("a", "relay-1"), into(&created.thread, "second"))
        .expect("post");
    let store = harness.store();
    store
        .ks
        .post_order
        .insert(0u64.to_be_bytes(), b"not an entry")
        .expect("write");
    let no_timestamp = |kind: keys::Expiry| vec![kind as u8, 7];
    let no_thread = keys::expiry(keys::Expiry::Thread, i64::MIN, b"x");
    for key in [
        no_timestamp(keys::Expiry::Thread),
        no_thread,
        no_timestamp(keys::Expiry::Replay),
        no_timestamp(keys::Expiry::Seen),
    ] {
        store.ks.expiry.insert(key, []).expect("write");
    }
    harness
        .clock
        .advance(MESSAGE_TTL + SEEN_TTL + SEEN_REFRESH_INTERVAL + Duration::from_secs(1));

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
            messages: 2,
            threads: 1,
            replays: 1,
            seen: 1,
            skipped_unreadable: 4,
            more: false,
        }
    );
    assert_eq!(store.stored_rows(), only_seen(0));
    assert_eq!(store.sweep(), Ok(SweepReport::default()));
}

#[test]
fn sweep_out_of_budget_on_an_undecodable_message_index_row_removes_nothing() {
    let harness = Harness::new();
    start(&harness, "a", "topic", &[]);
    let store = harness.store();
    store
        .ks
        .post_order
        .insert(0u64.to_be_bytes(), b"not an entry")
        .expect("write");
    harness.clock.advance(MESSAGE_TTL + Duration::from_secs(1));
    let before = store.stored_rows();

    let none = store.sweep_bounded(0).expect("sweep");
    assert_eq!(
        none,
        SweepReport {
            more: true,
            ..SweepReport::default()
        }
    );
    assert_eq!(
        store.stored_rows(),
        before,
        "no row goes without budget, the bad one included"
    );

    let one = store.sweep_bounded(1).expect("sweep");
    assert_eq!(
        one,
        SweepReport {
            skipped_unreadable: 1,
            more: true,
            ..SweepReport::default()
        }
    );
    let after = store.stored_rows();
    assert_eq!(
        (after[0], after[1], after[3]),
        (1, 1, 1),
        "the thread stays while its message does"
    );

    let rest = store.sweep().expect("sweep");
    assert_eq!((rest.messages, rest.threads, rest.more), (1, 1, false));
    assert_eq!(store.stored_rows(), only_seen(1));
}

#[test]
fn expired_thread_is_swept_with_the_messages_stamped_ahead_of_its_last_post() {
    let harness = Harness::new();
    harness.clock.advance(AHEAD);
    let ahead = start(&harness, "a", "made ahead", &["b"]);
    say(&harness, "a", &ahead.thread, "ahead too");
    say(&harness, "a", &ahead.thread, "and this");
    harness.rewind(AHEAD);
    say(&harness, "a", &ahead.thread, "after the correction");
    harness
        .clock
        .advance(MESSAGE_TTL + SEEN_TTL + SEEN_REFRESH_INTERVAL + Duration::from_secs(1));
    let store = harness.store();

    let first = store.sweep_bounded(2).expect("sweep");
    assert_eq!(
        first,
        SweepReport {
            messages: 2,
            more: true,
            ..SweepReport::default()
        },
        "the one message expired by its stamp, and one of the three stamped ahead"
    );
    let rows = store.stored_rows();
    assert_eq!(
        (rows[0], rows[1], rows[2], rows[3]),
        (1, 2, 2, 2),
        "the thread stays while a message of it does"
    );

    let second = store.sweep_bounded(2).expect("sweep");
    assert_eq!(
        second,
        SweepReport {
            messages: 2,
            threads: 1,
            more: true,
            ..SweepReport::default()
        }
    );
    let third = store.sweep_bounded(2).expect("sweep");
    assert_eq!(
        third,
        SweepReport {
            seen: 1,
            ..SweepReport::default()
        }
    );
    assert_eq!(store.stored_rows(), only_seen(0));
}

#[test]
fn message_stamped_ahead_stays_in_a_thread_that_is_still_posted_to() {
    let harness = Harness::new();
    harness.clock.advance(AHEAD);
    let ahead = start(&harness, "a", "made ahead", &["b"]);
    harness.rewind(AHEAD);
    say(&harness, "a", &ahead.thread, "after the correction");
    harness.clock.advance(MESSAGE_TTL - Duration::from_secs(1));
    say(&harness, "a", &ahead.thread, "keeps the thread");
    harness.clock.advance(Duration::from_secs(2));

    let report = harness.store().sweep().expect("sweep");

    assert_eq!((report.messages, report.threads, report.more), (1, 0, false));
    assert_eq!(
        bodies(&harness.inbox(&caller("b"), history()).expect("inbox")),
        ["first", "keeps the thread"]
    );
}

#[test]
fn message_whose_index_row_is_gone_is_swept_with_its_thread() {
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &[]);
    say(&harness, "a", &created.thread, "second");
    let store = harness.store();
    let oldest = store
        .ks
        .post_order
        .iter()
        .next()
        .expect("an index row")
        .key()
        .expect("read");
    store.ks.post_order.remove(oldest).expect("remove");
    harness.clock.advance(MESSAGE_TTL + Duration::from_secs(1));

    let report = store.sweep().expect("sweep");

    assert_eq!((report.messages, report.threads, report.more), (2, 1, false));
    assert_eq!(store.stored_rows(), only_seen(1));
}
