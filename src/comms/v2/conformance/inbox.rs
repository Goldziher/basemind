//! Reading: positions, the three ways to not mark, budget, limit, cursors, replay and the wait.

use std::cell::Cell;
use std::time::Duration;

use super::super::api::{
    CommsError, Cursor, InboxBody, InboxMessage, MAX_INLINE_BODY_BYTES, MAX_LIMIT, MessageId, REPLAY_TTL,
    RESPONSE_BUDGET_BYTES, ReplayKey, ThreadFilter, ThreadQuery, WaitOutcome, message_cost,
};
use super::{
    ConformanceHarness, agent, bodies, by_id, gone_thread, history, ids, list, new_thread, notify, only, peek, read,
    read_as, say, start, tick, wait_then_read, who,
};

/// Long enough that a wait meant to end by an arrival never ends by its timeout instead.
const GENEROUS_WAIT: Duration = Duration::from_secs(30);

/// Short enough that waiting it out costs a test nothing.
const SHORT_WAIT: Duration = Duration::from_millis(30);

fn no_messages() -> Vec<InboxMessage> {
    Vec::new()
}

fn no_ids() -> Vec<MessageId> {
    Vec::new()
}

/// `to` may name an agent that has never connected; it finds the thread on its first read.
pub fn recipient_that_never_connected_finds_the_thread_on_its_first_inbox(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);

    let reply = read(h, "b");

    assert_eq!(reply.messages.len(), 1);
    let message = &reply.messages[0];
    assert_eq!(message.id, created.id);
    assert_eq!(message.subject, "topic");
    assert_eq!(message.from, agent(h, "a"));
    assert_eq!(message.reply_to, None);
    assert_eq!(message.body, "first");
    assert_eq!(message.body_bytes, 5);
    assert!(!message.truncated);
    assert_eq!(reply.read_to, [created.id]);
    assert!(!reply.more);
    assert_eq!(reply.next_cursor, None);
    assert_eq!(reply.notices, Vec::<String>::new());
    assert_eq!(reply.wait, WaitOutcome::NotWaited);
}

/// Reading is the acknowledgement: `read_seq` moves to the last message returned.
pub fn reading_advances_read_seq_to_the_last_message_returned(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    tick(h);
    let second = say(h, "a", &created.thread, "second").expect("post");

    let reply = read(h, "b");
    assert_eq!(bodies(&reply), ["first", "second"]);
    assert!(reply.messages[0].ts_micros < reply.messages[1].ts_micros);
    assert_eq!(reply.read_to, [second.id]);
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((2, 2)));

    let again = read(h, "b");
    assert_eq!(again.messages, no_messages());
    assert_eq!(again.read_to, no_ids());
}

/// `peek` shows what is unread and moves nothing, however often it is repeated.
pub fn peek_returns_messages_without_moving_any_position(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);

    let first = read_as(h, "b", peek()).expect("inbox");
    let second = read_as(h, "b", peek()).expect("inbox");

    assert_eq!(ids(&first), [created.id]);
    assert_eq!(first.read_to, no_ids());
    assert_eq!(second, first);
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((0, 0)));
}

/// The hooks' read returns front matter only, for what the actor has not been told about, moves
/// only `notified_seq` and leaves the messages unread for the tool.
pub fn notify_announces_front_matter_once_and_leaves_messages_unread(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    say(h, "a", &created.thread, "second").expect("post");
    let one = |body: InboxBody| InboxBody { limit: Some(1), ..body };

    let announced = read_as(h, "b", one(notify())).expect("inbox");
    assert_eq!(ids(&announced), std::slice::from_ref(&created.id));
    let message = &announced.messages[0];
    assert_eq!((message.body.as_str(), message.body_bytes), ("", 5));
    assert_eq!((message.subject.as_str(), &message.from), ("topic", &agent(h, "a")));
    assert!(!message.truncated);
    assert!(announced.more);
    assert_eq!(announced.read_to, no_ids());
    assert_eq!(announced.next_cursor, None);
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((0, 1)));

    assert_eq!(bodies(&read_as(h, "b", notify()).expect("inbox")), [""]);
    assert_eq!(read_as(h, "b", notify()).expect("inbox").messages, no_messages());
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((0, 2)));
    assert_eq!(bodies(&read(h, "b")), ["first", "second"]);
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((2, 2)));
    assert_eq!(
        read_as(
            h,
            "b",
            InboxBody {
                unread: false,
                ..notify()
            }
        )
        .map(|reply| reply.messages),
        Err(CommsError::not_applicable("unread", "notify"))
    );
}

