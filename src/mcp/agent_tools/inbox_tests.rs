//! `agent_inbox` against the in-memory reference store.

use std::cell::RefCell;
use std::time::Duration;

use serde_json::{Value, json};

use super::super::tests::{Agent, HOME, actor, args, message, parse_error, refused, thread};
use super::super::{AgentInboxParams, AgentInboxResponse, AgentMessage};
use super::run_agent_inbox;
use crate::comms::v2::api::{
    AgentOps, Caller, CommsError, InboxBody, InboxMessage, InboxReply, InboxSelect, MAX_INLINE_BODY_BYTES, Mark,
    RESPONSE_BUDGET_BYTES, RepoKey, WaitOutcome,
};
use crate::comms::v2::conformance::{ConformanceHarness, wait_then_read};
use crate::comms::v2::fake::{FAKE_EPOCH_MICROS, FakeAgents};

/// A store in which alice has started `t-aaaaaa` with bob and posted `bodies` into it.
fn thread_with(bodies: &[&str]) -> (FakeAgents, Agent, Agent) {
    let fake = FakeAgents::new();
    let (alice, bob) = (Agent::new("alice"), Agent::new("bob"));
    for (index, body) in bodies.iter().enumerate() {
        let arguments = match index {
            0 => json!({ "subject": "auth refactor", "body": body, "to": ["bob"] }),
            _ => json!({ "thread": "t-aaaaaa", "body": body }),
        };
        alice.post(&fake, arguments).expect("alice's post");
    }
    (fake, alice, bob)
}

fn ids(response: &AgentInboxResponse) -> Vec<String> {
    response.messages.iter().map(|message| message.id.to_string()).collect()
}

/// How many messages `agent_threads` counts as unread for `agent` in `t-aaaaaa`.
fn unread(fake: &FakeAgents, agent: &Agent) -> u64 {
    let described = agent
        .threads(fake, json!({ "thread": "t-aaaaaa" }))
        .expect("the thread described");
    described.threads[0].unread
}

/// Every page of a read that pages with a cursor, as the ids on each page.
fn pages(fake: &FakeAgents, agent: &Agent, arguments: &Value) -> Vec<Vec<String>> {
    let mut pages = Vec::new();
    let mut cursor = None;
    for _ in 0..16 {
        let mut arguments = arguments.clone();
        if let Some(cursor) = &cursor {
            arguments["cursor"] = json!(cursor);
        }
        let page = agent.inbox(fake, arguments).expect("a page");
        assert_eq!(page.more, page.next_cursor.is_some(), "a cursor comes with `more`");
        assert!(page.read_to.is_empty(), "a paged read marks nothing");
        pages.push(ids(&page));
        cursor = page.next_cursor;
        if cursor.is_none() {
            return pages;
        }
    }
    panic!("the read never reached its last page: {pages:?}")
}

fn not_applicable(field: &str, with: &str) -> String {
    format!("`agent_inbox`: `{field}` does not apply together with `{with}`: drop `{field}`, or make two calls")
}

#[test]
fn no_arguments_returns_the_unread_messages_and_marks_them_read() {
    let (fake, _alice, bob) = thread_with(&["first"]);

    let read = bob.inbox(&fake, json!({})).expect("bob's inbox");

    assert_eq!(
        read,
        AgentInboxResponse {
            messages: vec![AgentMessage {
                id: message("t-aaaaaa.1"),
                subject: "auth refactor".to_string(),
                from: actor("alice"),
                ts_micros: FAKE_EPOCH_MICROS,
                reply_to: None,
                body: "first".to_string(),
                body_bytes: 5,
                truncated: false,
            }],
            read_to: vec![message("t-aaaaaa.1")],
            more: false,
            next_cursor: None,
            wait: WaitOutcome::NotWaited,
            notices: Vec::new(),
        }
    );
    assert_eq!(
        serde_json::to_value(&read).expect("serialize"),
        json!({
            "messages": [{
                "id": "t-aaaaaa.1",
                "subject": "auth refactor",
                "from": "alice",
                "ts_micros": FAKE_EPOCH_MICROS,
                "body": "first",
                "body_bytes": 5,
            }],
            "read_to": ["t-aaaaaa.1"],
            "more": false,
            "notices": [],
        })
    );

    let again = bob.inbox(&fake, json!({})).expect("bob's inbox, read");
    assert_eq!(
        serde_json::to_value(&again).expect("serialize"),
        json!({ "messages": [], "more": false, "notices": [] })
    );
    assert_eq!(unread(&fake, &bob), 0);
}

