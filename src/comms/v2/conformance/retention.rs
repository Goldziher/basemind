//! Retention and the store version: what expires, what is dropped, what a newer store refuses.

use std::time::Duration;

use super::super::api::{
    Actor, COMMS_STORE_VER, CommsError, InboxBody, MAX_ACTIVE_THREADS_PER_REPO, MAX_MESSAGES_PER_THREAD, MESSAGE_TTL,
    MessageId, REPLAY_TTL, ReplayKey, ThreadAction, ThreadFilter, ThreadId,
};
use super::{
    ConformanceHarness, agent, agents, bodies, by_id, change, describe, find, history, ids, into, list, new_thread,
    notify, only, peek, read, read_as, say, start, thread_ids, who,
};

const SECOND: Duration = Duration::from_secs(1);

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// How far ahead a wrong clock reads in the scenarios about one: five years.
const AHEAD: Duration = Duration::from_secs(5 * 365 * 24 * 60 * 60);

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

/// What has expired is not returned by a newer store either, although nothing is removed from
/// it: expiry is a matter of the clock, not of a sweep.
pub fn expired_messages_are_not_returned_on_a_newer_store(h: &impl ConformanceHarness) {
    let old = start(h, "a", "old", &["b"]);
    h.advance(20 * DAY);
    let second = say(h, "a", &old.thread, "second").expect("post");
    let young = start(h, "a", "young", &["b"]);
    h.set_store_version(COMMS_STORE_VER + 1);

    h.advance(15 * DAY);
    assert_eq!(
        ids(&read_as(h, "b", peek()).expect("inbox")),
        [second.id.clone(), young.id.clone()]
    );
    assert_eq!(
        ids(&read_as(h, "b", history()).expect("inbox")),
        [second.id.clone(), young.id.clone()]
    );
    assert_eq!(
        read_as(h, "b", by_id(&old.id)).map(|reply| reply.messages),
        Err(CommsError::MessageNotFound { id: old.id.clone() })
    );
    let summary = describe(h, &who(h, "b"), &old.thread).expect("the thread is still there");
    assert_eq!((summary.head, summary.unread), (2, 1));

    h.advance(16 * DAY);
    assert_eq!(read_as(h, "b", peek()).expect("inbox").messages, Vec::new());
    assert_eq!(read_as(h, "b", history()).expect("inbox").messages, Vec::new());
    assert_eq!(thread_ids(&list(h, "b")), no_threads());
    assert_eq!(
        read_as(h, "b", by_id(&second.id)).map(|reply| reply.messages),
        Err(CommsError::MessageNotFound { id: second.id })
    );
    assert_eq!(
        describe(h, &who(h, "b"), &young.thread),
        Err(CommsError::ThreadNotFound {
            thread: young.thread.clone()
        })
    );
    let peek_one = InboxBody {
        select: only(&young.thread).select,
        ..peek()
    };
    assert_eq!(
        read_as(h, "b", peek_one).map(|reply| reply.messages),
        Err(CommsError::ThreadNotFound { thread: young.thread })
    );
}

/// A replay entry written after the clock was set back expires an hour later by the clock, not
/// by the stamp of a post made while it ran ahead.
pub fn replay_entries_expire_by_the_clock_after_it_was_set_back(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    h.advance(2 * DAY);
    say(h, "a", &created.thread, "stamped ahead").expect("post");
    h.rewind(2 * DAY);

    let relayed = who(h, "a").with_replay_key(ReplayKey::parse("relay-1").expect("replay key"));
    let first = h
        .post(&relayed, into(&created.thread, "after the correction"))
        .expect("post");
    assert_eq!(
        h.post(&relayed, into(&created.thread, "after the correction")),
        Ok(first.clone())
    );
    h.advance(REPLAY_TTL + SECOND);
    let again = h
        .post(&relayed, into(&created.thread, "after the correction"))
        .expect("post");
    assert_eq!(
        again.id.seq(),
        first.id.seq() + 1,
        "the replay entry expired by the clock"
    );
}

