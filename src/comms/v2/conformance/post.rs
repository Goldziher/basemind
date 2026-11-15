//! Posting: thread creation, membership, "seen", limits and replay.

use std::time::Duration;

use super::super::api::{
    Actor, CommsError, InboxBody, MAX_RECIPIENTS, MAX_THREAD_PATHS, MessageId, PostBody, PostTarget, REPLAY_TTL,
    ReplayKey, SEEN_REFRESH_INTERVAL, SEEN_TTL, ThreadAction,
};
use super::{
    ConformanceHarness, agent, agents, bodies, by_id, change, gone_thread, ids, into, list, new_thread, notify, read,
    say, start, who,
};

fn key(text: &str) -> ReplayKey {
    ReplayKey::parse(text).expect("replay key")
}

fn reply_to(message: &MessageId, body: &str) -> PostBody {
    PostBody {
        target: PostTarget::ReplyTo(message.clone()),
        body: body.to_string(),
    }
}

/// The creator and every `to` recipient are members from creation, reading from sequence 0.
pub fn creating_post_puts_creator_and_recipients_at_position_zero(h: &impl ConformanceHarness) {
    let reply = start(h, "a", "topic", &["b"]);

    assert!(reply.created);
    assert_eq!(reply.id.thread(), &reply.thread);
    assert_eq!(reply.id.seq(), 1);
    assert_eq!(reply.notices, Vec::<String>::new());
    assert_eq!(h.position(&reply.thread, &agent(h, "a")), Some((0, 0)));
    assert_eq!(h.position(&reply.thread, &agent(h, "b")), Some((0, 0)));
    assert_eq!(h.position(&reply.thread, &agent(h, "c")), None);
}

/// A subject is a topic: posting it twice makes two threads.
pub fn subject_is_not_a_key_so_two_posts_make_two_threads(h: &impl ConformanceHarness) {
    let first = start(h, "a", "topic", &[]);
    let second = start(h, "a", "topic", &[]);

    assert_ne!(first.thread, second.thread);
    assert_eq!(h.thread_count(), 2);
}

/// `pending` names each recipient that has made no call once, and never the caller.
pub fn pending_lists_only_recipients_that_made_no_call(h: &impl ConformanceHarness) {
    read(h, "b");

    let reply = h
        .post(
            &who(h, "a"),
            new_thread(h, "topic", &["b", "typo", "typo", "a"], &[], "first"),
        )
        .expect("thread created");

    assert_eq!(reply.pending, agents(h, &["typo"]));
}

/// An agent is seen by calling, not by being addressed.
pub fn being_named_in_to_does_not_make_an_agent_seen(h: &impl ConformanceHarness) {
    assert_eq!(start(h, "a", "one", &["b"]).pending, agents(h, &["b"]));
    assert_eq!(start(h, "a", "two", &["b"]).pending, agents(h, &["b"]));

    let created = start(h, "a", "three", &[]);
    let invited = change(
        h,
        &who(h, "a"),
        &created.thread,
        ThreadAction::Invite(agents(h, &["c"])),
    )
    .expect("invite");
    assert_eq!(invited.pending, agents(h, &["c"]));
    let again = change(
        h,
        &who(h, "a"),
        &created.thread,
        ThreadAction::Invite(agents(h, &["c"])),
    )
    .expect("invite");
    assert_eq!(again.pending, agents(h, &["c"]));
}

/// Only a call that succeeds makes its caller seen. Any kind does, a hook's read included.
pub fn failed_call_does_not_make_an_agent_seen(h: &impl ConformanceHarness) {
    let gone = gone_thread(h);

    assert_eq!(
        say(h, "b", &gone.thread, "hi"),
        Err(CommsError::ThreadNotFound {
            thread: gone.thread.clone()
        })
    );
    assert_eq!(start(h, "a", "one", &["b"]).pending, agents(h, &["b"]));

    h.inbox(&who(h, "b"), notify()).expect("a hook's read");
    assert_eq!(start(h, "a", "two", &["b"]).pending, Vec::<Actor>::new());
}

/// An agent is seen for [`SEEN_TTL`] from its last call, whether or not that call rewrote its
/// record, and for at most [`SEEN_REFRESH_INTERVAL`] longer: a record is rewritten only once it
/// is that old, so that a call need not write, and is kept that much longer to make up for it.
pub fn seen_record_lasts_thirty_days_from_the_last_call_and_at_most_a_day_more(h: &impl ConformanceHarness) {
    let second = Duration::from_secs(1);
    read(h, "b");
    h.advance(SEEN_REFRESH_INTERVAL - second);
    read(h, "b");
    h.advance(SEEN_TTL - second);
    assert_eq!(start(h, "a", "one", &["b"]).pending, Vec::<Actor>::new());
    h.advance(2 * second);
    assert_eq!(start(h, "a", "two", &["b"]).pending, agents(h, &["b"]));

    read(h, "b");
    h.advance(SEEN_TTL + second);
    assert_eq!(start(h, "a", "three", &["b"]).pending, Vec::<Actor>::new());
    h.advance(SEEN_REFRESH_INTERVAL);
    assert_eq!(start(h, "a", "four", &["b"]).pending, agents(h, &["b"]));
}