#[test]
fn peek_returns_the_unread_messages_and_leaves_them_unread() {
    let (fake, _alice, bob) = thread_with(&["first", "second"]);

    for _ in 0..2 {
        let peeked = bob.inbox(&fake, json!({ "peek": true })).expect("a peek");
        assert_eq!(ids(&peeked), ["t-aaaaaa.1", "t-aaaaaa.2"]);
        assert!(peeked.read_to.is_empty());
    }
    assert_eq!(unread(&fake, &bob), 2);

    let read = bob.inbox(&fake, json!({})).expect("the read that marks");
    assert_eq!(ids(&read), ["t-aaaaaa.1", "t-aaaaaa.2"]);
    assert_eq!(read.read_to, [message("t-aaaaaa.2")]);
    assert_eq!(unread(&fake, &bob), 0);
    assert!(
        bob.inbox(&fake, json!({ "peek": true }))
            .expect("a peek")
            .messages
            .is_empty()
    );
}

#[test]
fn unread_false_reads_history_with_the_callers_own_posts_and_marks_nothing() {
    let (fake, _alice, bob) = thread_with(&["first"]);
    bob.post(&fake, json!({ "thread": "t-aaaaaa", "body": "second" }))
        .expect("bob's post");

    for arguments in [json!({ "unread": false }), json!({ "unread": false, "peek": true })] {
        let history = bob.inbox(&fake, arguments).expect("history");
        assert_eq!(ids(&history), ["t-aaaaaa.1", "t-aaaaaa.2"]);
        assert_eq!(history.messages[1].from, actor("bob"));
        assert!(history.read_to.is_empty());
    }

    assert_eq!(unread(&fake, &bob), 1);
    assert_eq!(
        ids(&bob.inbox(&fake, json!({})).expect("the unread read")),
        ["t-aaaaaa.1"]
    );
}

#[test]
fn thread_limits_the_read_to_one_thread() {
    let (fake, alice, bob) = thread_with(&["in the first thread"]);
    alice
        .post(
            &fake,
            json!({ "subject": "second", "body": "in the second", "to": ["bob"] }),
        )
        .expect("a second thread");

    let one = bob.inbox(&fake, json!({ "thread": "t-aaaaab" })).expect("one thread");
    assert_eq!(ids(&one), ["t-aaaaab.1"]);
    assert_eq!(one.messages[0].subject, "second");

    let rest = bob.inbox(&fake, json!({})).expect("every thread");
    assert_eq!(ids(&rest), ["t-aaaaaa.1"]);
}

#[test]
fn an_unknown_field_is_refused_naming_the_accepted_ones() {
    let refusal = parse_error::<AgentInboxParams>(json!({ "ack": true }));

    assert!(refusal.starts_with("`agent_inbox`: unknown field `ack`"), "{refusal}");
    let (_, accepted) = refusal.split_once("Accepted fields: ").expect("the accepted fields");
    for field in [
        "`thread`",
        "`id`",
        "`unread`",
        "`peek`",
        "`announce`",
        "`wait_secs`",
        "`limit`",
        "`cursor`",
        "`as`",
    ] {
        assert!(accepted.contains(field), "{field} is missing from: {accepted}");
    }
    assert_eq!(accepted.matches('`').count(), 18, "nine fields: {accepted}");
    assert!(!accepted.contains("(required)"), "{accepted}");
}

#[test]
fn a_scalar_sent_as_a_string_is_read_and_a_mistyped_one_is_refused_by_name() {
    let params: AgentInboxParams = args(json!({ "limit": "5", "peek": "true", "wait_secs": 3.0 }));
    assert_eq!(
        (params.limit, params.peek, params.wait_secs),
        (Some(5), Some(true), Some(3))
    );

    let mistyped = parse_error::<AgentInboxParams>(json!({ "wait_secs": -1, "peek": "maybe" }));
    assert!(mistyped.contains("`wait_secs` must be an integer"), "{mistyped}");
    assert!(mistyped.contains("`peek` must be a boolean"), "{mistyped}");
}

