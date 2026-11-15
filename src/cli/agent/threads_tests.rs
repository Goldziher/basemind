//! `agent threads` against the in-memory store: listing, discovery, the four changes, paging,
//! and what is refused.

use super::*;
use crate::cli::agent::args::AgentCmd;
use crate::cli::agent::test_support::{caller, json, parse, refusal, text};
use crate::cli::exit::{ERROR, USAGE};
use crate::comms::v2::api::MIN_QUERY_CHARS;
use crate::comms::v2::fake::{FAKE_EPOCH_MICROS, FakeAgents};
use crate::comms::v2::{Actor, Cursor, IdKind, InvalidId, ThreadFilter, ThreadId, ThreadUpdate};
use crate::path::RelPath;

/// The open thread of [`open`], as its creator `a` sees it.
const OPEN_FOR_CREATOR: &str = "t-aaaaaa\ttopic\topen\tmember\tunread 0\t1m\t-\n";

/// Agent `a` starts a thread about `src/comms` that is open to the repository.
fn open(fake: &FakeAgents) {
    text(
        fake,
        "a",
        &["post", "--subject", "topic", "--paths", "src/comms", "--body", "hello"],
    );
}

/// Agent `a` starts an invitation-only thread with `b`.
fn invitation_only(fake: &FakeAgents) {
    text(
        fake,
        "a",
        &["post", "--subject", "topic", "--to", "b", "--body", "hello"],
    );
}

fn thread() -> ThreadId {
    ThreadId::parse("t-aaaaaa").expect("thread id")
}

/// The call the flags after `threads` make, as agent `a`.
fn request(flags: &[&str]) -> Result<ThreadsCall, CommsError> {
    let mut line = vec!["threads"];
    line.extend_from_slice(flags);
    match parse(&line).expect("the command line parses") {
        AgentCmd::Threads(args) => threads_request(args),
        other => panic!("not a threads call: {other:?}"),
    }
}

fn refused_with(flags: &[&str]) -> CommsError {
    request(flags).expect_err("the flags are refused")
}

#[test]
fn threads_lists_one_thread_per_line() {
    let fake = FakeAgents::new();
    invitation_only(&fake);

    assert_eq!(
        text(&fake, "a", &["threads"]),
        "t-aaaaaa\ttopic\tinvite-only\tmember\tunread 0\t1m\tb\n"
    );
    assert_eq!(
        text(&fake, "b", &["threads"]),
        "t-aaaaaa\ttopic\tinvite-only\tmember\tunread 1\t1m\ta\n"
    );
    assert_eq!(text(&fake, "c", &["threads"]), "(no threads)\n");
}

#[test]
fn threads_describes_one_thread() {
    let fake = FakeAgents::new();
    open(&fake);

    assert_eq!(
        text(&fake, "a", &["threads", "--thread", "t-aaaaaa"]),
        format!("{OPEN_FOR_CREATOR}repo: github.com/acme/app\ncreator: a\npaths: src/comms\nhead: 1\n")
    );
}

#[test]
fn threads_discovers_open_threads_by_query_and_path() {
    let fake = FakeAgents::new();
    open(&fake);
    let found = "t-aaaaaa\ttopic\topen\tnot-member\tunread 0\t1m\ta\n";

    assert_eq!(text(&fake, "b", &["threads"]), "(no threads)\n");
    assert_eq!(text(&fake, "b", &["threads", "--query", "TOPIC"]), found);
    assert_eq!(text(&fake, "b", &["threads", "--path", "src/comms/store.rs"]), found);
    assert_eq!(text(&fake, "b", &["threads", "--path", "docs"]), "(no threads)\n");
    assert_eq!(
        text(&fake, "b", &["threads", "--query", "topic", "--path", "docs"]),
        "(no threads)\n"
    );
}

#[test]
fn threads_join_then_leave() {
    let fake = FakeAgents::new();
    open(&fake);

    assert_eq!(
        text(&fake, "b", &["threads", "--thread", "t-aaaaaa", "--join"]),
        "joined t-aaaaaa\nt-aaaaaa\ttopic\topen\tmember\tunread 0\t1m\ta\n"
    );
    assert_eq!(
        text(&fake, "b", &["threads", "--thread", "t-aaaaaa", "--leave"]),
        "left t-aaaaaa\nt-aaaaaa\ttopic\topen\tnot-member\tunread 0\t1m\ta\n"
    );
}

#[test]
fn threads_invite_lists_the_agents_not_seen() {
    let fake = FakeAgents::new();
    open(&fake);

    assert_eq!(
        text(
            &fake,
            "a",
            &["threads", "--thread", "t-aaaaaa", "--invite", "c", "--invite", "d"]
        ),
        "invited into t-aaaaaa\nt-aaaaaa\ttopic\topen\tmember\tunread 0\t1m\tc, d\npending: c, d\n"
    );
    assert_eq!(
        text(&fake, "c", &["threads"]),
        "t-aaaaaa\ttopic\topen\tmember\tunread 0\t1m\ta, d\n"
    );
}