/// A post made while the clock ran far ahead is stamped ahead. Once the clock is corrected the
/// next post is stamped from the clock again and expires [`MESSAGE_TTL`] after the corrected
/// time, alone: its thread keeps the last activity it was given ahead, is listed first, and
/// keeps the message stamped ahead.
pub fn post_after_the_clock_was_corrected_is_stamped_and_expires_by_the_clock(h: &impl ConformanceHarness) {
    h.advance(AHEAD);
    let ahead = start(h, "a", "made ahead", &["b"]);
    h.rewind(AHEAD);
    let after = say(h, "a", &ahead.thread, "after the correction").expect("post");
    let later = start(h, "a", "later", &["b"]);

    let whole = read_as(h, "b", history()).expect("inbox");
    assert_eq!(ids(&whole), [ahead.id.clone(), after.id.clone(), later.id.clone()]);
    let stamps: Vec<i64> = whole.messages.iter().map(|message| message.ts_micros).collect();
    assert!(
        stamps[1] < stamps[2] && stamps[2] < stamps[0],
        "the posts after the correction are stamped from the clock: {stamps:?}"
    );
    let summary = describe(h, &who(h, "b"), &ahead.thread).expect("thread");
    assert_eq!(
        summary.last_activity_micros, stamps[0],
        "last activity does not move back"
    );
    assert_eq!(
        thread_ids(&list(h, "b")),
        [ahead.thread.clone(), later.thread.clone()],
        "the thread posted to while the clock ran ahead is listed first"
    );

    h.advance(MESSAGE_TTL - SECOND);
    assert_eq!(read_as(h, "b", history()).expect("inbox").messages.len(), 3);
    assert_eq!(list(h, "b").threads.len(), 2);

    h.advance(2 * SECOND);
    assert_eq!(
        ids(&read_as(h, "b", history()).expect("inbox")),
        std::slice::from_ref(&ahead.id)
    );
    assert_eq!(thread_ids(&list(h, "b")), std::slice::from_ref(&ahead.thread));
    assert_eq!(
        read_as(h, "b", by_id(&after.id)).map(|reply| reply.messages),
        Err(CommsError::MessageNotFound { id: after.id })
    );
    assert_eq!(h.thread_count(), 1);
    assert_eq!(bodies(&read_as(h, "b", by_id(&ahead.id)).expect("inbox")), ["first"]);
}

/// A post stamped while the clock read behind does not take its thread with it: the thread's
/// last activity stays where the earlier post put it, so once the clock is right again the
/// thread and that earlier message are still there, sweep or not, and only the message stamped
/// behind has expired.
pub fn post_stamped_while_the_clock_read_behind_expires_alone(h: &impl ConformanceHarness) {
    let behind = MESSAGE_TTL + DAY;
    let created = start(h, "a", "topic", &["b"]);
    h.rewind(behind);
    let stamped_behind = say(h, "a", &created.thread, "stamped behind").expect("post");
    let whole = read_as(h, "b", history()).expect("inbox");
    assert_eq!(ids(&whole), [created.id.clone(), stamped_behind.id.clone()]);
    assert!(
        whole.messages[1].ts_micros < whole.messages[0].ts_micros,
        "the second post is stamped from the clock, which reads behind"
    );
    h.advance(behind);

    h.sweep();

    assert_eq!(thread_ids(&list(h, "b")), std::slice::from_ref(&created.thread));
    let summary = describe(h, &who(h, "b"), &created.thread).expect("thread");
    assert_eq!((summary.head, summary.unread), (2, 1));
    assert_eq!(summary.last_activity_micros, whole.messages[0].ts_micros);
    assert_eq!(
        ids(&read_as(h, "b", history()).expect("inbox")),
        std::slice::from_ref(&created.id)
    );
    assert_eq!(bodies(&read_as(h, "b", by_id(&created.id)).expect("inbox")), ["first"]);
    assert_eq!(
        read_as(h, "b", by_id(&stamped_behind.id)).map(|reply| reply.messages),
        Err(CommsError::MessageNotFound { id: stamped_behind.id })
    );
    assert_eq!(h.thread_count(), 1);
    say(h, "b", &created.thread, "still open").expect("the thread takes posts");
}

/// A clock that read ahead for a while holds no cap slot for the threads nobody posted to in
/// that time: they expire [`MESSAGE_TTL`] after their own last post. The one posted to then
/// stays, with the last activity that post gave it.
pub fn thread_cap_is_not_held_by_threads_untouched_while_the_clock_read_ahead(h: &impl ConformanceHarness) {
    let glitch = 20 * DAY;
    let threads: Vec<ThreadId> = (0..MAX_ACTIVE_THREADS_PER_REPO)
        .map(|_| start(h, "a", "made before", &[]).thread)
        .collect();
    h.advance(glitch);
    say(h, "a", &threads[0], "while the clock read ahead").expect("post");
    h.rewind(glitch);
    let one_more = || h.post(&who(h, "a"), new_thread(h, "one more", &[], &[], "first"));
    assert_eq!(
        one_more(),
        Err(CommsError::ThreadCapReached {
            repo: who(h, "a").repo,
            cap: MAX_ACTIVE_THREADS_PER_REPO,
        })
    );

    h.advance(MESSAGE_TTL + SECOND);

    let fresh = one_more().expect("the untouched threads expired with their own last post");
    assert_eq!(thread_ids(&list(h, "a")), [fresh.thread, threads[0].clone()]);
    assert_eq!(h.thread_count(), 2);
}