/// An agent's own posts are not unread to it, in the inbox or in the count.
pub fn own_posts_are_never_unread(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);

    assert_eq!(read(h, "a").messages, no_messages());
    assert_eq!(h.position(&created.thread, &agent(h, "a")), Some((0, 0)));
    assert_eq!(list(h, "a").threads[0].unread, 0);
}

/// A post by a sibling actor of the same principal is unread like anyone else's, so a parent
/// and a subagent that passes `as` can talk; each one's own posts stay read for it.
pub fn posts_of_a_sibling_actor_are_unread(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    say(h, "b", &created.thread, "from the parent").expect("post");
    say(h, "b.helper", &created.thread, "from the helper").expect("post");

    assert_eq!(list(h, "b").threads[0].unread, 2);
    assert_eq!(bodies(&read(h, "b")), ["first", "from the helper"]);
    assert_eq!(bodies(&read(h, "b.helper")), Vec::<&str>::new());
    say(h, "b", &created.thread, "again").expect("post");
    assert_eq!(bodies(&read(h, "b.helper")), ["again"]);
}

/// An actor without marks in a thread starts at the highest read position any actor of its
/// principal has reached there. Once it has read, its marks are its own.
pub fn new_actor_starts_where_its_principal_has_read_to(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    say(h, "a", &created.thread, "second").expect("post");
    let (b, helper) = (agent(h, "b"), agent(h, "b.helper"));
    assert_eq!(h.position(&created.thread, &helper), Some((0, 0)));

    read(h, "b");
    assert_eq!(h.position(&created.thread, &helper), Some((2, 2)));
    assert_eq!(read(h, "b.helper").messages, no_messages());
    say(h, "a", &created.thread, "third").expect("post");
    assert_eq!(bodies(&read(h, "b")), ["third"]);

    assert_eq!(h.position(&created.thread, &helper), Some((2, 2)), "its own marks now");
    assert_eq!(bodies(&read(h, "b.helper")), ["third"]);
    assert_eq!(h.position(&created.thread, &b), Some((3, 3)));
}

/// `unread: false` returns read, unread and own messages, oldest first, and moves neither
/// `read_seq` nor `notified_seq`.
pub fn history_read_returns_everything_and_moves_no_position(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    say(h, "b", &created.thread, "second").expect("post");
    say(h, "a", &created.thread, "third").expect("post");

    let reply = read_as(h, "b", history()).expect("inbox");

    assert_eq!(bodies(&reply), ["first", "second", "third"]);
    assert_eq!(reply.read_to, no_ids());
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((0, 0)));

    let reply = read_as(
        h,
        "b",
        InboxBody {
            unread: false,
            ..peek()
        },
    )
    .expect("inbox");
    assert_eq!(bodies(&reply), ["first", "second", "third"]);
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((0, 0)));
    assert_eq!(bodies(&read(h, "b")), ["first", "third"]);
}

/// Across threads, messages come in the order they were posted to the store.
pub fn inbox_spans_threads_in_post_order(h: &impl ConformanceHarness) {
    let one = start(h, "a", "one", &["b"]);
    let two = start(h, "a", "two", &["b"]);
    let third = say(h, "a", &one.thread, "third").expect("post");

    let reply = read(h, "b");

    assert_eq!(ids(&reply), [one.id, two.id.clone(), third.id.clone()]);
    let mut read_to = reply.read_to.clone();
    read_to.sort();
    let mut expected = vec![third.id, two.id];
    expected.sort();
    assert_eq!(read_to, expected);
    assert_eq!(h.position(&two.thread, &agent(h, "b")), Some((1, 1)));
    assert_eq!(h.position(&one.thread, &agent(h, "b")), Some((2, 2)));
}

/// `thread` limits the read, and what it marks, to that thread.
pub fn thread_selection_reads_one_thread_only(h: &impl ConformanceHarness) {
    let one = start(h, "a", "one", &["b"]);
    let two = start(h, "a", "two", &["b"]);

    assert_eq!(ids(&read_as(h, "b", only(&two.thread)).expect("inbox")), [two.id]);
    assert_eq!(h.position(&one.thread, &agent(h, "b")), Some((0, 0)));
}

