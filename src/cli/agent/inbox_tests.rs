//! `agent inbox` against the in-memory store: marking and peeking, paging, the injected wait,
//! and what is refused.

use std::cell::RefCell;
use std::time::Duration;

use super::*;
use crate::cli::agent::args::AgentCmd;
use crate::cli::agent::test_support::{TEXT, caller, capture, json, owner, parse, refusal, text};
use crate::cli::exit::{ERROR, USAGE, exit_code_for};
use crate::comms::v2::api::{MAX_INLINE_BODY_BYTES, MAX_WAIT_SECS};
use crate::comms::v2::conformance::ConformanceHarness;
use crate::comms::v2::fake::{FAKE_EPOCH_MICROS, FakeAgents};
use crate::comms::v2::front::Session;
use crate::comms::v2::{Caller, IdKind, InboxBody, InboxReply, InvalidId, Mark, ReadKind, ThreadId, WaitOutcome};

const FIRST: &str = "t-aaaaaa.1\ta\ttopic\t1m\nhello\n\n";
const SECOND: &str = "t-aaaaaa.2\ta\ttopic\t1m\nsecond\n\n";
const THIRD: &str = "t-aaaaaa.3\ta\ttopic\t1m\nthird\n\n";

/// Agent `a` starts an invitation-only thread with `b` and posts `hello`.
fn start(fake: &FakeAgents) {
    text(
        fake,
        "a",
        &["post", "--subject", "topic", "--to", "b", "--body", "hello"],
    );
}

/// [`start`], then two more posts by `a`.
fn start_with_three(fake: &FakeAgents) {
    start(fake);
    for body in ["second", "third"] {
        text(fake, "a", &["post", "--thread", "t-aaaaaa", "--body", body]);
    }
}

fn args(flags: &[&str]) -> InboxArgs {
    let mut line = vec!["inbox"];
    line.extend_from_slice(flags);
    match parse(&line).expect("the command line parses") {
        AgentCmd::Inbox(args) => args,
        other => panic!("not an inbox read: {other:?}"),
    }
}

/// The call the flags after `inbox` make, as agent `b`.
fn request(flags: &[&str]) -> Result<InboxCall, CommsError> {
    inbox_request(args(flags))
}

fn refused_with(flags: &[&str]) -> CommsError {
    request(flags).expect_err("the flags are refused")
}

fn empty_reply(wait: WaitOutcome) -> InboxReply {
    InboxReply {
        messages: Vec::new(),
        read_to: Vec::new(),
        more: false,
        next_cursor: None,
        notices: Vec::new(),
        wait,
    }
}

#[test]
fn inbox_prints_one_block_per_message_and_marks_it_read() {
    let fake = FakeAgents::new();
    start(&fake);

    assert_eq!(text(&fake, "b", &["inbox"]), FIRST);
    assert_eq!(text(&fake, "b", &["inbox"]), "(no messages)\n");
}

#[test]
fn inbox_peek_leaves_the_message_unread() {
    let fake = FakeAgents::new();
    start(&fake);

    assert_eq!(text(&fake, "b", &["inbox", "--peek"]), FIRST);
    assert_eq!(text(&fake, "b", &["inbox", "--peek"]), FIRST);
    assert_eq!(text(&fake, "b", &["inbox"]), FIRST);
    assert_eq!(text(&fake, "b", &["inbox", "--peek"]), "(no messages)\n");
}

#[test]
fn inbox_that_marks_continues_from_the_read_position() {
    let fake = FakeAgents::new();
    start_with_three(&fake);

    assert_eq!(
        text(&fake, "b", &["inbox", "--limit", "1"]),
        format!("{FIRST}more: call again for the next unread messages\n")
    );
    assert_eq!(text(&fake, "b", &["inbox"]), format!("{SECOND}{THIRD}"));
}

#[test]
fn inbox_peek_pages_with_the_cursor() {
    let fake = FakeAgents::new();
    start_with_three(&fake);

    let page = json(&fake, "b", &["inbox", "--peek", "--limit", "2"]);
    let cursor = page["next_cursor"].as_str().expect("a cursor for the next page");

    assert_eq!(page["messages"].as_array().map(Vec::len), Some(2));
    assert_eq!(page["more"], true);
    assert_eq!(
        text(&fake, "b", &["inbox", "--peek", "--limit", "2"]),
        format!("{FIRST}{SECOND}next_cursor: {cursor}\n")
    );
    assert_eq!(
        text(&fake, "b", &["inbox", "--peek", "--limit", "2", "--cursor", cursor]),
        THIRD
    );
}

