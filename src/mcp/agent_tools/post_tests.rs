//! `agent_post` against the in-memory reference store.

use serde_json::json;

use super::super::tests::{Agent, actor, args, message, parse_error, refused, thread};
use super::super::{AgentPostParams, AgentPostResponse, AgentSession};
use super::run_agent_post;
use crate::comms::v2::api::{
    AgentOps, MAX_ACTIVE_THREADS_PER_REPO, MAX_POST_BODY_BYTES, MAX_RECIPIENTS, MAX_THREAD_PATHS, ReplayKey,
    cap_archive_notice,
};
use crate::comms::v2::fake::FakeAgents;

const NO_TARGET: &str = "`agent_post`: pass exactly one of thread, reply_to, subject; this call set";

#[test]
fn a_subject_starts_a_thread_and_thread_or_reply_to_post_into_it() {
    let fake = FakeAgents::new();
    let (alice, bob) = (Agent::new("alice"), Agent::new("bob"));

    let started = alice
        .post(
            &fake,
            json!({ "subject": "auth refactor", "body": "first", "to": ["bob"], "paths": ["src/auth/"] }),
        )
        .expect("a post that starts a thread");
    assert_eq!(
        started,
        AgentPostResponse {
            id: message("t-aaaaaa.1"),
            thread: thread("t-aaaaaa"),
            created: true,
            pending: vec![actor("bob")],
            notices: Vec::new(),
        }
    );

    let posted = bob
        .post(&fake, json!({ "thread": "t-aaaaaa", "body": "second" }))
        .expect("a post into the thread");
    assert_eq!(
        posted,
        AgentPostResponse {
            id: message("t-aaaaaa.2"),
            thread: thread("t-aaaaaa"),
            created: false,
            pending: Vec::new(),
            notices: Vec::new(),
        }
    );

    let replied = alice
        .post(&fake, json!({ "reply_to": "t-aaaaaa.2", "body": "third" }))
        .expect("a reply");
    assert_eq!(
        serde_json::to_value(&replied).expect("serialize"),
        json!({ "id": "t-aaaaaa.3", "thread": "t-aaaaaa", "notices": [] })
    );
    assert_eq!(fake.arrivals(), 3);

    let read = bob.inbox(&fake, json!({})).expect("bob's inbox");
    let reply_targets: Vec<_> = read.messages.iter().map(|message| message.reply_to.clone()).collect();
    assert_eq!(reply_targets, [None, Some(message("t-aaaaaa.2"))]);
}

#[test]
fn an_unknown_field_is_refused_naming_the_accepted_ones() {
    let refusal = parse_error::<AgentPostParams>(json!({ "subject": "s", "body": "b", "from": "mallory" }));

    assert!(refusal.starts_with("`agent_post`: unknown field `from`"), "{refusal}");
    let (_, accepted) = refusal.split_once("Accepted fields: ").expect("the accepted fields");
    for field in [
        "`body` (required)",
        "`thread`",
        "`reply_to`",
        "`subject`",
        "`to`",
        "`paths`",
        "`as`",
    ] {
        assert!(accepted.contains(field), "{field} is missing from: {accepted}");
    }
    assert_eq!(accepted.matches('`').count(), 14, "seven fields: {accepted}");
}

#[test]
fn a_missing_or_mistyped_field_is_refused_by_name() {
    let missing = parse_error::<AgentPostParams>(json!({ "subject": "s" }));
    assert!(missing.contains("missing field `body`"), "{missing}");

    let mistyped = parse_error::<AgentPostParams>(json!({ "subject": "s", "body": "b", "to": "bob" }));
    assert!(mistyped.contains("`to` must be an array"), "{mistyped}");
}

