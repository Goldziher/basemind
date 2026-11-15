//! Retention and the store version: what a sweep removes, what a wrong clock does, what a newer
//! store refuses.

use std::time::Duration;

use super::super::api::{
    Actor, COMMS_STORE_VER, CommsError, InboxBody, MAX_MESSAGES_PER_THREAD, MESSAGE_TTL, ThreadAction, ThreadFilter,
    ThreadId,
};
use super::{
    ConformanceHarness, agent, agents, bodies, by_id, change, find, history, ids, list, notify, only, peek, read,
    read_as, say, start, thread_ids, who,
};

const SECOND: Duration = Duration::from_secs(1);

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn no_threads() -> Vec<ThreadId> {
    Vec::new()
}

/// Past the per-thread cap a sweep drops the oldest messages, and the unread count is of what
/// is still stored.
pub fn sweep_drops_messages_outside_the_newest_of_their_thread(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    let second = say(h, "a", &created.thread, "more").expect("post");
    for _ in 1..MAX_MESSAGES_PER_THREAD {
        say(h, "a", &created.thread, "more").expect("post");
    }
    h.sweep();

    assert_eq!(
        read_as(h, "b", by_id(&created.id)).map(|reply| reply.messages),
        Err(CommsError::MessageNotFound { id: created.id })
    );
    assert_eq!(
        ids(&read_as(h, "b", by_id(&second.id)).expect("inbox")),
        std::slice::from_ref(&second.id)
    );

    let summary = &list(h, "b").threads[0];
    assert_eq!(summary.head, MAX_MESSAGES_PER_THREAD + 1);
    assert_eq!(summary.unread, MAX_MESSAGES_PER_THREAD);

    let oldest = read_as(
        h,
        "b",
        InboxBody {
            limit: Some(1),
            ..InboxBody::default()
        },
    )
    .expect("inbox");
    assert_eq!(ids(&oldest), [second.id]);
    assert_eq!(list(h, "b").threads[0].unread, MAX_MESSAGES_PER_THREAD - 1);
}

/// No read hides a record for its age: a message past its retention is returned until a sweep
/// removes it, and the sweep removes its idle thread with it.
pub fn record_past_its_retention_is_returned_until_a_sweep_removes_it(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);

    h.advance(MESSAGE_TTL - SECOND);
    h.sweep();
    assert_eq!(h.thread_count(), 1);

    h.advance(2 * SECOND);
    assert_eq!(
        ids(&read_as(h, "b", peek()).expect("inbox")),
        std::slice::from_ref(&created.id)
    );

    h.sweep();
    assert_eq!(h.thread_count(), 0);
    assert_eq!(thread_ids(&list(h, "a")), no_threads());
    assert_eq!(bodies(&read(h, "b")), Vec::<&str>::new());
    assert_eq!(
        say(h, "a", &created.thread, "late"),
        Err(CommsError::ThreadNotFound {
            thread: created.thread.clone()
        })
    );
    assert_eq!(
        read_as(h, "b", only(&created.thread)).map(|reply| reply.messages),
        Err(CommsError::ThreadNotFound { thread: created.thread })
    );
}

/// Messages are swept one by one: an old message goes while its thread and its newer messages
/// stay.
pub fn sweep_removes_old_messages_inside_a_live_thread(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    h.advance(20 * DAY);
    let second = say(h, "a", &created.thread, "second").expect("post");
    h.advance(15 * DAY);
    h.sweep();

    assert_eq!(h.thread_count(), 1);
    let summary = &list(h, "b").threads[0];
    assert_eq!(summary.unread, 1);
    assert_eq!(summary.head, 2);
    assert_eq!(
        read_as(h, "b", by_id(&created.id)).map(|reply| reply.messages),
        Err(CommsError::MessageNotFound { id: created.id })
    );
    assert_eq!(bodies(&read_as(h, "b", history()).expect("inbox")), ["second"]);
    let unread = read(h, "b");
    assert_eq!(ids(&unread), [second.id]);
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((2, 2)));
}

/// History pages by cursor through a thread whose oldest messages a sweep removed.
pub fn cursor_pages_history_after_a_sweep_removed_the_oldest_messages(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    say(h, "a", &created.thread, "second").expect("post");
    h.advance(20 * DAY);
    for body in ["third", "fourth", "fifth"] {
        say(h, "a", &created.thread, body).expect("post");
    }
    h.advance(15 * DAY);
    h.sweep();

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = read_as(
            h,
            "b",
            InboxBody {
                limit: Some(1),
                cursor: cursor.take(),
                ..history()
            },
        )
        .expect("inbox");
        seen.extend(page.messages.into_iter().map(|message| message.body));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen, ["third", "fourth", "fifth"]);
}