#[test]
fn a_field_that_does_not_apply_is_refused_not_ignored() {
    let (fake, _alice, bob) = thread_with(&["first"]);
    let by_id = |extra: Value| {
        let mut arguments = json!({ "id": "t-aaaaaa.1" });
        for (field, value) in extra.as_object().expect("an object") {
            arguments[field.as_str()] = value.clone();
        }
        refused(bob.inbox(&fake, arguments))
    };

    assert_eq!(by_id(json!({ "thread": "t-aaaaaa" })), not_applicable("thread", "id"));
    assert_eq!(by_id(json!({ "limit": 5 })), not_applicable("limit", "id"));
    assert_eq!(by_id(json!({ "peek": true })), not_applicable("peek", "id"));
    assert_eq!(by_id(json!({ "unread": false })), not_applicable("unread", "id"));
    assert_eq!(by_id(json!({ "cursor": "abc" })), not_applicable("cursor", "id"));
    assert_eq!(by_id(json!({ "wait_secs": 5 })), not_applicable("wait_secs", "id"));
    assert_eq!(
        refused(bob.inbox(&fake, json!({ "unread": false, "wait_secs": 5 }))),
        not_applicable("wait_secs", "unread")
    );
    assert_eq!(
        refused(bob.inbox(&fake, json!({ "cursor": "abc" }))),
        "`agent_inbox`: `cursor` does not apply to an unread read that marks what it returns: it continues from \
         your read position, so call again without `cursor`; only `peek` and `unread: false` reads page with one"
    );
    assert_eq!(unread(&fake, &bob), 1, "a refused read marks nothing");
}

#[test]
fn a_field_at_its_default_or_empty_counts_as_left_out() {
    let (fake, _alice, bob) = thread_with(&["first"]);

    let read = bob
        .inbox(
            &fake,
            json!({
                "id": "t-aaaaaa.1",
                "thread": "",
                "unread": true,
                "peek": false,
                "announce": false,
                "wait_secs": 0,
                "limit": 20,
                "cursor": "",
                "as": "",
            }),
        )
        .expect("a read by id");

    assert_eq!(ids(&read), ["t-aaaaaa.1"]);
}

#[test]
fn wait_secs_over_forty_is_refused_not_clamped() {
    let (fake, _alice, bob) = thread_with(&["first"]);

    assert_eq!(
        refused(bob.inbox(&fake, json!({ "wait_secs": 41 }))),
        "`agent_inbox`: `wait_secs` is 41, above the maximum of 40: wait at most 40 seconds and call again"
    );
    assert_eq!(unread(&fake, &bob), 1);
}

#[test]
fn a_call_that_waits_hands_the_checked_read_to_the_daemons_hook() {
    let (fake, _alice, bob) = thread_with(&["first"]);
    let asked = RefCell::new(Vec::new());
    let waiting_read = |caller: &Caller, body: InboxBody, wait: Duration| -> Result<InboxReply, CommsError> {
        asked.borrow_mut().push((caller.clone(), body.clone(), wait));
        fake.inbox(caller, body)
    };

    let read = run_agent_inbox(
        &fake,
        &bob.session(),
        args(json!({ "thread": "t-aaaaaa", "peek": true, "limit": 5, "wait_secs": 40, "as": "reviewer" })),
        &waiting_read,
    );

    assert_eq!(
        ids(&read.expect("the hook's reply")),
        ["t-aaaaaa.1"],
        "the reviewer reads as a member: the member is its principal, bob"
    );
    assert_eq!(
        asked.into_inner(),
        [(
            Caller::new(actor("bob.reviewer"), RepoKey::parse(HOME).expect("a repository key")),
            InboxBody {
                select: InboxSelect::Thread(thread("t-aaaaaa")),
                unread: true,
                mark: Mark::Peek,
                limit: Some(5),
                cursor: None,
            },
            Duration::from_secs(40),
        )]
    );
}