#[test]
fn a_post_needs_exactly_one_of_thread_reply_to_and_subject() {
    let fake = FakeAgents::new();
    let alice = Agent::new("alice");

    for (arguments, set) in [
        (json!({ "body": "b" }), "[]"),
        (
            json!({ "body": "b", "thread": "", "reply_to": "", "subject": "" }),
            "[]",
        ),
        (
            json!({ "body": "b", "thread": "t-aaaaaa", "subject": "s" }),
            "[thread, subject]",
        ),
        (
            json!({ "body": "b", "thread": "t-aaaaaa", "reply_to": "t-aaaaaa.1" }),
            "[thread, reply_to]",
        ),
        (
            json!({ "body": "b", "thread": "t-aaaaaa", "reply_to": "t-aaaaaa.1", "subject": "s" }),
            "[thread, reply_to, subject]",
        ),
    ] {
        assert_eq!(
            refused(alice.post(&fake, arguments)),
            format!("{NO_TARGET} {set}: `subject` starts a thread, the others post into one")
        );
    }
    assert_eq!(fake.arrivals(), 0);
}

#[test]
fn to_and_paths_are_refused_on_a_post_that_creates_no_thread() {
    let fake = FakeAgents::new();
    let alice = Agent::new("alice");
    alice
        .post(&fake, json!({ "subject": "topic", "body": "first" }))
        .expect("the thread");

    assert_eq!(
        refused(alice.post(&fake, json!({ "thread": "t-aaaaaa", "body": "b", "to": ["bob"] }))),
        "`agent_post`: `to` is fixed when a thread is created and needs `subject`: to add members to an existing \
         thread use agent_threads {thread, invite}"
    );
    assert_eq!(
        refused(alice.post(
            &fake,
            json!({ "reply_to": "t-aaaaaa.1", "body": "b", "paths": ["src"] })
        )),
        "`agent_post`: `paths` is fixed when a thread is created and needs `subject`: to add members to an \
         existing thread use agent_threads {thread, invite}"
    );
    assert_eq!(fake.arrivals(), 1);
}

#[test]
fn a_malformed_id_is_refused_with_its_grammar() {
    let fake = FakeAgents::new();
    let alice = Agent::new("alice");

    let bad_thread = refused(alice.post(&fake, json!({ "thread": "T-1", "body": "b" })));
    assert!(
        bad_thread.starts_with("`agent_post`: invalid thread id \"T-1\": expected `t-` followed by 6 characters"),
        "{bad_thread}"
    );

    let bad_message = refused(alice.post(&fake, json!({ "reply_to": "t-aaaaaa", "body": "b" })));
    assert!(
        bad_message.starts_with("`agent_post`: invalid message id \"t-aaaaaa\": expected `<thread>.<seq>`"),
        "{bad_message}"
    );

    let bad_recipient = refused(alice.post(&fake, json!({ "subject": "s", "body": "b", "to": ["not an id"] })));
    assert!(
        bad_recipient.starts_with("`agent_post`: invalid agent id \"not an id\""),
        "{bad_recipient}"
    );
}

#[test]
fn as_acts_as_a_sub_identity_of_the_sessions_principal_and_as_nobody_else() {
    let fake = FakeAgents::new();
    let (alice, bob, mallory) = (Agent::new("alice"), Agent::new("bob"), Agent::new("mallory"));

    alice
        .post(
            &fake,
            json!({ "subject": "s", "body": "from the worker", "to": ["bob"], "as": "worker" }),
        )
        .expect("a post as a sub-identity");
    alice
        .post(&fake, json!({ "thread": "t-aaaaaa", "body": "as the principal" }))
        .expect("the principal is the member, so every actor of it may post");
    alice
        .post(&fake, json!({ "thread": "t-aaaaaa", "body": "as bob?", "as": "bob" }))
        .expect("`as` names a sub-identity of alice");

    let read = bob.inbox(&fake, json!({})).expect("bob's inbox");
    let senders: Vec<_> = read.messages.iter().map(|message| message.from.clone()).collect();
    assert_eq!(senders, [actor("alice.worker"), actor("alice"), actor("alice.bob")]);

    assert_eq!(
        refused(mallory.post(&fake, json!({ "thread": "t-aaaaaa", "body": "me too", "as": "worker" }))),
        "`agent_post`: not a member of thread t-aaaaaa: join it with agent_threads {thread, join: true}, or ask a \
         member to invite you"
    );
    assert_eq!(fake.arrivals(), 3);
}