#[test]
fn inbox_history_returns_own_posts_and_marks_nothing() {
    let fake = FakeAgents::new();
    start_with_three(&fake);

    assert_eq!(
        text(&fake, "a", &["inbox", "--unread", "false", "--thread", "t-aaaaaa"]),
        format!("{FIRST}{SECOND}{THIRD}")
    );
    assert_eq!(
        text(&fake, "b", &["inbox", "--unread", "false"]),
        format!("{FIRST}{SECOND}{THIRD}")
    );
    assert_eq!(text(&fake, "b", &["inbox"]), format!("{FIRST}{SECOND}{THIRD}"));
}

#[test]
fn inbox_cuts_a_long_body_and_reads_it_whole_by_id() {
    let fake = FakeAgents::new();
    let body = "x".repeat(MAX_INLINE_BODY_BYTES + 1);
    text(
        &fake,
        "a",
        &["post", "--subject", "topic", "--to", "b", "--body", body.as_str()],
    );

    let whole = json(&fake, "b", &["inbox", "--id", "t-aaaaaa.1"]);
    assert_eq!(whole["messages"][0]["body"].as_str().map(str::len), Some(body.len()));
    assert_eq!(whole["messages"][0]["truncated"], false);
    assert_eq!(
        text(&fake, "b", &["inbox"]),
        format!(
            "t-aaaaaa.1\ta\ttopic\t1m\n{}\n(truncated: read the whole body with `basemind agent inbox --id t-aaaaaa.1`)\n\n",
            "x".repeat(MAX_INLINE_BODY_BYTES)
        )
    );
}

#[test]
fn inbox_json_is_the_reply() {
    let fake = FakeAgents::new();
    start(&fake);
    let peek = InboxBody {
        mark: Mark::Peek,
        ..InboxBody::default()
    };
    let expected = fake.inbox(&caller("b"), peek).expect("peek");

    let peeked = json(&fake, "b", &["inbox", "--peek"]);
    assert_eq!(peeked, serde_json::to_value(&expected).expect("serializable"));
    assert_eq!(peeked["messages"][0]["id"], "t-aaaaaa.1");
    assert_eq!(peeked["messages"][0]["from"], "a");
    assert_eq!(peeked["read_to"], serde_json::json!([]));
    assert_eq!(peeked["wait"], "not_waited");

    let read = json(&fake, "b", &["inbox"]);
    assert_eq!(read["read_to"], serde_json::json!(["t-aaaaaa.1"]));
}

#[test]
fn inbox_as_a_sibling_reads_the_principals_mail_at_its_own_position() {
    let fake = FakeAgents::new();
    start_with_three(&fake);

    assert_eq!(
        text(&fake, "b", &["inbox", "--limit", "1"]),
        format!("{FIRST}more: call again for the next unread messages\n")
    );
    assert_eq!(
        text(&fake, "b", &["inbox", "--as", "helper"]),
        format!("{SECOND}{THIRD}"),
        "a sibling is a member through its principal and starts where the principal has read to"
    );
    assert_eq!(text(&fake, "b", &["inbox", "--as", "helper"]), "(no messages)\n");
    assert_eq!(
        text(&fake, "b", &["inbox"]),
        format!("{SECOND}{THIRD}"),
        "the sibling's read did not move the principal's position"
    );
}

#[test]
fn inbox_announce_lists_front_matter_once_and_marks_nothing_read() {
    let fake = FakeAgents::new();
    start_with_three(&fake);
    let marks = || fake.position(&ThreadId::parse("t-aaaaaa").expect("thread id"), &caller("b").actor);

    assert_eq!(
        text(&fake, "b", &["inbox", "--announce", "--limit", "2"]),
        "t-aaaaaa.1\ta\ttopic\t1m\n(5 bytes, not read yet)\n\n\
         t-aaaaaa.2\ta\ttopic\t1m\n(6 bytes, not read yet)\n\n\
         more: call again for the next unread messages\n"
    );
    assert_eq!(marks(), Some((0, 2)), "only the announcement mark moved");

    let rest = json(&fake, "b", &["inbox", "--announce"]);
    assert_eq!(
        rest["messages"],
        serde_json::json!([{
            "id": "t-aaaaaa.3",
            "subject": "topic",
            "from": "a",
            "ts_micros": FAKE_EPOCH_MICROS + 2,
            "reply_to": null,
            "body": "",
            "body_bytes": 5,
            "truncated": false,
        }])
    );
    assert_eq!(rest["read_to"], serde_json::json!([]));
    assert_eq!(rest["more"], false);
    assert_eq!(text(&fake, "b", &["inbox", "--announce"]), "(no messages)\n");
    assert_eq!(marks(), Some((0, 3)));

    assert_eq!(text(&fake, "b", &["inbox"]), format!("{FIRST}{SECOND}{THIRD}"));
    assert_eq!(marks(), Some((3, 3)));
}