/// Reading a thread, or one of its messages by id, requires membership.
pub fn reading_requires_membership(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &[]);
    let not_member = CommsError::NotMember {
        thread: created.thread.clone(),
    };

    assert_eq!(
        read_as(h, "c", only(&created.thread)).map(|reply| reply.messages),
        Err(not_member.clone())
    );
    assert_eq!(
        read_as(h, "c", by_id(&created.id)).map(|reply| reply.messages),
        Err(not_member)
    );
    assert_eq!(read(h, "c").messages, no_messages());
}

/// A thread the store does not hold is named in the refusal, in every read mode.
pub fn inbox_on_an_unknown_thread_is_refused(h: &impl ConformanceHarness) {
    let gone = gone_thread(h);
    let not_found = CommsError::ThreadNotFound {
        thread: gone.thread.clone(),
    };

    for body in [
        only(&gone.thread),
        InboxBody {
            unread: false,
            ..only(&gone.thread)
        },
    ] {
        assert_eq!(
            read_as(h, "ghost", body).map(|reply| reply.messages),
            Err(not_found.clone())
        );
    }
}

/// A body over the inline limit is cut and flagged, and still counts as read.
pub fn long_body_is_returned_cut_and_counts_as_read(h: &impl ConformanceHarness) {
    let long = "x".repeat(MAX_INLINE_BODY_BYTES + 900);
    let created = h
        .post(&who(h, "a"), new_thread(h, "topic", &["b"], &[], &long))
        .expect("thread created");

    let reply = read(h, "b");

    assert_eq!(reply.messages[0].body, "x".repeat(MAX_INLINE_BODY_BYTES));
    assert!(reply.messages[0].truncated);
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((1, 1)));
}

/// A read by id is the way to the whole body, and it moves nothing.
pub fn reading_by_id_returns_the_whole_body_and_moves_no_position(h: &impl ConformanceHarness) {
    let long = "x".repeat(MAX_INLINE_BODY_BYTES + 900);
    let created = h
        .post(&who(h, "a"), new_thread(h, "topic", &["b"], &[], &long))
        .expect("thread created");

    let reply = read_as(h, "b", by_id(&created.id)).expect("inbox");

    assert_eq!(reply.messages[0].body, long);
    assert!(!reply.messages[0].truncated);
    assert_eq!(reply.read_to, no_ids());
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((0, 0)));
}

/// Beside `id`, a field that would change a list read is refused rather than ignored.
pub fn reading_by_id_refuses_fields_that_do_not_apply(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    let refused = |field: &str, body: InboxBody| {
        assert_eq!(
            read_as(h, "b", body).map(|reply| reply.messages),
            Err(CommsError::not_applicable(field, "id")),
            "{field}"
        );
    };

    refused(
        "limit",
        InboxBody {
            limit: Some(1),
            ..by_id(&created.id)
        },
    );
    refused(
        "unread",
        InboxBody {
            unread: false,
            ..by_id(&created.id)
        },
    );
    refused(
        "peek",
        InboxBody {
            select: by_id(&created.id).select,
            ..peek()
        },
    );
    refused(
        "cursor",
        InboxBody {
            cursor: Some(Cursor::from_wire("anything")),
            ..by_id(&created.id)
        },
    );
}

/// An id that names no stored message is refused, whether its thread exists or not.
pub fn reading_an_unknown_message_id_is_refused(h: &impl ConformanceHarness) {
    let gone = gone_thread(h);
    let created = start(h, "a", "topic", &["b"]);
    let beyond = MessageId::new(created.thread, 2).expect("message id");

    for missing in [beyond, gone.id] {
        assert_eq!(
            read_as(h, "a", by_id(&missing)).map(|reply| reply.messages),
            Err(CommsError::MessageNotFound { id: missing })
        );
    }
}