#[test]
fn an_empty_as_is_the_field_left_out() {
    let fake = FakeAgents::new();
    let alice = Agent::new("alice");

    alice
        .post(&fake, json!({ "subject": "s", "body": "b", "as": "" }))
        .expect("a post as the principal");
    alice
        .post(&fake, json!({ "thread": "t-aaaaaa", "body": "again" }))
        .expect("the principal is the member");
    assert_eq!(fake.arrivals(), 2);
}

#[test]
fn an_invalid_as_is_refused_and_nothing_is_posted() {
    let fake = FakeAgents::new();
    let alice = Agent::new("alice");
    let too_long = "x".repeat(33);

    for name in ["Worker", "a.b", "has space", too_long.as_str()] {
        let refusal = refused(alice.post(&fake, json!({ "subject": "s", "body": "b", "as": name })));
        assert!(
            refusal.starts_with("`agent_post`: `as`: invalid sub-identity name "),
            "{refusal}"
        );
        assert!(
            refusal.ends_with("expected 1 to 32 characters of a-z, 0-9, `_` or `-`"),
            "{refusal}"
        );
    }

    let long_principal = Agent::new(&"p".repeat(120));
    let composed = refused(long_principal.post(&fake, json!({ "subject": "s", "body": "b", "as": "0123456789" })));
    assert!(
        composed.starts_with("`agent_post`: `as`: invalid agent id "),
        "{composed}"
    );
    assert_eq!(fake.arrivals(), 0);
}

#[test]
fn a_body_is_refused_when_empty_or_over_the_post_limit() {
    let fake = FakeAgents::new();
    let alice = Agent::new("alice");

    for blank in ["", "  \n\t"] {
        assert_eq!(
            refused(alice.post(&fake, json!({ "subject": "s", "body": blank }))),
            "`agent_post`: `body` is empty: write the message to post"
        );
    }
    assert_eq!(
        refused(alice.post(
            &fake,
            json!({ "subject": "s", "body": "x".repeat(MAX_POST_BODY_BYTES + 1) })
        )),
        "`agent_post`: `body` is 16385 bytes, above the maximum of 16384: shorten it, or split it across posts"
    );
    assert_eq!(fake.arrivals(), 0);

    alice
        .post(
            &fake,
            json!({ "subject": "s", "body": "x".repeat(MAX_POST_BODY_BYTES) }),
        )
        .expect("a body at the limit");
}

#[test]
fn a_subject_recipient_list_or_path_list_over_its_limit_is_refused_not_cut() {
    let fake = FakeAgents::new();
    let alice = Agent::new("alice");
    let post = |arguments| refused(alice.post(&fake, arguments));
    let bad_subject = "`agent_post`: `subject` must be one non-empty line of at most 200 bytes: shorten it and put the \
                       detail in `body`";

    assert_eq!(post(json!({ "subject": "s".repeat(201), "body": "b" })), bad_subject);
    assert_eq!(post(json!({ "subject": "two\nlines", "body": "b" })), bad_subject);

    let crowd: Vec<String> = (0..=MAX_RECIPIENTS).map(|n| format!("agent-{n}")).collect();
    assert_eq!(
        post(json!({ "subject": "s", "body": "b", "to": crowd })),
        "`agent_post`: `to` names 33 agents, above the maximum of 32: name fewer, and invite the rest in further \
         calls"
    );

    let paths: Vec<String> = (0..=MAX_THREAD_PATHS).map(|n| format!("src/dir{n}")).collect();
    assert_eq!(
        post(json!({ "subject": "s", "body": "b", "paths": paths })),
        "`agent_post`: `paths` has 17 entries, above the maximum of 16: name the common parent directories instead"
    );

    let outside = post(json!({ "subject": "s", "body": "b", "paths": ["../elsewhere"] }));
    assert!(
        outside.starts_with("`agent_post`: path \"../elsewhere\" is not a repository-relative path prefix"),
        "{outside}"
    );
    assert_eq!(
        post(json!({ "subject": "s", "body": "b", "to": ["alice"] })),
        "`agent_post`: `to` names only you: name another agent, or drop `to` to open the thread to the repository"
    );
    assert_eq!(fake.arrivals(), 0);
}