#[test]
fn inbox_refuses_announce_beside_a_flag_that_asks_for_another_read() {
    for (extra, field) in [
        (&["--id", "t-aaaaaa.1"][..], "id"),
        (&["--peek"][..], "peek"),
        (&["--unread", "false"][..], "unread"),
        (&["--wait-secs", "5"][..], "wait_secs"),
    ] {
        let flags = [&["--announce"][..], extra].concat();

        assert_eq!(refused_with(&flags), CommsError::not_applicable(field, "announce"));
    }
    assert_eq!(
        refused_with(&["--announce", "--cursor", "abc"]),
        CommsError::CursorNotApplicable
    );

    let announcing = request(&["--announce", "--thread", "t-aaaaaa", "--unread", "true"]).expect("accepted");
    assert_eq!(announcing.body.kind(), Ok(ReadKind::Announcing));
    assert!(announcing.wait.is_zero());
}

#[test]
fn inbox_prints_the_sessions_notices_after_the_replys() {
    let fake = FakeAgents::new();
    start(&fake);
    let own = ["2 live sessions share the identity b".to_string()];
    let owner = owner("b");
    let session = Session {
        notices: &own,
        ..owner.session()
    };
    let wait = |caller: &Caller, body: InboxBody, _: Duration| -> Result<InboxReply, CommsError> {
        let mut reply = fake.inbox(caller, body)?;
        reply.notices.push("from the daemon".to_string());
        Ok(reply)
    };

    let output = capture(|out| run_inbox(&fake, &session, args(&["--wait-secs", "1"]), &wait, &TEXT, out));

    assert_eq!(
        output.expect("read"),
        format!("{FIRST}notice: from the daemon\nnotice: {}\n", own[0])
    );
}

#[test]
fn inbox_refuses_fields_beside_an_id() {
    let by_id = ["--id", "t-aaaaaa.1"];
    for (extra, field) in [
        (&["--thread", "t-aaaaaa"][..], "thread"),
        (&["--limit", "5"][..], "limit"),
        (&["--peek"][..], "peek"),
        (&["--unread", "false"][..], "unread"),
        (&["--cursor", "abc"][..], "cursor"),
        (&["--wait-secs", "5"][..], "wait_secs"),
    ] {
        let flags = [&by_id[..], extra].concat();

        assert_eq!(refused_with(&flags), CommsError::not_applicable(field, "id"));
    }
}

#[test]
fn inbox_refuses_a_cursor_on_a_read_that_marks() {
    assert_eq!(refused_with(&["--cursor", "abc"]), CommsError::CursorNotApplicable);
}

#[test]
fn inbox_refuses_a_wait_out_of_range_or_with_nothing_to_wait_for() {
    assert_eq!(
        refused_with(&["--wait-secs", "41"]),
        CommsError::WaitOutOfRange {
            given: MAX_WAIT_SECS + 1,
            max: MAX_WAIT_SECS,
        }
    );
    assert_eq!(
        refused_with(&["--wait-secs", "5", "--unread", "false"]),
        CommsError::not_applicable("wait_secs", "unread")
    );
}

#[test]
fn inbox_refuses_malformed_ids() {
    assert_eq!(
        refused_with(&["--thread", "nope"]),
        CommsError::InvalidId(InvalidId::new(IdKind::Thread, "nope"))
    );
    assert_eq!(
        refused_with(&["--id", "t-aaaaaa"]),
        CommsError::InvalidId(InvalidId::new(IdKind::Message, "t-aaaaaa"))
    );
}

#[test]
fn inbox_accepts_defaults_passed_explicitly() {
    let by_id = request(&["--id", "t-aaaaaa.1", "--unread", "true", "--wait-secs", "0"]).expect("accepted");
    assert_eq!(by_id.body.kind(), Ok(ReadKind::ById));
    assert!(by_id.wait.is_zero());

    let history = request(&["--peek", "--unread", "false"]).expect("accepted");
    assert_eq!(history.body.kind(), Ok(ReadKind::History));

    let longest = request(&["--peek", "--wait-secs", "40", "--thread", "t-aaaaaa"]).expect("accepted");
    assert_eq!(longest.wait, Duration::from_secs(40));
    assert_eq!(longest.body.kind(), Ok(ReadKind::Peek));
    assert_eq!(
        longest.body.thread(),
        Some(&ThreadId::parse("t-aaaaaa").expect("thread id"))
    );
}