/// The first successful call records an agent at once, and a record a day old is rewritten by
/// the next call, so an agent that keeps calling stays seen however long it does.
pub fn agent_that_keeps_calling_stays_seen(h: &impl ConformanceHarness) {
    read(h, "b");
    assert_eq!(start(h, "a", "one", &["b"]).pending, Vec::<Actor>::new());

    for _ in 0..3 {
        h.advance(SEEN_TTL / 2);
        read(h, "b");
    }
    h.advance(SEEN_TTL - Duration::from_secs(1));
    assert_eq!(start(h, "a", "two", &["b"]).pending, Vec::<Actor>::new());
}

/// Posting into a thread, directly or as a reply, requires membership.
pub fn posting_requires_membership(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &[]);
    let not_member = CommsError::NotMember {
        thread: created.thread.clone(),
    };

    assert_eq!(say(h, "c", &created.thread, "hi"), Err(not_member.clone()));
    assert_eq!(h.post(&who(h, "c"), reply_to(&created.id, "hi")), Err(not_member));
}

/// `a.helper` is not `a`: membership is per composed id.
pub fn sub_identity_is_a_different_member_than_its_principal(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);

    assert_eq!(
        say(h, "a.helper", &created.thread, "hi"),
        Err(CommsError::NotMember { thread: created.thread })
    );
}

/// A reply lands in the thread of the message it answers and records which one.
pub fn reply_goes_into_the_thread_of_the_message_it_answers(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);

    let reply = h.post(&who(h, "b"), reply_to(&created.id, "answer")).expect("post");

    assert_eq!(reply.thread, created.thread);
    assert_eq!(reply.id.seq(), 2);
    assert!(!reply.created);
    let inbox = read(h, "a");
    assert_eq!(bodies(&inbox), ["answer"]);
    assert_eq!(inbox.messages[0].reply_to, Some(created.id.clone()));

    let missing = MessageId::new(created.thread, 9).expect("message id");
    assert_eq!(
        h.post(&who(h, "b"), reply_to(&missing, "answer")),
        Err(CommsError::MessageNotFound { id: missing })
    );
}

/// A refused post leaves no thread behind.
pub fn invalid_posts_are_refused_before_anything_is_stored(h: &impl ConformanceHarness) {
    let a = who(h, "a");
    let before = h.arrivals();

    assert_eq!(
        h.post(&a, new_thread(h, "topic", &[], &[], "  ")),
        Err(CommsError::EmptyBody)
    );
    assert!(matches!(
        h.post(&a, new_thread(h, "two\nlines", &[], &[], "first")),
        Err(CommsError::InvalidSubject { .. })
    ));
    assert!(matches!(
        h.post(&a, new_thread(h, "topic", &[], &["../other"], "first")),
        Err(CommsError::InvalidPath { .. })
    ));
    assert_eq!(h.thread_count(), 0);
    assert_eq!(h.arrivals(), before);
}