#[test]
fn a_message_that_arrives_during_the_wait_is_returned_and_marked() {
    let (fake, alice, bob) = thread_with(&["first"]);
    bob.inbox(&fake, json!({})).expect("bob reads what is there");
    let waiting_read = |caller: &Caller, body: InboxBody, wait: Duration| -> Result<InboxReply, CommsError> {
        wait_then_read(&fake, caller, body, wait, || {
            alice
                .post(&fake, json!({ "thread": "t-aaaaaa", "body": "here now" }))
                .expect("a post during the wait");
        })
    };

    let read = run_agent_inbox(&fake, &bob.session(), args(json!({ "wait_secs": 30 })), &waiting_read)
        .expect("the read that waited");

    assert_eq!(ids(&read), ["t-aaaaaa.2"]);
    assert_eq!(read.wait, WaitOutcome::Arrived);
    assert_eq!(read.read_to, [message("t-aaaaaa.2")]);
    assert_eq!(
        serde_json::to_value(&read).expect("serialize")["wait"],
        json!("arrived")
    );
    assert_eq!(unread(&fake, &bob), 0);
}

#[test]
fn a_sibling_actor_sees_its_principals_mail_from_where_the_principal_has_read() {
    let (fake, alice, bob) = thread_with(&["first", "second"]);

    bob.inbox(&fake, json!({ "limit": 1 }))
        .expect("the principal reads one");
    let reviewer = bob
        .inbox(&fake, json!({ "as": "reviewer" }))
        .expect("the sibling's first read");
    assert_eq!(
        ids(&reviewer),
        ["t-aaaaaa.2"],
        "it starts at the highest position of its principal"
    );
    assert_eq!(
        ids(&bob.inbox(&fake, json!({})).expect("the principal")),
        ["t-aaaaaa.2"]
    );

    alice
        .post(&fake, json!({ "thread": "t-aaaaaa", "body": "third" }))
        .expect("alice's post");
    assert_eq!(
        ids(&bob.inbox(&fake, json!({ "as": "reviewer" })).expect("the sibling")),
        ["t-aaaaaa.3"]
    );
    assert_eq!(
        ids(&bob.inbox(&fake, json!({})).expect("the principal")),
        ["t-aaaaaa.3"],
        "each actor has its own read position"
    );
}

#[test]
fn a_post_by_a_sibling_is_unread_for_the_principal_and_never_for_its_poster() {
    let (fake, _alice, bob) = thread_with(&["first"]);
    bob.inbox(&fake, json!({})).expect("the principal reads");

    bob.post(&fake, json!({ "thread": "t-aaaaaa", "body": "done", "as": "worker" }))
        .expect("the sibling posts");

    assert!(
        bob.inbox(&fake, json!({ "as": "worker" }))
            .expect("the poster")
            .messages
            .is_empty()
    );
    let read = bob.inbox(&fake, json!({})).expect("the principal");
    assert_eq!(ids(&read), ["t-aaaaaa.2"]);
    assert_eq!(read.messages[0].from, actor("bob.worker"));
}

#[test]
fn an_invalid_as_is_refused_and_as_never_names_another_principal() {
    let (fake, _alice, bob) = thread_with(&["first"]);

    for name in ["Reviewer", "bob.reviewer"] {
        let refusal = refused(bob.inbox(&fake, json!({ "as": name })));
        assert!(
            refusal.starts_with(&format!("`agent_inbox`: `as`: invalid sub-identity name {name:?}")),
            "{refusal}"
        );
    }
    assert_eq!(fake.position(&thread("t-aaaaaa"), &actor("bob")), Some((0, 0)));

    let read = bob
        .inbox(&fake, json!({ "as": "alice" }))
        .expect("the sub-identity of bob that is named alice");
    assert_eq!(ids(&read), ["t-aaaaaa.1"], "bob.alice reads bob's mail, not alice's");
    assert_eq!(fake.position(&thread("t-aaaaaa"), &actor("bob.alice")), Some((1, 1)));
}