/// The 16 KB budget counts body, subject, sender and id. Delivery stops at the first message
/// that does not fit, even when a later one would, and what is left out stays unread.
pub fn response_budget_counts_more_than_the_body_and_never_skips(h: &impl ConformanceHarness) {
    let full = "x".repeat(MAX_INLINE_BODY_BYTES);
    assert_eq!(4 * MAX_INLINE_BODY_BYTES, RESPONSE_BUDGET_BYTES);
    let created = h
        .post(&who(h, "a"), new_thread(h, "topic", &["b"], &[], &full))
        .expect("thread created");
    for _ in 0..3 {
        say(h, "a", &created.thread, &full).expect("post");
    }
    say(h, "a", &created.thread, "tiny").expect("post");

    let first = read(h, "b");
    let spent: usize = first.messages.iter().map(message_cost).sum();
    assert_eq!(first.messages.len(), 3, "four bodies alone are exactly the budget");
    assert!(spent <= RESPONSE_BUDGET_BYTES);
    assert!(first.more);
    assert_eq!(first.next_cursor, None);
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((3, 3)));

    let second = read(h, "b");
    assert_eq!(second.messages.len(), 2);
    assert!(spent + message_cost(&second.messages[0]) > RESPONSE_BUDGET_BYTES);
    assert_eq!(second.messages[1].body, "tiny");
    assert!(!second.more);
}

/// `limit` cuts the page. After a read that marks, the rest is simply still unread.
pub fn limit_cuts_the_page_and_the_rest_stays_unread(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    say(h, "a", &created.thread, "second").expect("post");
    say(h, "a", &created.thread, "third").expect("post");

    let first = read_as(
        h,
        "b",
        InboxBody {
            limit: Some(2),
            ..InboxBody::default()
        },
    )
    .expect("inbox");

    assert_eq!(bodies(&first), ["first", "second"]);
    assert!(first.more);
    assert_eq!(first.next_cursor, None);
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((2, 2)));
    let rest = read(h, "b");
    assert_eq!(bodies(&rest), ["third"]);
    assert!(!rest.more);
}

/// A `limit` of 0 means 1, and one above [`MAX_LIMIT`] means [`MAX_LIMIT`].
pub fn limit_is_clamped_to_one_and_to_the_maximum(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    for _ in 0..MAX_LIMIT {
        say(h, "a", &created.thread, "more").expect("post");
    }
    let limited = |limit: u32| {
        read_as(
            h,
            "b",
            InboxBody {
                limit: Some(limit),
                ..peek()
            },
        )
        .expect("inbox")
    };

    let none = limited(0);
    assert_eq!(none.messages.len(), 1);
    assert!(none.more);

    let all = limited(MAX_LIMIT + 900);
    assert_eq!(all.messages.len(), MAX_LIMIT as usize);
    assert!(all.more);
}

/// The read position is the cursor of a read that marks; passing one is refused, naming it.
pub fn read_that_marks_takes_no_cursor(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    say(h, "a", &created.thread, "second").expect("post");
    let page = read_as(
        h,
        "b",
        InboxBody {
            limit: Some(1),
            ..peek()
        },
    )
    .expect("inbox");
    let cursor = page.next_cursor.expect("a peek that left a message out has a cursor");

    for body in [InboxBody::default(), notify()] {
        let refused = read_as(
            h,
            "b",
            InboxBody {
                cursor: Some(cursor.clone()),
                ..body
            },
        )
        .expect_err("a cursor on a read that marks");
        assert_eq!(refused, CommsError::CursorNotApplicable);
        assert!(refused.to_string().contains("`cursor`"), "{refused}");
    }
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((0, 0)));
}

/// A peek pages with its cursor and still marks nothing.
pub fn cursor_pages_a_peek_without_marking(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    say(h, "a", &created.thread, "second").expect("post");
    let page = |cursor: Option<Cursor>| {
        read_as(
            h,
            "b",
            InboxBody {
                limit: Some(1),
                cursor,
                ..peek()
            },
        )
        .expect("inbox")
    };

    let first = page(None);
    assert_eq!(bodies(&first), ["first"]);
    assert!(first.more);
    let second = page(first.next_cursor.clone());
    assert_eq!(bodies(&second), ["second"]);
    assert!(!second.more);
    assert_eq!(second.next_cursor, None);
    assert_eq!(page(first.next_cursor), second, "a cursor can be used again");
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((0, 0)));
}