#[test]
fn pending_lists_the_recipients_that_have_made_no_call() {
    let fake = FakeAgents::new();
    let (alice, bob) = (Agent::new("alice"), Agent::new("bob"));
    bob.inbox(&fake, json!({})).expect("bob makes a call");

    let started = alice
        .post(&fake, json!({ "subject": "s", "body": "b", "to": ["bob", "carol"] }))
        .expect("the thread");

    assert_eq!(started.pending, [actor("carol")]);
    assert_eq!(
        serde_json::to_value(&started).expect("serialize"),
        json!({
            "id": "t-aaaaaa.1",
            "thread": "t-aaaaaa",
            "created": true,
            "pending": ["carol"],
            "notices": [],
        })
    );
}

#[test]
fn posting_needs_a_thread_that_exists_and_membership_of_it() {
    let fake = FakeAgents::new();
    let (alice, mallory) = (Agent::new("alice"), Agent::new("mallory"));
    alice
        .post(&fake, json!({ "subject": "private", "body": "b", "to": ["bob"] }))
        .expect("the thread");

    assert_eq!(
        refused(mallory.post(&fake, json!({ "thread": "t-aaaaaa", "body": "let me in" }))),
        "`agent_post`: not a member of thread t-aaaaaa: join it with agent_threads {thread, join: true}, or ask a \
         member to invite you"
    );
    assert_eq!(
        refused(alice.post(&fake, json!({ "thread": "t-zzzzzz", "body": "b" }))),
        "`agent_post`: thread t-zzzzzz not found: list your threads with agent_threads, or search with `query` or \
         `path`"
    );
    assert_eq!(
        refused(alice.post(&fake, json!({ "reply_to": "t-aaaaaa.9", "body": "b" }))),
        "`agent_post`: message t-aaaaaa.9 not found: it was never posted or has expired; read the thread with \
         agent_inbox `thread`"
    );
    assert_eq!(fake.arrivals(), 1);
}

#[test]
fn a_replayed_request_returns_the_first_post_and_stores_no_second_one() {
    let fake = FakeAgents::new();
    let alice = Agent::new("alice");
    let key = ReplayKey::parse("relay-7f3a").expect("a replay key");
    let session = AgentSession {
        replay_key: Some(&key),
        ..alice.session()
    };
    let arguments = json!({ "subject": "s", "body": "once" });

    let first = run_agent_post(&fake, &session, args(arguments.clone())).expect("the post");
    let replayed = run_agent_post(&fake, &session, args(arguments.clone())).expect("its replay");
    assert_eq!(replayed, first);
    assert_eq!(fake.arrivals(), 1);

    let repeated = alice.post(&fake, arguments).expect("a second request");
    assert_eq!(repeated.thread, thread("t-aaaaab"));
    assert_eq!(fake.arrivals(), 2);
}

#[test]
fn the_post_that_archives_a_thread_at_the_cap_carries_the_stores_notice_unchanged() {
    let fake = FakeAgents::new();
    let alice = Agent::new("alice");
    let own = ["the session's own notice".to_string()];
    let session = AgentSession {
        notices: &own,
        ..alice.session()
    };
    for n in 0..MAX_ACTIVE_THREADS_PER_REPO {
        let filled = alice
            .post(&fake, json!({ "subject": format!("topic {n}"), "body": "b" }))
            .expect("a thread under the cap");
        assert!(filled.notices.is_empty(), "{:?}", filled.notices);
    }

    let over = run_agent_post(&fake, &session, args(json!({ "subject": "one more", "body": "b" })))
        .expect("the post at the cap succeeds");

    assert!(over.created);
    assert_eq!(over.notices, [cap_archive_notice(&thread("t-aaaaaa")), own[0].clone()]);
    assert_eq!(
        refused(alice.post(&fake, json!({ "thread": "t-aaaaaa", "body": "still here?" }))),
        "`agent_post`: thread t-aaaaaa is archived: read it with agent_inbox `thread`, or start a new thread by \
         posting with `subject`"
    );
}