#[test]
fn announce_lists_front_matter_once_and_leaves_the_messages_unread() {
    let (fake, _alice, bob) = thread_with(&["first", "the second one", "third"]);
    let marks = || fake.position(&thread("t-aaaaaa"), &actor("bob"));

    let told = bob
        .inbox(&fake, json!({ "announce": true, "limit": 2 }))
        .expect("an announcing read");

    assert_eq!(ids(&told), ["t-aaaaaa.1", "t-aaaaaa.2"], "at most `limit`");
    assert!(told.more);
    assert_eq!(told.next_cursor, None);
    assert!(told.read_to.is_empty());
    assert!(told.messages.iter().all(|message| message.body.is_empty()));
    assert_eq!(told.messages[1].body_bytes, 14);
    assert_eq!(
        serde_json::to_value(&told).expect("serialize"),
        json!({
            "messages": [
                {
                    "id": "t-aaaaaa.1",
                    "subject": "auth refactor",
                    "from": "alice",
                    "ts_micros": FAKE_EPOCH_MICROS,
                    "body_bytes": 5,
                },
                {
                    "id": "t-aaaaaa.2",
                    "subject": "auth refactor",
                    "from": "alice",
                    "ts_micros": FAKE_EPOCH_MICROS + 1,
                    "body_bytes": 14,
                },
            ],
            "more": true,
            "notices": [],
        })
    );
    assert_eq!(marks(), Some((0, 2)), "only the announcement mark moved");
    assert_eq!(unread(&fake, &bob), 3);

    let rest = bob.inbox(&fake, json!({ "announce": true })).expect("the rest");
    assert_eq!(ids(&rest), ["t-aaaaaa.3"]);
    assert!(!rest.more);
    assert!(
        bob.inbox(&fake, json!({ "announce": true }))
            .expect("nothing new to announce")
            .messages
            .is_empty()
    );
    assert_eq!(marks(), Some((0, 3)));

    let read = bob.inbox(&fake, json!({})).expect("the read that marks");
    assert_eq!(ids(&read), ["t-aaaaaa.1", "t-aaaaaa.2", "t-aaaaaa.3"]);
    assert_eq!(read.messages[1].body, "the second one");
    assert_eq!(marks(), Some((3, 3)));
}

#[test]
fn announce_is_refused_beside_a_field_that_asks_for_another_read() {
    let (fake, _alice, bob) = thread_with(&["first"]);
    let announcing = |field: &str, value: Value| {
        let mut arguments = json!({ "announce": true });
        arguments[field] = value;
        refused(bob.inbox(&fake, arguments))
    };

    assert_eq!(announcing("id", json!("t-aaaaaa.1")), not_applicable("id", "announce"));
    assert_eq!(announcing("peek", json!(true)), not_applicable("peek", "announce"));
    assert_eq!(announcing("unread", json!(false)), not_applicable("unread", "announce"));
    assert_eq!(
        announcing("wait_secs", json!(5)),
        not_applicable("wait_secs", "announce")
    );
    assert!(
        announcing("cursor", json!("abc")).starts_with("`agent_inbox`: `cursor` does not apply"),
        "an announcing read takes no cursor"
    );
    assert_eq!(
        fake.position(&thread("t-aaaaaa"), &actor("bob")),
        Some((0, 0)),
        "a refused read announces nothing"
    );
}

#[test]
fn a_body_over_the_inline_limit_is_cut_flagged_and_read_whole_by_id() {
    let long = "x".repeat(MAX_INLINE_BODY_BYTES + 904);
    let (fake, _alice, bob) = thread_with(&[long.as_str()]);

    let read = bob.inbox(&fake, json!({})).expect("bob's inbox");
    assert_eq!(read.messages[0].body.len(), MAX_INLINE_BODY_BYTES);
    assert!(read.messages[0].truncated);
    assert_eq!(
        serde_json::to_value(&read).expect("serialize")["messages"][0]["truncated"],
        json!(true)
    );
    assert_eq!(unread(&fake, &bob), 0, "a cut message counts as read");

    let whole = bob
        .inbox(&fake, json!({ "id": "t-aaaaaa.1" }))
        .expect("the message by id");
    assert_eq!(whole.messages[0].body, long);
    assert!(!whole.messages[0].truncated);
    assert!(whole.read_to.is_empty());
}

#[test]
fn a_cut_body_ends_on_a_character_boundary() {
    let long = "€".repeat(2_000);
    let (fake, _alice, bob) = thread_with(&[long.as_str()]);

    let read = bob.inbox(&fake, json!({})).expect("bob's inbox");

    assert_eq!(read.messages[0].body, "€".repeat(1_365));
    assert!(read.messages[0].truncated);
}

