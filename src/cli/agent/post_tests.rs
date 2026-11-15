//! `agent post` against the in-memory store: what it prints, what it refuses and who it posts as.

use super::*;
use crate::cli::agent::args::AgentCmd;
use crate::cli::agent::test_support::{caller, json, parse, refusal, text};
use crate::cli::exit::{ERROR, USAGE};
use crate::comms::v2::api::{MAX_RECIPIENTS, MAX_SUBJECT_BYTES};
use crate::comms::v2::fake::FakeAgents;
use crate::comms::v2::{IdKind, InboxBody, InvalidId, MessageId, ThreadId, ThreadQuery};

const START: [&str; 7] = ["post", "--subject", "topic", "--to", "b", "--body", "hello"];

/// The request the flags after `post` make, as agent `a`.
fn request(flags: &[&str]) -> Result<(Caller, PostBody), CommsError> {
    let mut args = vec!["post"];
    args.extend_from_slice(flags);
    match parse(&args).expect("the command line parses") {
        AgentCmd::Post(args) => post_request(&caller("a"), args),
        other => panic!("not a post: {other:?}"),
    }
}

fn refused_with(flags: &[&str]) -> CommsError {
    request(flags).expect_err("the flags are refused")
}

fn thread() -> ThreadId {
    ThreadId::parse("t-aaaaaa").expect("thread id")
}

#[test]
fn post_with_subject_creates_a_thread_and_lists_recipients_not_seen() {
    let fake = FakeAgents::new();

    assert_eq!(text(&fake, "a", &START), "t-aaaaaa.1\tcreated\npending: b\n");
}

#[test]
fn post_into_a_thread_and_as_a_reply() {
    let fake = FakeAgents::new();
    text(&fake, "a", &START);

    assert_eq!(
        text(&fake, "b", &["post", "--reply-to", "t-aaaaaa.1", "--body", "on it"]),
        "t-aaaaaa.2\tposted\n"
    );
    assert_eq!(
        text(&fake, "a", &["post", "--thread", "t-aaaaaa", "--body", "thanks"]),
        "t-aaaaaa.3\tposted\n"
    );

    let unread = fake.inbox(&caller("a"), InboxBody::default()).expect("inbox");
    assert_eq!(unread.messages.len(), 1);
    assert_eq!(unread.messages[0].body, "on it");
    assert_eq!(
        unread.messages[0].reply_to,
        Some(MessageId::parse("t-aaaaaa.1").expect("message id"))
    );
}

#[test]
fn post_json_is_the_reply() {
    let fake = FakeAgents::new();

    assert_eq!(
        json(&fake, "a", &START),
        serde_json::json!({
            "id": "t-aaaaaa.1",
            "thread": "t-aaaaaa",
            "created": true,
            "pending": ["b"],
            "notices": [],
        })
    );
}

#[test]
fn post_stores_normalized_paths() {
    let fake = FakeAgents::new();
    text(
        &fake,
        "a",
        &["post", "--subject", "topic", "--paths", "src/comms/", "--body", "hello"],
    );

    let listed = fake.threads_query(&caller("a"), ThreadQuery::default()).expect("list");
    let paths: Vec<String> = listed.threads[0].paths.iter().map(ToString::to_string).collect();
    assert_eq!(paths, ["src/comms"]);
    assert!(listed.threads[0].open);
}

#[test]
fn post_needs_exactly_one_target() {
    assert_eq!(
        refused_with(&["--body", "hello"]),
        CommsError::InvalidPostTarget { given: Vec::new() }
    );
    assert_eq!(
        refused_with(&["--body", "hello", "--thread", "t-aaaaaa", "--subject", "topic"]),
        CommsError::InvalidPostTarget {
            given: vec!["thread".to_string(), "subject".to_string()],
        }
    );
    assert_eq!(
        refused_with(&["--body", "hello", "--thread", "t-aaaaaa", "--reply-to", "t-aaaaaa.1"]),
        CommsError::InvalidPostTarget {
            given: vec!["thread".to_string(), "reply_to".to_string()],
        }
    );
}