/// `to`, `invite` and `paths` have a cap each, and one over is refused, not cut.
pub fn recipients_and_paths_are_capped(h: &impl ConformanceHarness) {
    let a = who(h, "a");
    let names: Vec<String> = (0..=MAX_RECIPIENTS).map(|n| format!("r{n}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let paths: Vec<String> = (0..=MAX_THREAD_PATHS).map(|n| format!("src/m{n}")).collect();
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();

    assert_eq!(
        h.post(&a, new_thread(h, "crowd", &names, &[], "first")),
        Err(CommsError::TooManyRecipients {
            field: "to".to_string(),
            given: MAX_RECIPIENTS + 1,
            max: MAX_RECIPIENTS,
        })
    );
    assert_eq!(
        h.post(&a, new_thread(h, "sprawl", &[], &paths, "first")),
        Err(CommsError::TooManyPaths {
            given: MAX_THREAD_PATHS + 1,
            max: MAX_THREAD_PATHS,
        })
    );
    assert_eq!(h.thread_count(), 0);

    let at_cap = h
        .post(
            &a,
            new_thread(h, "full", &names[..MAX_RECIPIENTS], &paths[..MAX_THREAD_PATHS], "first"),
        )
        .expect("a post at both caps");
    assert_eq!(
        change(h, &a, &at_cap.thread, ThreadAction::Invite(agents(h, &names))),
        Err(CommsError::TooManyRecipients {
            field: "invite".to_string(),
            given: MAX_RECIPIENTS + 1,
            max: MAX_RECIPIENTS,
        })
    );
}

/// A `to` of just the caller would make a thread nobody else can reach.
pub fn to_naming_only_the_creator_is_refused(h: &impl ConformanceHarness) {
    assert_eq!(
        h.post(&who(h, "a"), new_thread(h, "alone", &["a", "a"], &[], "first")),
        Err(CommsError::OnlySelfRecipient)
    );
    assert_eq!(h.thread_count(), 0);

    let with_self = h
        .post(&who(h, "a"), new_thread(h, "pair", &["a", "b"], &[], "first"))
        .expect("thread created");
    assert_eq!(list(h, "a").threads[0].peers, agents(h, &["b"]));
    assert_eq!(h.position(&with_self.thread, &agent(h, "a")), Some((0, 0)));
}

/// A replayed post returns its first reply and stores nothing, for [`REPLAY_TTL`], per agent.
pub fn replayed_post_into_a_thread_returns_the_first_reply(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    let a = who(h, "a").with_replay_key(key("relay-1"));

    let first = h.post(&a, into(&created.thread, "second")).expect("post");
    let stored = h.arrivals();
    let replay = h.post(&a, into(&created.thread, "second")).expect("post");
    assert_eq!(first.id.seq(), 2);
    assert_eq!(replay, first);
    assert_eq!(h.arrivals(), stored, "a replay stores nothing");

    let b = who(h, "b").with_replay_key(key("relay-1"));
    assert_eq!(h.post(&b, into(&created.thread, "mine")).expect("post").id.seq(), 3);

    h.advance(REPLAY_TTL + Duration::from_secs(1));
    assert_eq!(h.post(&a, into(&created.thread, "second")).expect("post").id.seq(), 4);
}

/// Without a key there is no dedupe: a repeated call is a second message.
pub fn post_without_a_replay_key_always_makes_a_new_message(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);

    assert_eq!(say(h, "a", &created.thread, "same").expect("post").id.seq(), 2);
    assert_eq!(say(h, "a", &created.thread, "same").expect("post").id.seq(), 3);
}

/// A replayed creating post returns the thread it already created.
pub fn replayed_creating_post_returns_the_thread_already_created(h: &impl ConformanceHarness) {
    let a = who(h, "a").with_replay_key(key("relay-1"));

    let first = h.post(&a, new_thread(h, "topic", &["b"], &[], "first")).expect("post");
    let replay = h.post(&a, new_thread(h, "topic", &["b"], &[], "first")).expect("post");

    assert!(first.created);
    assert_eq!(replay, first);
    assert_eq!(h.thread_count(), 1);
}

/// A post into a thread dedupes on `(from, thread, key)`: the same key elsewhere is a new post.
pub fn same_replay_key_in_another_thread_is_a_new_post(h: &impl ConformanceHarness) {
    let one = start(h, "a", "one", &["b"]);
    let two = start(h, "a", "two", &["b"]);
    let a = who(h, "a").with_replay_key(key("relay-1"));

    let in_one = h.post(&a, into(&one.thread, "same key")).expect("post");
    let in_two = h.post(&a, into(&two.thread, "same key")).expect("post");

    assert_eq!(in_one.thread, one.thread);
    assert_eq!(in_two.thread, two.thread);
    assert_eq!(ids(&read(h, "b")).len(), 4);
    assert_eq!(h.post(&a, into(&one.thread, "same key")), Ok(in_one));
}

/// A reply dedupes like any post into its thread.
pub fn replayed_reply_returns_the_first_reply(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    let b = who(h, "b").with_replay_key(key("relay-7"));

    let first = h.post(&b, reply_to(&created.id, "answer")).expect("post");
    let replay = h.post(&b, reply_to(&created.id, "answer")).expect("post");

    assert_eq!(replay, first);
    assert_eq!(bodies(&read(h, "a")), ["answer"]);
    let whole = h.inbox(&who(h, "a"), by_id(&first.id)).expect("inbox");
    assert_eq!(whole.messages[0].reply_to, Some(created.id));
}

/// The arrivals counter moves on every stored post and on nothing else.
pub fn arrivals_increase_with_every_stored_post(h: &impl ConformanceHarness) {
    let before = h.arrivals();
    let created = start(h, "a", "topic", &["b"]);
    let after_create = h.arrivals();
    assert!(after_create > before);

    say(h, "b", &created.thread, "second").expect("post");
    let after_post = h.arrivals();
    assert!(after_post > after_create);

    read(h, "a");
    h.inbox(&who(h, "a"), InboxBody::default()).expect("inbox");
    list(h, "a");
    assert!(say(h, "c", &created.thread, "refused").is_err());
    assert_eq!(h.arrivals(), after_post);
}
