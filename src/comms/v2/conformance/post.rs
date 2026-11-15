//! Posting: thread creation, membership, "seen", limits and replay.

use std::time::Duration;

use super::super::api::{
    Actor, CommsError, InboxBody, MAX_POST_BODY_BYTES, MAX_RECIPIENTS, MAX_THREAD_PATHS, MessageId, PostBody,
    PostTarget, REPLAY_TTL, ReplayKey, SEEN_REFRESH_INTERVAL, SEEN_TTL, ThreadAction,
};
use super::{
    ConformanceHarness, agent, agents, bodies, by_id, change, describe, gone_thread, ids, into, list, new_thread,
    notify, principals, read, say, start, who,
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

/// A seen record is written on an actor's first call and rewritten once it is a day old, and a
/// sweep removes it 31 days after it was written: the actor is seen for at least
/// [`SEEN_TTL`] after its last call and at most [`SEEN_REFRESH_INTERVAL`] longer.
pub fn seen_record_is_swept_thirty_to_thirty_one_days_after_the_last_call(h: &impl ConformanceHarness) {
    let second = Duration::from_secs(1);
    read(h, "b");
    h.advance(SEEN_REFRESH_INTERVAL - second);
    read(h, "b");
    h.advance(SEEN_TTL);
    h.sweep();
    assert_eq!(
        start(h, "a", "one", &["b"]).pending,
        Vec::<Actor>::new(),
        "thirty days after its last call, which did not rewrite the record"
    );
    h.advance(2 * second);
    h.sweep();
    assert_eq!(start(h, "a", "two", &["b"]).pending, agents(h, &["b"]));
}

/// A record a day old is rewritten by the next call, so an actor that keeps calling stays seen
/// however long it does.
pub fn agent_that_keeps_calling_stays_seen(h: &impl ConformanceHarness) {
    read(h, "b");
    for _ in 0..3 {
        h.advance(SEEN_TTL / 2);
        read(h, "b");
    }
    h.advance(SEEN_TTL - Duration::from_secs(1));
    h.sweep();
    assert_eq!(start(h, "a", "one", &["b"]).pending, Vec::<Actor>::new());
}

/// An actor's marks are removed with its seen record. When it calls again it is first seen
/// again and starts by the rule of a new actor.
pub fn marks_go_with_the_seen_record_and_a_returning_actor_starts_again(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    let helper = agent(h, "b.helper");
    assert_eq!(bodies(&read(h, "b.helper")), ["first"]);
    h.advance(SEEN_TTL - Duration::from_secs(1));
    say(h, "a", &created.thread, "second").expect("post");
    assert_eq!(h.position(&created.thread, &helper), Some((1, 1)));

    h.advance(SEEN_REFRESH_INTERVAL + Duration::from_secs(2));
    h.sweep();

    assert_eq!(h.position(&created.thread, &helper), Some((0, 0)));
    assert_eq!(
        h.post(&who(h, "a"), new_thread(h, "again", &["b.helper"], &[], "first"))
            .expect("thread created")
            .pending,
        [helper]
    );
    assert_eq!(bodies(&read(h, "b.helper")), ["second", "first"]);
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

/// Membership is the principal's: a sub-identity posts into, reads and is described the threads
/// its principal is a member of, and may archive what its principal created.
pub fn sub_identity_acts_in_the_threads_of_its_principal(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);

    say(h, "b.helper", &created.thread, "from the helper").expect("a sub-identity of a member");
    assert_eq!(bodies(&read(h, "a")), ["from the helper"]);
    let summary = describe(h, &who(h, "b.helper"), &created.thread).expect("thread");
    assert!(summary.member);
    assert_eq!(summary.peers, principals(h, &["a"]));
    assert_eq!(
        say(h, "c.helper", &created.thread, "hi"),
        Err(CommsError::NotMember {
            thread: created.thread.clone()
        })
    );
    let archived = change(h, &who(h, "a.helper"), &created.thread, ThreadAction::Archive).expect("archive");
    assert!(archived.threads[0].archived);
}

/// A sub-identity named in `to` or `invite` makes its principal the member, and is itself what
/// `pending` lists.
pub fn naming_a_sub_identity_makes_its_principal_the_member(h: &impl ConformanceHarness) {
    let created = h
        .post(&who(h, "a"), new_thread(h, "topic", &["b.helper"], &[], "first"))
        .expect("thread created");
    assert_eq!(created.pending, agents(h, &["b.helper"]));
    assert_eq!(list(h, "a").threads[0].peers, principals(h, &["b"]));
    assert_eq!(bodies(&read(h, "b")), ["first"]);

    let invited = change(
        h,
        &who(h, "a"),
        &created.thread,
        ThreadAction::Invite(agents(h, &["c.helper", "c", "b.other"])),
    )
    .expect("invite");
    assert_eq!(invited.pending, agents(h, &["b.other", "c", "c.helper"]));
    assert_eq!(invited.threads[0].peers, principals(h, &["b", "c"]));
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

/// A body of [`MAX_POST_BODY_BYTES`] is stored and read back whole by id; one byte more is
/// refused, not cut.
pub fn body_is_capped_at_sixteen_kibibytes(h: &impl ConformanceHarness) {
    assert_eq!(MAX_POST_BODY_BYTES, 16 * 1024);
    let largest = "x".repeat(MAX_POST_BODY_BYTES);
    let created = h
        .post(&who(h, "a"), new_thread(h, "topic", &["b"], &[], &largest))
        .expect("a body at the limit");

    let whole = h.inbox(&who(h, "b"), by_id(&created.id)).expect("inbox");
    assert_eq!(whole.messages[0].body.len(), MAX_POST_BODY_BYTES);
    assert_eq!(whole.messages[0].body_bytes, MAX_POST_BODY_BYTES as u64);
    assert_eq!(
        say(h, "a", &created.thread, &format!("{largest}x")),
        Err(CommsError::BodyTooLarge {
            len: MAX_POST_BODY_BYTES + 1,
            max: MAX_POST_BODY_BYTES,
        })
    );
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
    assert_eq!(list(h, "a").threads[0].peers, principals(h, &["b"]));
    assert_eq!(h.position(&with_self.thread, &agent(h, "a")), Some((0, 0)));
}

/// A replayed post returns its first reply and stores nothing, per agent, until a sweep removes
/// the entry once it is [`REPLAY_TTL`] old.
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
    assert_eq!(h.post(&a, into(&created.thread, "second")), Ok(first));
    h.sweep();
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