#[test]
fn post_refuses_creation_fields_without_a_subject() {
    assert_eq!(
        refused_with(&["--body", "hello", "--thread", "t-aaaaaa", "--to", "b"]),
        CommsError::CreationOnly {
            field: "to".to_string(),
        }
    );
    assert_eq!(
        refused_with(&["--body", "hello", "--reply-to", "t-aaaaaa.1", "--paths", "src"]),
        CommsError::CreationOnly {
            field: "paths".to_string(),
        }
    );
}

#[test]
fn post_refuses_malformed_values() {
    assert_eq!(
        refused_with(&["--body", "hello", "--thread", "nope"]),
        CommsError::InvalidId(InvalidId::new(IdKind::Thread, "nope"))
    );
    assert_eq!(
        refused_with(&["--body", "hello", "--reply-to", "t-aaaaaa"]),
        CommsError::InvalidId(InvalidId::new(IdKind::Message, "t-aaaaaa"))
    );
    assert_eq!(
        refused_with(&["--body", "hello", "--subject", "topic", "--to", "not an id"]),
        CommsError::InvalidId(InvalidId::new(IdKind::Actor, "not an id"))
    );
    assert_eq!(
        refused_with(&["--body", "hello", "--subject", "two\nlines"]),
        CommsError::InvalidSubject { max: MAX_SUBJECT_BYTES }
    );
    assert!(matches!(
        refused_with(&["--body", "hello", "--subject", "topic", "--paths", "../up"]),
        CommsError::InvalidPath { .. }
    ));
    assert_eq!(
        refused_with(&["--body", "  ", "--subject", "topic"]),
        CommsError::EmptyBody
    );
}

#[test]
fn post_refuses_more_recipients_than_the_cap() {
    let recipients: Vec<String> = (0..=MAX_RECIPIENTS).map(|n| format!("r{n}")).collect();
    let mut flags = vec!["--body", "hello", "--subject", "topic"];
    for recipient in &recipients {
        flags.extend(["--to", recipient.as_str()]);
    }

    assert_eq!(
        refused_with(&flags),
        CommsError::TooManyRecipients {
            field: "to".to_string(),
            given: MAX_RECIPIENTS + 1,
            max: MAX_RECIPIENTS,
        }
    );
}

#[test]
fn post_reads_an_empty_optional_flag_as_left_out() {
    let (_, body) = request(&["--body", "hello", "--thread", "", "--subject", "topic"]).expect("accepted");

    assert_eq!(
        body.target,
        PostTarget::NewThread {
            subject: "topic".to_string(),
            to: Vec::new(),
            paths: Vec::new(),
        }
    );
}

#[test]
fn post_refused_flags_are_a_usage_error_and_store_nothing() {
    let fake = FakeAgents::new();

    assert_eq!(
        refusal(&fake, "a", &["post", "--body", "hello"]),
        (
            USAGE,
            format!("agent post: {}", CommsError::InvalidPostTarget { given: Vec::new() })
        )
    );
    assert_eq!(fake.arrivals(), 0);
}

#[test]
fn post_refused_by_the_store_is_a_runtime_error_with_its_fix() {
    let fake = FakeAgents::new();
    text(&fake, "a", &START);

    let (code, message) = refusal(&fake, "c", &["post", "--thread", "t-aaaaaa", "--body", "hello"]);

    assert_eq!(code, ERROR);
    assert_eq!(
        message,
        format!("agent post: {}", CommsError::NotMember { thread: thread() })
    );
    assert!(message.contains("ask a member to invite you"), "{message}");
    assert_eq!(fake.arrivals(), 1);
}

#[test]
fn post_as_a_sub_identity_is_a_distinct_actor() {
    let fake = FakeAgents::new();
    text(
        &fake,
        "a",
        &[
            "post",
            "--subject",
            "topic",
            "--to",
            "b",
            "--body",
            "hello",
            "--as",
            "helper",
        ],
    );

    let unread = fake.inbox(&caller("b"), InboxBody::default()).expect("inbox");
    assert_eq!(unread.messages[0].from.as_str(), "a.helper");
    let own = fake.threads_query(&caller("a"), ThreadQuery::default()).expect("list");
    assert!(own.threads.is_empty(), "the principal is not a member: {own:?}");
}

#[test]
fn post_refuses_a_malformed_sub_identity() {
    assert_eq!(
        refused_with(&["--body", "hello", "--subject", "topic", "--as", "Bad Name"]),
        CommsError::InvalidId(InvalidId::new(IdKind::Sub, "Bad Name"))
    );
}