/// History pages across threads in post order, returning each message once.
pub fn cursor_pages_history_across_threads(h: &impl ConformanceHarness) {
    let one = start(h, "a", "one", &["b"]);
    let two = start(h, "a", "two", &["b"]);
    let mine = say(h, "b", &one.thread, "mine").expect("post");
    let last = say(h, "a", &two.thread, "last").expect("post");

    let mut seen = Vec::new();
    let mut cursor = None;
    for _ in 0..4 {
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
        seen.extend(ids(&page));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    assert_eq!(seen, [one.id, two.id, mine.id, last.id]);
    assert_eq!(cursor, None);
    assert_eq!(h.position(&one.thread, &agent(h, "b")), Some((0, 0)));
}

/// A cursor continues only the read that issued it: same agent, same selection, same mode.
pub fn cursor_is_bound_to_actor_selection_and_mode(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b", "c"]);
    say(h, "a", &created.thread, "second").expect("post");
    let first_page = InboxBody {
        limit: Some(1),
        ..peek()
    };
    let cursor = read_as(h, "b", first_page.clone())
        .expect("inbox")
        .next_cursor
        .expect("a cursor");
    let with_cursor = |body: InboxBody, cursor: &Cursor| InboxBody {
        cursor: Some(cursor.clone()),
        ..body
    };
    let refused = |name: &str, body: InboxBody| {
        let error = read_as(h, name, body).expect_err("a cursor of another read");
        assert!(matches!(error, CommsError::InvalidCursor { .. }), "{error:?}");
        assert!(error.to_string().contains("without `cursor`"), "{error}");
    };

    refused("c", with_cursor(first_page.clone(), &cursor));
    refused(
        "b",
        with_cursor(
            InboxBody {
                select: only(&created.thread).select,
                ..first_page.clone()
            },
            &cursor,
        ),
    );
    refused("b", with_cursor(history(), &cursor));
    refused("b", with_cursor(first_page.clone(), &Cursor::from_wire("not-a-cursor")));
    refused(
        "b",
        with_cursor(first_page.clone(), &Cursor::from_wire("x".repeat(5000))),
    );

    let listing_cursor = {
        start(h, "a", "another", &["b"]);
        let listed = h
            .threads_query(
                &who(h, "b"),
                ThreadQuery::List(ThreadFilter {
                    limit: Some(1),
                    ..ThreadFilter::default()
                }),
            )
            .expect("threads");
        listed.next_cursor.expect("a listing cursor")
    };
    refused("b", with_cursor(first_page.clone(), &listing_cursor));

    assert_eq!(
        bodies(&read_as(h, "b", with_cursor(first_page, &cursor)).expect("inbox")),
        ["second"]
    );
}

/// A replayed read returns the range it first delivered instead of an empty page, and a later
/// message does not replace that range.
pub fn replayed_inbox_read_returns_the_range_first_delivered(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    let keyed = who(h, "b").with_replay_key(ReplayKey::parse("relay-1").expect("replay key"));

    let first = h.inbox(&keyed, InboxBody::default()).expect("inbox");
    assert_eq!(ids(&first), [created.id]);
    assert_eq!(h.inbox(&keyed, InboxBody::default()).expect("inbox"), first);

    let second = say(h, "a", &created.thread, "second").expect("post");
    assert_eq!(h.inbox(&keyed, InboxBody::default()).expect("inbox"), first);
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((1, 1)));
    assert_eq!(ids(&read(h, "b")), [second.id]);
    assert_eq!(h.inbox(&keyed, InboxBody::default()).expect("inbox"), first);

    h.advance(REPLAY_TTL + Duration::from_secs(1));
    h.sweep();
    assert_eq!(
        h.inbox(&keyed, InboxBody::default()).expect("inbox").messages,
        no_messages()
    );
}

/// A read that delivered nothing is not remembered: the same key reads afresh, so a waiter that
/// looks again under its key gets the message that arrived.
pub fn replay_key_of_an_empty_read_is_not_remembered(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    read(h, "b");
    let keyed = who(h, "b").with_replay_key(ReplayKey::parse("relay-1").expect("replay key"));

    assert_eq!(
        h.inbox(&keyed, InboxBody::default()).expect("inbox").messages,
        no_messages()
    );
    let second = say(h, "a", &created.thread, "second").expect("post");

    assert_eq!(ids(&h.inbox(&keyed, InboxBody::default()).expect("inbox")), [second.id]);
}

