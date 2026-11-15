//! The grammar: every flag parses into its field, and what is not a flag is refused.

use clap::error::ErrorKind;

use super::*;
use crate::cli::agent::test_support::parse;

fn some(value: &str) -> Option<String> {
    Some(value.to_string())
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(ToString::to_string).collect()
}

#[test]
fn post_takes_every_field_of_agent_post() {
    let cmd = parse(&[
        "post",
        "--body",
        "hello",
        "--thread",
        "t-aaaaaa",
        "--reply-to",
        "t-aaaaaa.1",
        "--subject",
        "topic",
        "--to",
        "b",
        "--to",
        "c",
        "--paths",
        "src/comms",
        "--paths",
        "docs",
        "--as",
        "helper",
    ])
    .expect("parses");

    assert_eq!(
        cmd,
        AgentCmd::Post(PostArgs {
            body: "hello".to_string(),
            thread: some("t-aaaaaa"),
            reply_to: some("t-aaaaaa.1"),
            subject: some("topic"),
            to: strings(&["b", "c"]),
            paths: strings(&["src/comms", "docs"]),
            as_name: some("helper"),
        })
    );
}

#[test]
fn post_with_a_body_alone_leaves_every_other_field_out() {
    assert_eq!(
        parse(&["post", "--body", "hello"]).expect("parses"),
        AgentCmd::Post(PostArgs {
            body: "hello".to_string(),
            ..PostArgs::default()
        })
    );
}

#[test]
fn post_without_a_body_is_refused() {
    let error = parse(&["post", "--subject", "topic"]).expect_err("a body is required");

    assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn post_body_may_start_with_a_hyphen() {
    assert_eq!(
        parse(&["post", "--body", "- done", "--subject", "topic"]).expect("parses"),
        AgentCmd::Post(PostArgs {
            body: "- done".to_string(),
            subject: some("topic"),
            ..PostArgs::default()
        })
    );
}

#[test]
fn post_has_no_idempotency_key() {
    let error = parse(&["post", "--body", "hello", "--idempotency-key", "k"]).expect_err("no such flag");

    assert_eq!(error.kind(), ErrorKind::UnknownArgument);
}

#[test]
fn inbox_takes_every_field_of_agent_inbox() {
    let cmd = parse(&[
        "inbox",
        "--thread",
        "t-aaaaaa",
        "--id",
        "t-aaaaaa.1",
        "--unread",
        "false",
        "--peek",
        "--announce",
        "--wait-secs",
        "40",
        "--limit",
        "5",
        "--cursor",
        "abc",
        "--as",
        "helper",
    ])
    .expect("parses");

    assert_eq!(
        cmd,
        AgentCmd::Inbox(InboxArgs {
            thread: some("t-aaaaaa"),
            id: some("t-aaaaaa.1"),
            unread: Some(false),
            peek: true,
            announce: true,
            wait_secs: Some(40),
            limit: Some(5),
            cursor: some("abc"),
            as_name: some("helper"),
        })
    );
}

#[test]
fn inbox_without_flags_leaves_every_field_out() {
    assert_eq!(
        parse(&["inbox"]).expect("parses"),
        AgentCmd::Inbox(InboxArgs::default())
    );
}

#[test]
fn inbox_unread_takes_true_or_false() {
    assert_eq!(
        parse(&["inbox", "--unread", "true"]).expect("parses"),
        AgentCmd::Inbox(InboxArgs {
            unread: Some(true),
            ..InboxArgs::default()
        })
    );
    assert!(parse(&["inbox", "--unread", "maybe"]).is_err());
    assert!(parse(&["inbox", "--unread"]).is_err());
}

#[test]
fn inbox_numbers_must_be_unsigned_integers() {
    assert!(parse(&["inbox", "--wait-secs", "soon"]).is_err());
    assert!(parse(&["inbox", "--wait-secs", "-1"]).is_err());
    assert!(parse(&["inbox", "--limit", "many"]).is_err());
}

#[test]
fn threads_takes_every_field_of_agent_threads() {
    let cmd = parse(&[
        "threads",
        "--query",
        "topic",
        "--path",
        "src/comms",
        "--thread",
        "t-aaaaaa",
        "--join",
        "--leave",
        "--invite",
        "b",
        "--invite",
        "c",
        "--archive",
        "--archived",
        "--limit",
        "5",
        "--cursor",
        "abc",
        "--as",
        "helper",
    ])
    .expect("parses");

    assert_eq!(
        cmd,
        AgentCmd::Threads(ThreadsArgs {
            query: some("topic"),
            path: some("src/comms"),
            thread: some("t-aaaaaa"),
            join: true,
            leave: true,
            invite: strings(&["b", "c"]),
            archive: true,
            archived: true,
            limit: Some(5),
            cursor: some("abc"),
            as_name: some("helper"),
        })
    );
}

#[test]
fn threads_without_flags_leaves_every_field_out() {
    assert_eq!(
        parse(&["threads"]).expect("parses"),
        AgentCmd::Threads(ThreadsArgs::default())
    );
}

#[test]
fn status_and_cleanup_take_no_flags() {
    assert_eq!(parse(&["status"]).expect("parses"), AgentCmd::Status);
    assert_eq!(parse(&["cleanup"]).expect("parses"), AgentCmd::Cleanup);
    assert!(parse(&["status", "--as", "helper"]).is_err());
    assert!(parse(&["cleanup", "--apply"]).is_err());
}

#[test]
fn removed_operations_are_not_commands() {
    for removed in ["register", "ack", "remove-member", "archive", "join"] {
        let error = parse(&[removed]).expect_err("not a command");

        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand, "{removed}");
    }
}

#[test]
fn json_is_the_global_flag_of_the_binary_and_not_one_of_the_group() {
    let error = parse(&["threads", "--json"]).expect_err("--json is parsed by the binary");

    assert_eq!(error.kind(), ErrorKind::UnknownArgument);
}