#[test]
fn a_delivered_message_is_passed_on_as_the_store_returned_it() {
    let delivered = |seq: u64, body: String, body_bytes: u64, truncated: bool| InboxMessage {
        id: message(&format!("t-aaaaaa.{seq}")),
        subject: "topic".to_string(),
        from: actor("alice"),
        ts_micros: FAKE_EPOCH_MICROS,
        reply_to: None,
        body,
        body_bytes,
        truncated,
    };
    let over_budget = "x".repeat(RESPONSE_BUDGET_BYTES + 1);
    let reply = InboxReply {
        messages: vec![
            delivered(1, "x".repeat(MAX_INLINE_BODY_BYTES), 9_000, true),
            delivered(2, over_budget.clone(), over_budget.len() as u64, false),
            delivered(3, String::new(), 7, false),
        ],
        read_to: vec![message("t-aaaaaa.3")],
        more: true,
        next_cursor: None,
        notices: Vec::new(),
        wait: WaitOutcome::NotWaited,
    };

    let response = AgentInboxResponse::from_reply(reply, Vec::new());

    let shown: Vec<_> = response
        .messages
        .iter()
        .map(|message| (message.body.len(), message.body_bytes, message.truncated))
        .collect();
    assert_eq!(
        shown,
        [
            (MAX_INLINE_BODY_BYTES, 9_000, true),
            (RESPONSE_BUDGET_BYTES + 1, (RESPONSE_BUDGET_BYTES + 1) as u64, false),
            (0, 7, false),
        ],
        "a read that marks has moved past all three: none may be cut or dropped here"
    );
    assert_eq!(response.read_to, [message("t-aaaaaa.3")]);
    assert!(response.more);
}

#[test]
fn a_read_that_marks_continues_from_the_read_position_until_nothing_is_left() {
    let (fake, _alice, bob) = thread_with(&["1", "2", "3", "4", "5"]);
    let mut seen = Vec::new();

    let first = bob.inbox(&fake, json!({ "limit": 2 })).expect("the first two");
    assert!(first.more);
    assert_eq!(first.next_cursor, None, "the read position is the cursor");
    assert_eq!(first.read_to, [message("t-aaaaaa.2")]);
    seen.extend(ids(&first));

    let second = bob.inbox(&fake, json!({ "limit": 2 })).expect("the next two");
    assert!(second.more);
    seen.extend(ids(&second));

    let last = bob.inbox(&fake, json!({ "limit": 2 })).expect("the last one");
    assert!(!last.more);
    seen.extend(ids(&last));

    assert_eq!(
        seen,
        ["t-aaaaaa.1", "t-aaaaaa.2", "t-aaaaaa.3", "t-aaaaaa.4", "t-aaaaaa.5"]
    );
    assert!(bob.inbox(&fake, json!({})).expect("nothing left").messages.is_empty());
}

#[test]
fn paging_with_the_cursor_returns_every_message_once() {
    let (fake, _alice, bob) = thread_with(&["1", "2", "3", "4", "5"]);
    let expected: [&[&str]; 3] = [
        &["t-aaaaaa.1", "t-aaaaaa.2"],
        &["t-aaaaaa.3", "t-aaaaaa.4"],
        &["t-aaaaaa.5"],
    ];
    let wider: [&[&str]; 2] = [
        &["t-aaaaaa.1", "t-aaaaaa.2", "t-aaaaaa.3", "t-aaaaaa.4"],
        &["t-aaaaaa.5"],
    ];

    assert_eq!(pages(&fake, &bob, &json!({ "peek": true, "limit": 2 })), expected);
    assert_eq!(pages(&fake, &bob, &json!({ "unread": false, "limit": 2 })), expected);
    assert_eq!(
        pages(&fake, &bob, &json!({ "thread": "t-aaaaaa", "peek": true, "limit": 4 })),
        wider
    );
    assert_eq!(unread(&fake, &bob), 5);
}

#[test]
fn a_cursor_of_another_read_or_another_agent_is_refused() {
    let (fake, alice, bob) = thread_with(&["1", "2", "3"]);
    let peeked = bob
        .inbox(&fake, json!({ "peek": true, "limit": 1 }))
        .expect("a page of a peek");
    let cursor = json!(peeked.next_cursor.expect("a cursor"));
    let invalid = |refusal: String| {
        assert!(refusal.starts_with("`agent_inbox`: `cursor` "), "{refusal}");
        assert!(
            refusal.ends_with("is not a `next_cursor` of this request: repeat the call without `cursor`"),
            "{refusal}"
        );
    };

    invalid(refused(bob.inbox(&fake, json!({ "unread": false, "cursor": cursor }))));
    invalid(refused(bob.inbox(
        &fake,
        json!({ "peek": true, "thread": "t-aaaaaa", "cursor": cursor }),
    )));
    invalid(refused(alice.inbox(&fake, json!({ "peek": true, "cursor": cursor }))));
    invalid(refused(
        bob.inbox(&fake, json!({ "peek": true, "cursor": "not-a-cursor" })),
    ));

    let next = bob
        .inbox(&fake, json!({ "peek": true, "limit": 1, "cursor": cursor }))
        .expect("the cursor's own read");
    assert_eq!(ids(&next), ["t-aaaaaa.2"]);
}