/// Archiving does not extend or shorten retention.
pub fn archived_thread_is_swept_like_any_other(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    change(h, &who(h, "a"), &created.thread, ThreadAction::Archive).expect("archive");
    let archived = ThreadFilter {
        archived: true,
        ..ThreadFilter::default()
    };

    h.advance(MESSAGE_TTL - SECOND);
    h.sweep();
    assert_eq!(
        thread_ids(&find(h, &who(h, "b"), archived.clone()).expect("threads")),
        [created.thread]
    );

    h.advance(2 * SECOND);
    h.sweep();
    assert_eq!(
        thread_ids(&find(h, &who(h, "b"), archived).expect("threads")),
        no_threads()
    );
    assert_eq!(h.thread_count(), 0);
}

/// The floor of a timestamp is its thread's last one. A clock that jumps forward and comes back
/// pins only the threads posted to meanwhile: their later stamps stay ahead of the clock, and a
/// thread left alone is stamped from the clock again.
pub fn forward_clock_jump_pins_only_the_threads_posted_to_meanwhile(h: &impl ConformanceHarness) {
    let pinned = start(h, "a", "pinned", &["b"]);
    let free = start(h, "a", "free", &["b"]);
    h.advance(10 * DAY);
    say(h, "a", &pinned.thread, "during the jump").expect("post");
    h.rewind(10 * DAY);
    h.advance(SECOND);
    say(h, "a", &free.thread, "after, elsewhere").expect("post");
    say(h, "a", &pinned.thread, "after, here").expect("post");

    let stamps = |thread: &ThreadId| -> Vec<i64> {
        let page = read_as(h, "b", InboxBody { ..only(thread) }).expect("inbox");
        page.messages.iter().map(|message| message.ts_micros).collect()
    };
    let (pinned, free) = (stamps(&pinned.thread), stamps(&free.thread));
    assert_eq!(pinned[2], pinned[1] + 1, "one microsecond past the thread's last stamp");
    assert!(free[1] > free[0]);
    assert!(free[1] < pinned[1], "the other thread follows the clock");
}

/// A sweep judges age against the clock as it reads when the sweep runs. Under a clock that
/// reads more than the retention ahead it removes every message and thread, and correcting the
/// clock brings nothing back.
pub fn sweep_under_a_clock_far_ahead_removes_everything_for_good(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    start(h, "a", "another", &[]);

    h.advance(MESSAGE_TTL + DAY);
    h.sweep();
    h.rewind(MESSAGE_TTL + DAY);

    assert_eq!(h.thread_count(), 0);
    assert_eq!(read(h, "b").messages, Vec::new());
    assert_eq!(
        read_as(h, "b", by_id(&created.id)).map(|reply| reply.messages),
        Err(CommsError::MessageNotFound { id: created.id })
    );
}

/// A store newer than the binary refuses whatever would write to it, the hooks' read included,
/// and still answers what only reads: a peek, history, a read by id and a listing.
pub fn store_newer_than_the_binary_is_refused_for_writing_only(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    h.set_store_version(COMMS_STORE_VER + 1);
    let newer = CommsError::StoreNewer {
        store: COMMS_STORE_VER + 1,
        binary: COMMS_STORE_VER,
    };

    assert_eq!(say(h, "a", &created.thread, "hi"), Err(newer.clone()));
    assert_eq!(
        change(h, &who(h, "c"), &created.thread, ThreadAction::Join),
        Err(newer.clone())
    );
    assert_eq!(
        read_as(h, "b", InboxBody::default()).map(|reply| reply.messages),
        Err(newer.clone())
    );
    let refused = read_as(h, "b", notify()).expect_err("a hook's read writes notified_seq");
    assert_eq!(refused, newer);
    assert!(refused.to_string().contains("upgrade basemind"), "{refused}");

    assert_eq!(
        ids(&read_as(h, "b", peek()).expect("inbox")),
        std::slice::from_ref(&created.id)
    );
    assert_eq!(bodies(&read_as(h, "b", history()).expect("inbox")), ["first"]);
    let whole = read_as(h, "b", by_id(&created.id)).expect("a read by id writes nothing");
    assert_eq!(bodies(&whole), ["first"]);
    assert_eq!(thread_ids(&list(h, "b")), std::slice::from_ref(&created.thread));
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((0, 0)));
}

/// On a newer store not even "seen" is recorded, and nothing is swept: a caller that only read
/// there is still pending once the store is writable again, and a record past its retention is
/// still there.
pub fn nothing_is_recorded_or_swept_on_a_newer_store(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    h.set_store_version(COMMS_STORE_VER + 1);
    read_as(h, "b", peek()).expect("inbox");
    read_as(h, "b", by_id(&created.id)).expect("inbox");
    list(h, "b");
    h.advance(MESSAGE_TTL + DAY);
    h.sweep();
    assert_eq!(bodies(&read_as(h, "b", peek()).expect("inbox")), ["first"]);
    h.rewind(MESSAGE_TTL + DAY);

    h.set_store_version(COMMS_STORE_VER);
    assert_eq!(start(h, "a", "again", &["b"]).pending, agents(h, &["b"]));
    read(h, "b");
    assert_eq!(start(h, "a", "once more", &["b"]).pending, Vec::<Actor>::new());
}