/// A thread created or posted to while the clock ran far ahead holds its cap slot once the
/// clock is corrected, also after a post stamped from the corrected clock and that post's
/// retention. Archiving it frees the slot.
pub fn thread_touched_while_the_clock_read_ahead_holds_its_cap_slot_until_archived(h: &impl ConformanceHarness) {
    h.advance(AHEAD);
    let made_ahead: Vec<ThreadId> = (0..MAX_ACTIVE_THREADS_PER_REPO)
        .map(|_| start(h, "a", "made ahead", &[]).thread)
        .collect();
    h.rewind(AHEAD);
    let one_more = || h.post(&who(h, "a"), new_thread(h, "one more", &[], &[], "first"));
    let capped = CommsError::ThreadCapReached {
        repo: who(h, "a").repo,
        cap: MAX_ACTIVE_THREADS_PER_REPO,
    };
    assert_eq!(one_more(), Err(capped.clone()));

    say(h, "a", &made_ahead[0], "after the correction").expect("post");
    h.advance(MESSAGE_TTL + SECOND);
    assert_eq!(
        one_more(),
        Err(capped),
        "a post after the correction does not put the thread back on the clock"
    );
    assert_eq!(h.thread_count(), MAX_ACTIVE_THREADS_PER_REPO);

    change(h, &who(h, "a"), &made_ahead[0], ThreadAction::Archive).expect("archive");
    one_more().expect("archiving freed a place under the cap");
    assert_eq!(h.thread_count(), MAX_ACTIVE_THREADS_PER_REPO + 1);
}

/// Every message a paged read of `mode` returns to `b`, one message per page.
#[track_caller]
fn page_through(h: &impl ConformanceHarness, mode: fn() -> InboxBody, at_most: usize) -> Vec<MessageId> {
    let mut seen = Vec::new();
    let mut cursor = None;
    for _ in 0..at_most {
        let body = InboxBody {
            limit: Some(1),
            cursor: cursor.take(),
            ..mode()
        };
        let page = read_as(h, "b", body).expect("inbox");
        seen.extend(ids(&page));
        cursor = page.next_cursor;
        if cursor.is_none() {
            return seen;
        }
    }
    panic!("the read still pages after {at_most} pages: {seen:?}");
}

/// A cursor follows the order messages were posted in, whatever their stamps: it neither skips
/// nor repeats one across a message stamped ahead of the clock, nor around one a sweep removed
/// from the middle of a thread while an older message stamped ahead stays.
pub fn cursor_pages_across_a_message_stamped_ahead_and_a_swept_one(h: &impl ConformanceHarness) {
    h.advance(AHEAD);
    let before = start(h, "a", "before", &["b"]);
    let ahead = start(h, "a", "topic", &["b"]);
    h.rewind(AHEAD);
    let second = say(h, "a", &ahead.thread, "second").expect("post");
    h.advance(DAY);
    let other = start(h, "a", "other", &["b"]);
    let third = say(h, "a", &ahead.thread, "third").expect("post");

    let posted = [
        before.id.clone(),
        ahead.id.clone(),
        second.id,
        other.id.clone(),
        third.id.clone(),
    ];
    assert_eq!(page_through(h, history, 6), posted);
    assert_eq!(page_through(h, peek, 6), posted);

    h.advance(MESSAGE_TTL - DAY / 2);
    h.sweep();
    let kept = [before.id, ahead.id, other.id, third.id];
    assert_eq!(page_through(h, history, 5), kept);
    assert_eq!(page_through(h, peek, 5), kept);
}

/// What is hidden as expired is visible again when the clock goes back, as long as no sweep
/// removed it. Only refused calls are made while it is hidden, because a call that succeeds may
/// sweep (see [`ConformanceHarness::rewind`]).
pub fn expired_records_are_visible_again_when_the_clock_goes_back_before_a_sweep(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    let second = say(h, "a", &created.thread, "second").expect("post");
    let past = MESSAGE_TTL + SECOND;
    let gone = CommsError::ThreadNotFound {
        thread: created.thread.clone(),
    };

    h.advance(past);
    assert_eq!(describe(h, &who(h, "b"), &created.thread), Err(gone.clone()));
    assert_eq!(
        read_as(h, "b", by_id(&created.id)).map(|reply| reply.messages),
        Err(CommsError::MessageNotFound { id: created.id.clone() })
    );
    assert_eq!(say(h, "a", &created.thread, "late"), Err(gone));

    h.rewind(past);
    assert_eq!(thread_ids(&list(h, "b")), std::slice::from_ref(&created.thread));
    let summary = describe(h, &who(h, "b"), &created.thread).expect("thread");
    assert_eq!((summary.head, summary.unread), (2, 2));
    assert_eq!(ids(&read(h, "b")), [created.id, second.id]);
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((2, 2)));
}

/// What a sweep removed stays removed when the clock goes back.
pub fn swept_records_stay_removed_when_the_clock_goes_back(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    let past = MESSAGE_TTL + SECOND;

    h.advance(past);
    h.sweep();
    h.rewind(past);

    assert_eq!(thread_ids(&list(h, "b")), no_threads());
    assert_eq!(
        read_as(h, "b", by_id(&created.id)).map(|reply| reply.messages),
        Err(CommsError::MessageNotFound { id: created.id })
    );
    assert_eq!(
        say(h, "a", &created.thread, "late"),
        Err(CommsError::ThreadNotFound { thread: created.thread })
    );
    assert_eq!(h.thread_count(), 0);
}