#[test]
fn limit_defaults_to_twenty_and_zero_means_one() {
    let (fake, _alice, bob) = thread_with(&["m"; 21]);

    let one = bob.inbox(&fake, json!({ "peek": true, "limit": 0 })).expect("a page");
    assert_eq!(ids(&one), ["t-aaaaaa.1"]);
    assert!(one.more);

    let page = bob.inbox(&fake, json!({})).expect("the default page");
    assert_eq!(page.messages.len(), 20);
    assert!(page.more);
    assert_eq!(page.read_to, [message("t-aaaaaa.20")]);

    let rest = bob.inbox(&fake, json!({})).expect("the rest");
    assert_eq!(ids(&rest), ["t-aaaaaa.21"]);
    assert!(!rest.more);
}

#[test]
fn the_response_budget_stops_the_reply_and_says_so() {
    let body = "x".repeat(4_000);
    let (fake, _alice, bob) = thread_with(&[body.as_str(); 6]);
    let cost = |message: &AgentMessage| {
        message.body.len() + message.subject.len() + message.from.as_str().len() + message.id.to_string().len()
    };

    let peeked = bob.inbox(&fake, json!({ "peek": true })).expect("a peek");
    assert_eq!(peeked.messages.len(), 4);
    assert!(peeked.more);
    assert!(peeked.next_cursor.is_some());

    let read = bob.inbox(&fake, json!({})).expect("the read that marks");
    let spent: usize = read.messages.iter().map(cost).sum();
    assert_eq!(read.messages.len(), 4, "limit 20 was not what stopped it");
    assert!(spent <= RESPONSE_BUDGET_BYTES, "{spent} bytes delivered");
    assert!(
        spent + cost(&read.messages[0]) > RESPONSE_BUDGET_BYTES,
        "a fifth would not fit"
    );
    assert!(read.messages.iter().all(|message| !message.truncated));
    assert_eq!(serde_json::to_value(&read).expect("serialize")["more"], json!(true));
    assert_eq!(read.read_to, [message("t-aaaaaa.4")], "what was left out stays unread");
    assert_eq!(unread(&fake, &bob), 2);

    let rest = bob.inbox(&fake, json!({})).expect("the rest");
    assert_eq!(ids(&rest), ["t-aaaaaa.5", "t-aaaaaa.6"]);
    assert!(!rest.more);
}

#[test]
fn reading_needs_membership_of_the_thread() {
    let (fake, alice, _bob) = thread_with(&["first"]);
    let mallory = Agent::new("mallory");
    let not_member = "`agent_inbox`: not a member of thread t-aaaaaa: join it with agent_threads {thread, join: \
                      true}, or ask a member to invite you";

    assert_eq!(
        refused(mallory.inbox(&fake, json!({ "thread": "t-aaaaaa" }))),
        not_member
    );
    assert_eq!(refused(mallory.inbox(&fake, json!({ "id": "t-aaaaaa.1" }))), not_member);
    assert_eq!(
        refused(mallory.inbox(&fake, json!({ "thread": "t-aaaaaa", "unread": false }))),
        not_member
    );
    assert!(
        mallory
            .inbox(&fake, json!({}))
            .expect("mallory's own inbox")
            .messages
            .is_empty()
    );
    assert_eq!(
        refused(alice.inbox(&fake, json!({ "id": "t-aaaaaa.7" }))),
        "`agent_inbox`: message t-aaaaaa.7 not found: it was never posted or has expired; read the thread with \
         agent_inbox `thread`"
    );
    assert_eq!(
        refused(alice.inbox(&fake, json!({ "thread": "nope" }))),
        "`agent_inbox`: invalid thread id \"nope\": expected `t-` followed by 6 characters of a-z or 2-7, as \
         returned by agent_post or agent_threads"
    );
}
