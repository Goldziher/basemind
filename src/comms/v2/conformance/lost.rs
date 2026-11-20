//! Telling a member it lost messages: to a thread the thread cap archived, and to a sweep.

use super::super::api::{
    CommsError, DEFAULT_LIMIT, InboxBody, MAX_ACTIVE_THREADS_PER_REPO, MAX_MESSAGES_PER_THREAD, MessageId, PostReply,
    ReplayKey, ThreadAction, cap_archived_member_notice, swept_notice,
};
use super::{ConformanceHarness, bodies, change, history, only, peek, read, read_as, say, start, who};

fn no_notices() -> Vec<String> {
    Vec::new()
}

/// A thread `a` started with `b`, holding one message `b` has not read.
#[track_caller]
fn unread_for_b<H: ConformanceHarness>(h: &H) -> PostReply {
    start(h, "a", "topic", &["b"])
}

/// A read over all threads leaves out a thread the thread cap archived, and tells each of its
/// members so once, with how many of its messages are unread. A peek tells without recording it,
/// a replayed read tells again, and a reply into the thread is refused with how to continue. A
/// thread its creator archived tells nobody.
pub fn members_of_a_thread_the_cap_archived_are_told_on_their_next_read(h: &impl ConformanceHarness) {
    let lost = unread_for_b(h);
    let closed = start(h, "a", "closed by its creator", &["b"]);
    change(h, &who(h, "a"), &closed.thread, ThreadAction::Archive).expect("archive");
    assert_eq!(read_as(h, "b", peek()).expect("inbox").notices, no_notices());

    for _ in 0..MAX_ACTIVE_THREADS_PER_REPO {
        start(h, "c", "busy", &[]);
    }
    let told = cap_archived_member_notice(&lost.thread, 1);

    for _ in 0..2 {
        let peeked = read_as(h, "b", peek()).expect("inbox");
        assert_eq!(bodies(&peeked), Vec::<&str>::new());
        assert_eq!(peeked.notices, std::slice::from_ref(&told), "a peek records nothing");
    }
    let keyed = who(h, "b").with_replay_key(ReplayKey::parse("relay-1").expect("replay key"));
    let first = h.inbox(&keyed, InboxBody::default()).expect("inbox");
    assert_eq!(bodies(&first), Vec::<&str>::new());
    assert_eq!(first.notices, std::slice::from_ref(&told));
    assert_eq!(h.inbox(&keyed, InboxBody::default()).expect("inbox"), first);
    assert_eq!(read(h, "b").notices, no_notices(), "told once");

    assert_eq!(
        read(h, "a").notices,
        [cap_archived_member_notice(&lost.thread, 0)],
        "a member with nothing unread is told too"
    );
    assert_eq!(bodies(&read_as(h, "b", only(&lost.thread)).expect("inbox")), ["first"]);
    assert_eq!(
        say(h, "b", &lost.thread, "late"),
        Err(CommsError::ThreadArchivedByCap {
            thread: lost.thread,
            cap: MAX_ACTIVE_THREADS_PER_REPO,
        })
    );
}

/// A sweep removes messages whatever a member has read. The member's next unread read says how
/// many of them it missed in that thread, and goes on saying so until a read takes it past them;
/// a history read, which starts at the oldest message kept, does not.
pub fn messages_a_sweep_removed_before_a_member_read_them_are_counted(h: &impl ConformanceHarness) {
    let thread = unread_for_b(h).thread;
    let over = 5;
    for _ in 1..MAX_MESSAGES_PER_THREAD + over {
        say(h, "a", &thread, "more").expect("post");
    }
    h.sweep();
    let missed = swept_notice(&thread, over);

    let peeked = read_as(h, "b", peek()).expect("inbox");
    assert_eq!(peeked.notices, std::slice::from_ref(&missed));
    assert_eq!(
        peeked.messages[0].id,
        MessageId::new(thread.clone(), over + 1).expect("id")
    );
    assert_eq!(read_as(h, "b", history()).expect("inbox").notices, no_notices());

    let first = read(h, "b");
    assert_eq!(first.notices, std::slice::from_ref(&missed));
    let last = over + u64::from(DEFAULT_LIMIT);
    assert_eq!(first.read_to, [MessageId::new(thread, last).expect("id")]);
    assert_eq!(read(h, "b").notices, no_notices(), "the read went past them");
}