#[test]
fn inbox_refused_flags_are_a_usage_error() {
    let fake = FakeAgents::new();

    assert_eq!(
        refusal(&fake, "b", &["inbox", "--cursor", "abc"]),
        (USAGE, format!("agent inbox: {}", CommsError::CursorNotApplicable))
    );
}

#[test]
fn inbox_refused_by_the_store_is_a_runtime_error_with_its_fix() {
    let fake = FakeAgents::new();
    start(&fake);

    let (code, message) = refusal(&fake, "c", &["inbox", "--thread", "t-aaaaaa"]);
    assert_eq!(code, ERROR);
    assert!(
        message.starts_with("agent inbox: not a member of thread t-aaaaaa"),
        "{message}"
    );

    let (code, message) = refusal(&fake, "b", &["inbox", "--peek", "--cursor", "abc"]);
    assert_eq!(code, ERROR);
    assert!(message.contains("repeat the call without `cursor`"), "{message}");
}

#[test]
fn inbox_without_a_wait_never_calls_the_waiting_read() {
    let fake = FakeAgents::new();
    start(&fake);
    let waits = RefCell::new(0u32);
    let wait = |_: &Caller, _: InboxBody, _: Duration| -> Result<InboxReply, CommsError> {
        *waits.borrow_mut() += 1;
        Ok(empty_reply(WaitOutcome::TimedOut))
    };

    for flags in [&["--peek"][..], &["--peek", "--wait-secs", "0"][..]] {
        let output = capture(|out| run_inbox(&fake, &owner("b").session(), args(flags), &wait, &TEXT, out));

        assert_eq!(output.expect("read"), FIRST);
    }
    assert_eq!(*waits.borrow(), 0);
}

#[test]
fn inbox_with_a_wait_hands_the_whole_read_to_the_waiting_read() {
    let fake = FakeAgents::new();
    start(&fake);
    let asked = RefCell::new(Vec::new());
    let wait = |caller: &Caller, body: InboxBody, wait: Duration| -> Result<InboxReply, CommsError> {
        asked.borrow_mut().push((caller.actor.to_string(), body.clone(), wait));
        let mut reply = fake.inbox(caller, body)?;
        reply.wait = WaitOutcome::Arrived;
        Ok(reply)
    };

    let output = capture(|out| {
        run_inbox(
            &fake,
            &owner("b").session(),
            args(&["--wait-secs", "5", "--as", "helper", "--peek"]),
            &wait,
            &TEXT,
            out,
        )
    });

    assert_eq!(output.expect("read"), FIRST);
    let peek = InboxBody {
        mark: Mark::Peek,
        ..InboxBody::default()
    };
    assert_eq!(
        asked.into_inner(),
        [("b.helper".to_string(), peek, Duration::from_secs(5))]
    );
}

#[test]
fn inbox_reports_a_wait_that_timed_out() {
    let fake = FakeAgents::new();
    let wait = |_: &Caller, _: InboxBody, _: Duration| -> Result<InboxReply, CommsError> {
        Ok(empty_reply(WaitOutcome::TimedOut))
    };

    let output = capture(|out| {
        run_inbox(
            &fake,
            &owner("b").session(),
            args(&["--wait-secs", "40"]),
            &wait,
            &TEXT,
            out,
        )
    });

    assert_eq!(output.expect("read"), "(no messages; the wait timed out)\n");
}

#[test]
fn inbox_wait_out_of_range_is_refused_before_anything_waits() {
    let fake = FakeAgents::new();
    let waits = RefCell::new(0u32);
    let wait = |_: &Caller, _: InboxBody, _: Duration| -> Result<InboxReply, CommsError> {
        *waits.borrow_mut() += 1;
        Ok(empty_reply(WaitOutcome::TimedOut))
    };

    let error = capture(|out| {
        run_inbox(
            &fake,
            &owner("b").session(),
            args(&["--wait-secs", "41"]),
            &wait,
            &TEXT,
            out,
        )
    })
    .expect_err("refused");

    assert_eq!(exit_code_for(&error), USAGE);
    assert_eq!(
        error.to_string(),
        "agent inbox: `wait_secs` is 41, above the maximum of 40: wait at most 40 seconds and call again"
    );
    assert_eq!(*waits.borrow(), 0);
}

#[test]
fn inbox_surfaces_a_failure_of_the_waiting_read() {
    let fake = FakeAgents::new();

    let (code, message) = refusal(&fake, "b", &["inbox", "--wait-secs", "5"]);

    assert_eq!(code, ERROR);
    assert!(message.starts_with("agent inbox: the comms store failed"), "{message}");
}