/// A replay key belongs to the agent that used it: the same key from another agent is its own
/// read.
pub fn inbox_replay_key_is_scoped_to_its_agent(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b", "c"]);
    let key = ReplayKey::parse("relay-1").expect("replay key");
    let (b, c) = (
        who(h, "b").with_replay_key(key.clone()),
        who(h, "c").with_replay_key(key),
    );

    h.inbox(&b, InboxBody::default()).expect("inbox");
    let second = say(h, "a", &created.thread, "second").expect("post");

    assert_eq!(
        ids(&h.inbox(&c, InboxBody::default()).expect("inbox")),
        [created.id.clone(), second.id]
    );
    assert_eq!(ids(&h.inbox(&b, InboxBody::default()).expect("inbox")), [created.id]);
}

/// A page cut by `limit` in the middle of several threads advances each thread only as far as
/// its own messages were returned, for a read that marks and for the hooks' read alike.
pub fn limit_across_threads_never_skips_an_unread_message(h: &impl ConformanceHarness) {
    let one = start(h, "a", "one", &["b"]);
    let two = start(h, "a", "two", &["b"]);
    let third = say(h, "a", &one.thread, "third").expect("post");
    let fourth = say(h, "a", &two.thread, "fourth").expect("post");
    let b = agent(h, "b");
    let two_at_most = |body: InboxBody| InboxBody { limit: Some(2), ..body };

    let announced = read_as(h, "b", two_at_most(notify())).expect("inbox");
    assert_eq!(ids(&announced), [one.id.clone(), two.id.clone()]);
    assert!(announced.more);
    assert_eq!(h.position(&one.thread, &b), Some((0, 1)));
    assert_eq!(h.position(&two.thread, &b), Some((0, 1)));
    let rest = read_as(h, "b", two_at_most(notify())).expect("inbox");
    assert_eq!(ids(&rest), [third.id.clone(), fourth.id.clone()]);

    let first = read_as(h, "b", two_at_most(InboxBody::default())).expect("inbox");
    assert_eq!(ids(&first), [one.id, two.id]);
    assert!(first.more);
    assert_eq!(h.position(&one.thread, &b).map(|(read_seq, _)| read_seq), Some(1));
    assert_eq!(h.position(&two.thread, &b).map(|(read_seq, _)| read_seq), Some(1));
    let second = read_as(h, "b", two_at_most(InboxBody::default())).expect("inbox");
    assert_eq!(ids(&second), [third.id, fourth.id]);
    assert!(!second.more);
}

/// A waiter that re-reads when the arrivals counter changes gets the message that arrived.
pub fn wait_returns_when_a_message_arrives(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    read(h, "b");

    let reply = wait_then_read(h, &who(h, "b"), InboxBody::default(), GENEROUS_WAIT, || {
        say(h, "a", &created.thread, "ping").expect("post");
    })
    .expect("inbox");

    assert_eq!(reply.wait, WaitOutcome::Arrived);
    assert_eq!(bodies(&reply), ["ping"]);
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((2, 2)));
}

/// A wait that nothing ends returns empty and moves nothing.
pub fn wait_times_out_when_nothing_arrives(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    read(h, "b");

    let reply = wait_then_read(h, &who(h, "b"), InboxBody::default(), SHORT_WAIT, || {}).expect("inbox");

    assert_eq!(reply.wait, WaitOutcome::TimedOut);
    assert_eq!(reply.messages, no_messages());
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((1, 1)));
}

/// The caller's own post moves the arrivals counter but is not something to wake up for.
pub fn wait_ignores_the_callers_own_posts(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    read(h, "b");

    let reply = wait_then_read(h, &who(h, "b"), InboxBody::default(), SHORT_WAIT, || {
        say(h, "b", &created.thread, "mine").expect("post");
    })
    .expect("inbox");

    assert_eq!(reply.wait, WaitOutcome::TimedOut);
    assert_eq!(reply.messages, no_messages());
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((1, 1)));
}

/// With something unread there is nothing to wait for.
pub fn wait_is_skipped_when_something_is_unread(h: &impl ConformanceHarness) {
    start(h, "a", "topic", &["b"]);
    let waited = Cell::new(false);

    let reply = wait_then_read(h, &who(h, "b"), InboxBody::default(), GENEROUS_WAIT, || {
        waited.set(true)
    })
    .expect("inbox");

    assert_eq!(reply.wait, WaitOutcome::NotWaited);
    assert_eq!(bodies(&reply), ["first"]);
    assert!(!waited.get());
}