#[test]
fn threads_archive_hides_the_thread_from_the_default_listing() {
    let fake = FakeAgents::new();
    open(&fake);
    let archived = "t-aaaaaa\ttopic\topen,archived\tmember\tunread 0\t1m\t-\n";

    assert_eq!(
        text(&fake, "a", &["threads", "--thread", "t-aaaaaa", "--archive"]),
        format!("archived t-aaaaaa\n{archived}")
    );
    assert_eq!(text(&fake, "a", &["threads"]), "(no threads)\n");
    assert_eq!(text(&fake, "a", &["threads", "--archived"]), archived);
}

#[test]
fn threads_archive_by_another_agent_is_a_runtime_error_with_its_fix() {
    let fake = FakeAgents::new();
    open(&fake);

    let (code, message) = refusal(&fake, "b", &["threads", "--thread", "t-aaaaaa", "--archive"]);

    assert_eq!(code, ERROR);
    assert_eq!(
        message,
        format!("agent threads: {}", CommsError::NotCreator { thread: thread() })
    );
    assert!(message.contains("leave it with agent_threads"), "{message}");
    assert_eq!(text(&fake, "a", &["threads"]), OPEN_FOR_CREATOR);
}

#[test]
fn threads_pages_a_listing_with_the_cursor() {
    let fake = FakeAgents::new();
    for subject in ["one", "two", "three"] {
        text(&fake, "a", &["post", "--subject", subject, "--body", "hello"]);
    }

    let page = json(&fake, "a", &["threads", "--limit", "2"]);
    let cursor = page["next_cursor"].as_str().expect("a cursor for the next page");

    assert_eq!(page["threads"][0]["id"], "t-aaaaac");
    assert_eq!(page["threads"][1]["id"], "t-aaaaab");
    assert_eq!(page["threads"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        text(&fake, "a", &["threads", "--limit", "2"]),
        format!(
            "t-aaaaac\tthree\topen\tmember\tunread 0\t1m\t-\n\
             t-aaaaab\ttwo\topen\tmember\tunread 0\t1m\t-\n\
             next_cursor: {cursor}\n"
        )
    );
    assert_eq!(
        text(&fake, "a", &["threads", "--limit", "2", "--cursor", cursor]),
        "t-aaaaaa\tone\topen\tmember\tunread 0\t1m\t-\n"
    );
}

#[test]
fn threads_json_is_the_reply() {
    let fake = FakeAgents::new();
    open(&fake);
    let expected = fake.threads_query(&caller("a"), ThreadQuery::default()).expect("list");

    let listed = json(&fake, "a", &["threads"]);
    assert_eq!(listed, serde_json::to_value(&expected).expect("serializable"));
    assert_eq!(listed["threads"][0]["id"], "t-aaaaaa");
    assert_eq!(listed["threads"][0]["paths"], serde_json::json!(["src/comms"]));
    assert_eq!(listed["threads"][0]["last_activity_micros"], FAKE_EPOCH_MICROS);

    let joined = json(&fake, "b", &["threads", "--thread", "t-aaaaaa", "--join"]);
    assert_eq!(joined["threads"][0]["member"], true);
    assert_eq!(joined["threads"][0]["peers"], serde_json::json!(["a"]));
}

#[test]
fn threads_as_a_sub_identity_lists_its_principals_threads_with_its_own_unread_count() {
    let fake = FakeAgents::new();
    text(
        &fake,
        "a",
        &["post", "--subject", "topic", "--body", "hello", "--as", "helper"],
    );

    assert_eq!(
        text(&fake, "a", &["threads"]),
        "t-aaaaaa\ttopic\topen\tmember\tunread 1\t1m\t-\n"
    );
    assert_eq!(text(&fake, "a", &["threads", "--as", "helper"]), OPEN_FOR_CREATOR);
}

#[test]
fn threads_archived_is_read_by_naming_it_and_closed_to_posts_joins_and_invitations() {
    let fake = FakeAgents::new();
    open(&fake);
    text(&fake, "b", &["threads", "--thread", "t-aaaaaa", "--join"]);
    text(&fake, "a", &["post", "--thread", "t-aaaaaa", "--body", "second"]);
    text(&fake, "a", &["threads", "--thread", "t-aaaaaa", "--archive"]);
    let archived = format!("agent threads: {}", CommsError::ThreadArchived { thread: thread() });

    assert_eq!(text(&fake, "b", &["inbox"]), "(no messages)\n");
    assert_eq!(
        text(&fake, "b", &["inbox", "--thread", "t-aaaaaa"]),
        "t-aaaaaa.2\ta\ttopic\t1m\nsecond\n\n"
    );
    assert_eq!(
        refusal(&fake, "b", &["post", "--thread", "t-aaaaaa", "--body", "too late"]),
        (
            ERROR,
            "agent post: thread t-aaaaaa is archived: read it with agent_inbox `thread`, or start a new thread by \
             posting with `subject`"
                .to_string()
        )
    );
    assert_eq!(
        refusal(&fake, "c", &["threads", "--thread", "t-aaaaaa", "--join"]),
        (ERROR, archived.clone())
    );
    assert_eq!(
        refusal(&fake, "b", &["threads", "--thread", "t-aaaaaa", "--invite", "c"]),
        (ERROR, archived)
    );
    assert_eq!(
        text(&fake, "b", &["threads", "--thread", "t-aaaaaa", "--leave"]),
        "left t-aaaaaa\nt-aaaaaa\ttopic\topen,archived\tnot-member\tunread 0\t1m\ta\n"
    );
}

#[test]
fn threads_builds_each_kind_of_call() {
    assert_eq!(request(&[]), Ok(ThreadsCall::Query(ThreadQuery::default())));
    assert_eq!(
        request(&["--thread", "t-aaaaaa"]),
        Ok(ThreadsCall::Query(ThreadQuery::Describe(thread())))
    );
    assert_eq!(
        request(&[
            "--query",
            "topic",
            "--path",
            "src/",
            "--archived",
            "--limit",
            "5",
            "--cursor",
            "abc"
        ]),
        Ok(ThreadsCall::Query(ThreadQuery::List(ThreadFilter {
            query: Some("topic".to_string()),
            path: Some(RelPath::from("src")),
            archived: true,
            limit: Some(5),
            cursor: Some(Cursor::from_wire("abc")),
        })))
    );
    for (flag, action) in [
        ("--join", ThreadAction::Join),
        ("--leave", ThreadAction::Leave),
        ("--archive", ThreadAction::Archive),
    ] {
        assert_eq!(
            request(&["--thread", "t-aaaaaa", flag]),
            Ok(ThreadsCall::Update(ThreadUpdate {
                thread: thread(),
                action,
            }))
        );
    }
    assert_eq!(
        request(&["--thread", "t-aaaaaa", "--invite", "b", "--invite", "c.helper"]),
        Ok(ThreadsCall::Update(ThreadUpdate {
            thread: thread(),
            action: ThreadAction::Invite(vec![
                Actor::parse("b").expect("actor"),
                Actor::parse("c.helper").expect("actor"),
            ]),
        }))
    );
}

#[test]
fn threads_refuses_more_than_one_change() {
    assert_eq!(
        refused_with(&["--thread", "t-aaaaaa", "--join", "--leave"]),
        CommsError::MultipleUpdates {
            fields: vec!["join".to_string(), "leave".to_string()],
        }
    );
    assert_eq!(
        refused_with(&["--thread", "t-aaaaaa", "--invite", "b", "--archive"]),
        CommsError::MultipleUpdates {
            fields: vec!["invite".to_string(), "archive".to_string()],
        }
    );
}

#[test]
fn threads_refuses_a_change_without_a_thread() {
    for change in ["join", "leave", "archive"] {
        let flag = format!("--{change}");

        assert_eq!(
            refused_with(&[flag.as_str()]),
            CommsError::MissingField {
                field: "thread".to_string(),
                needed_by: change.to_string(),
            }
        );
    }
    assert_eq!(
        refused_with(&["--invite", "b"]),
        CommsError::MissingField {
            field: "thread".to_string(),
            needed_by: "invite".to_string(),
        }
    );
}

#[test]
fn threads_refuses_listing_flags_beside_a_change_or_a_description() {
    assert_eq!(
        refused_with(&["--thread", "t-aaaaaa", "--join", "--limit", "5"]),
        CommsError::not_applicable("limit", "join")
    );
    assert_eq!(
        refused_with(&["--thread", "t-aaaaaa", "--archive", "--archived"]),
        CommsError::not_applicable("archived", "archive")
    );
    assert_eq!(
        refused_with(&["--thread", "t-aaaaaa", "--query", "topic"]),
        CommsError::not_applicable("query", "thread")
    );
    assert_eq!(
        refused_with(&["--thread", "t-aaaaaa", "--path", "src"]),
        CommsError::not_applicable("path", "thread")
    );
    assert_eq!(
        refused_with(&["--thread", "t-aaaaaa", "--cursor", "abc"]),
        CommsError::not_applicable("cursor", "thread")
    );
}

#[test]
fn threads_refuses_malformed_values() {
    assert_eq!(
        refused_with(&["--query", "ab"]),
        CommsError::QueryTooShort { min: MIN_QUERY_CHARS }
    );
    assert!(matches!(
        refused_with(&["--path", "../up"]),
        CommsError::InvalidPath { .. }
    ));
    assert_eq!(
        refused_with(&["--thread", "nope"]),
        CommsError::InvalidId(InvalidId::new(IdKind::Thread, "nope"))
    );
    assert_eq!(
        refused_with(&["--thread", "t-aaaaaa", "--invite", "not an id"]),
        CommsError::InvalidId(InvalidId::new(IdKind::Actor, "not an id"))
    );
}

#[test]
fn threads_refused_flags_are_a_usage_error() {
    let fake = FakeAgents::new();

    assert_eq!(
        refusal(&fake, "a", &["threads", "--join"]),
        (
            USAGE,
            "agent threads: `join` needs `thread`: pass `thread` as well".to_string()
        )
    );
}
