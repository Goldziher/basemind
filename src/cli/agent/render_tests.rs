//! The text forms that no store reply reaches by itself: ages, notices, replies and reports.

use super::*;
use crate::cli::agent::test_support::{JSON, TEXT, capture};
use crate::comms::v2::fake::FAKE_EPOCH_MICROS;
use crate::comms::v2::{MessageId, ThreadId};

fn actor(id: &str) -> Actor {
    Actor::parse(id).expect("actor")
}

fn message_id(id: &str) -> MessageId {
    MessageId::parse(id).expect("message id")
}

fn rendered(run: impl FnOnce(&mut dyn Write) -> Result<()>) -> String {
    capture(run).expect("rendered")
}

#[test]
fn age_is_counted_in_the_largest_whole_unit() {
    let ago = |secs: i64| age(FAKE_EPOCH_MICROS - secs * MICROS_PER_SEC, FAKE_EPOCH_MICROS);

    assert_eq!(ago(0), "0s");
    assert_eq!(ago(59), "59s");
    assert_eq!(ago(60), "1m");
    assert_eq!(ago(3_599), "59m");
    assert_eq!(ago(3_600), "1h");
    assert_eq!(ago(86_399), "23h");
    assert_eq!(ago(86_400), "1d");
    assert_eq!(ago(30 * 86_400), "30d");
}

#[test]
fn age_of_a_timestamp_ahead_of_the_clock_is_zero() {
    assert_eq!(age(FAKE_EPOCH_MICROS + MICROS_PER_SEC, FAKE_EPOCH_MICROS), "0s");
    assert_eq!(age(i64::MAX, i64::MIN), "0s");
}

#[test]
fn render_now_reads_the_system_clock() {
    let opts = Render::now(true);

    assert!(opts.json);
    assert!(opts.now_micros > FAKE_EPOCH_MICROS, "{}", opts.now_micros);
}

#[test]
fn post_text_ends_with_the_notices() {
    let reply = PostReply {
        id: message_id("t-aaaaaa.2"),
        thread: ThreadId::parse("t-aaaaaa").expect("thread id"),
        created: false,
        pending: Vec::new(),
        notices: vec!["another session shares this identity".to_string()],
    };

    assert_eq!(
        rendered(|out| post(&reply, &TEXT, out)),
        "t-aaaaaa.2\tposted\nnotice: another session shares this identity\n"
    );
}

#[test]
fn inbox_text_names_the_message_replied_to_and_ends_the_body_once() {
    let reply = InboxReply {
        messages: vec![InboxMessage {
            id: message_id("t-aaaaaa.2"),
            subject: "topic".to_string(),
            from: actor("b"),
            ts_micros: FAKE_EPOCH_MICROS,
            reply_to: Some(message_id("t-aaaaaa.1")),
            body: "two\nlines\n\n".to_string(),
            body_bytes: 11,
            truncated: false,
        }],
        read_to: vec![message_id("t-aaaaaa.2")],
        more: false,
        next_cursor: None,
        notices: vec!["first".to_string(), "second".to_string()],
        wait: WaitOutcome::Arrived,
    };

    assert_eq!(
        rendered(|out| inbox(&reply, &TEXT, out)),
        "t-aaaaaa.2\tb\ttopic\t1m\nreply_to: t-aaaaaa.1\ntwo\nlines\n\nnotice: first\nnotice: second\n"
    );
}

#[test]
fn json_is_pretty_and_ends_with_a_newline() {
    let reply = PostReply {
        id: message_id("t-aaaaaa.1"),
        thread: ThreadId::parse("t-aaaaaa").expect("thread id"),
        created: true,
        pending: vec![actor("b")],
        notices: Vec::new(),
    };

    let output = rendered(|out| post(&reply, &JSON, out));

    assert!(output.starts_with("{\n  \"id\": \"t-aaaaaa.1\",\n"), "{output}");
    assert!(output.ends_with("}\n"), "{output}");
}

#[test]
fn report_text_is_one_line_per_field() {
    let value = serde_json::json!({"threads": 2, "store": "current"});

    assert_eq!(
        rendered(|out| report(&value, &TEXT, out)),
        "threads: 2\nstore: \"current\"\n"
    );
}

#[test]
fn report_json_is_the_value() {
    let value = serde_json::json!({"threads": 2, "store": "current"});

    let output = rendered(|out| report(&value, &JSON, out));

    assert_eq!(serde_json::from_str::<serde_json::Value>(&output).expect("JSON"), value);
}

#[test]
fn report_of_something_that_is_not_a_map_is_one_line() {
    assert_eq!(rendered(|out| report(&7, &TEXT, out)), "7\n");
}
