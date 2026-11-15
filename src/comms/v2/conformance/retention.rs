//! Retention and the store version: what expires, what is dropped, what a newer store refuses.

use std::time::Duration;

use super::super::api::{
    Actor, COMMS_STORE_VER, CommsError, InboxBody, MAX_MESSAGES_PER_THREAD, MESSAGE_TTL, ThreadAction, ThreadFilter,
    ThreadId,
};
use super::{
    ConformanceHarness, agent, agents, bodies, by_id, change, find, history, ids, list, notify, only, peek, read,
    read_as, say, start, thread_ids, who,
};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn no_threads() -> Vec<ThreadId> {
    Vec::new()
}

/// Past the per-thread cap the oldest message is dropped, and the unread count is of what is
/// still stored.
pub fn oldest_messages_are_dropped_past_the_per_thread_cap(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    let second = say(h, "a", &created.thread, "more").expect("post");
    for _ in 1..MAX_MESSAGES_PER_THREAD {
        say(h, "a", &created.thread, "more").expect("post");
    }

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

/// A thread lives as long as its newest message and is removed with it.
pub fn thread_is_removed_when_its_last_message_expires(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);

    h.advance(MESSAGE_TTL - Duration::from_secs(1));
    assert_eq!(thread_ids(&list(h, "a")), std::slice::from_ref(&created.thread));

    h.advance(Duration::from_secs(2));
    assert_eq!(thread_ids(&list(h, "a")), no_threads());
    assert_eq!(h.thread_count(), 0);
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

/// Messages expire one by one: an old message goes while its thread and its newer messages stay.
pub fn messages_expire_one_by_one_inside_a_live_thread(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    h.advance(20 * DAY);
    let second = say(h, "a", &created.thread, "second").expect("post");
    h.advance(15 * DAY);

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

/// Archiving does not extend or shorten retention.
pub fn archived_thread_expires_like_any_other(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    change(h, &who(h, "a"), &created.thread, ThreadAction::Archive).expect("archive");
    let archived = ThreadFilter {
        archived: true,
        ..ThreadFilter::default()
    };

    h.advance(MESSAGE_TTL - Duration::from_secs(1));
    assert_eq!(
        thread_ids(&find(h, &who(h, "b"), archived.clone()).expect("threads")),
        [created.thread]
    );

    h.advance(Duration::from_secs(2));
    assert_eq!(
        thread_ids(&find(h, &who(h, "b"), archived).expect("threads")),
        no_threads()
    );
    assert_eq!(h.thread_count(), 0);
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

/// On a newer store not even "seen" is recorded: a caller that only read there is still pending
/// once the store is writable again.
pub fn nothing_is_recorded_on_a_newer_store(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    h.set_store_version(COMMS_STORE_VER + 1);
    read_as(h, "b", peek()).expect("inbox");
    read_as(h, "b", by_id(&created.id)).expect("inbox");
    list(h, "b");

    h.set_store_version(COMMS_STORE_VER);
    assert_eq!(start(h, "a", "again", &["b"]).pending, agents(h, &["b"]));
    read(h, "b");
    assert_eq!(start(h, "a", "once more", &["b"]).pending, Vec::<Actor>::new());
}
