use super::*;
use crate::comms::v2::ids::IdKind;

fn thread(s: &str) -> ThreadId {
    ThreadId::parse(s).expect("thread id")
}

fn through_msgpack(error: &CommsError) -> CommsError {
    let packed = rmp_serde::to_vec_named(error).expect("msgpack");
    rmp_serde::from_slice(&packed).expect("error")
}

fn every_known_variant() -> Vec<CommsError> {
    let id = thread("t-aaaaab");
    let message = MessageId::new(id.clone(), 3).expect("message id");
    vec![
        CommsError::from(InvalidId::new(IdKind::Sub, "Bad")),
        CommsError::ThreadNotFound { thread: id.clone() },
        CommsError::MessageNotFound { id: message },
        CommsError::NotMember { thread: id.clone() },
        CommsError::ThreadClosed { thread: id.clone() },
        CommsError::NotCreator { thread: id.clone() },
        CommsError::ThreadArchived { thread: id },
        CommsError::MultipleUpdates {
            fields: vec!["join".to_string(), "archive".to_string()],
        },
        CommsError::InvalidPostTarget { given: Vec::new() },
        CommsError::CreationOnly {
            field: "to".to_string(),
        },
        CommsError::MissingField {
            field: "thread".to_string(),
            needed_by: "join".to_string(),
        },
        CommsError::not_applicable("query", "join"),
        CommsError::CursorNotApplicable,
        CommsError::QueryTooShort { min: 3 },
        CommsError::MemberCapReached {
            thread: thread("t-aaaaac"),
            cap: 256,
        },
        CommsError::StoreNewer {
            store: 1001,
            binary: 1000,
        },
        CommsError::WaitOutOfRange { given: 41, max: 40 },
        CommsError::invalid_cursor("nope"),
        CommsError::EmptyBody,
        CommsError::BodyTooLarge {
            len: 65_537,
            max: 16_384,
        },
        CommsError::InvalidSubject { max: 200 },
        CommsError::invalid_path(&RelPath::from("../other")),
        CommsError::TooManyRecipients {
            field: "to".to_string(),
            given: 33,
            max: 32,
        },
        CommsError::TooManyPaths { given: 17, max: 16 },
        CommsError::OnlySelfRecipient,
        CommsError::EmptyInvite,
        CommsError::Backend {
            detail: "disk full".to_string(),
        },
    ]
}

#[test]
fn error_carries_its_code_fields_and_rendered_message() {
    let not_member = CommsError::NotMember {
        thread: thread("t-aaaaab"),
    };
    assert_eq!(
        serde_json::to_value(&not_member).expect("json"),
        serde_json::json!({
            "code": "not_member",
            "thread": "t-aaaaab",
            "message": not_member.to_string(),
        })
    );

    let invalid_id = CommsError::from(InvalidId::new(IdKind::Sub, "Bad"));
    assert_eq!(
        serde_json::to_value(&invalid_id).expect("json"),
        serde_json::json!({
            "code": "invalid_id",
            "kind": "sub",
            "value": "Bad",
            "message": invalid_id.to_string(),
        })
    );

    let unit = serde_json::to_value(CommsError::EmptyBody).expect("json");
    assert_eq!(unit["code"], "empty_body");
    assert_eq!(unit["message"], CommsError::EmptyBody.to_string());
}

#[test]
fn every_variant_round_trips_through_json_and_msgpack() {
    let variants = every_known_variant();
    assert_eq!(variants.len(), 27, "a new variant belongs in this list");
    for error in variants {
        let json = serde_json::to_value(&error).expect("json");
        assert_eq!(json["message"], error.to_string(), "{error:?}");
        assert_eq!(json["code"], error.code(), "{error:?}");
        assert_eq!(
            serde_json::from_value::<CommsError>(json).expect("error"),
            error,
            "{error:?}"
        );
        assert_eq!(through_msgpack(&error), error, "{error:?}");
    }
}

#[test]
fn codes_are_distinct_snake_case_variant_names() {
    let mut codes: Vec<String> = every_known_variant().iter().map(CommsError::code).collect();
    assert!(codes.contains(&"invalid_post_target".to_string()));
    assert!(codes.contains(&"cursor_not_applicable".to_string()));
    assert!(codes.iter().all(|code| !code.is_empty()));
    let total = codes.len();
    codes.sort();
    codes.dedup();
    assert_eq!(codes.len(), total);
}

#[test]
fn unknown_code_decodes_into_the_fallback_with_the_senders_message() {
    let json = serde_json::json!({
        "code": "thread_frozen",
        "thread": "t-aaaaab",
        "until": 12,
        "message": "thread t-aaaaab is frozen: wait for its owner",
    });
    let decoded = serde_json::from_value::<CommsError>(json).expect("error");

    assert_eq!(
        decoded,
        CommsError::Unknown {
            code: "thread_frozen".to_string(),
            message: "thread t-aaaaab is frozen: wait for its owner".to_string(),
        }
    );
    assert_eq!(decoded.to_string(), "thread t-aaaaab is frozen: wait for its owner");
    assert_eq!(decoded.code(), "thread_frozen");

    assert_eq!(
        serde_json::to_value(&decoded).expect("json"),
        serde_json::json!({
            "code": "thread_frozen",
            "message": "thread t-aaaaab is frozen: wait for its owner",
        })
    );
    assert_eq!(through_msgpack(&decoded), decoded);
}

#[test]
fn known_code_with_fields_this_binary_cannot_read_falls_back_too() {
    let json = serde_json::json!({
        "code": "not_member",
        "thread": {"id": "t-aaaaab", "repo": "elsewhere"},
        "message": "not a member of thread t-aaaaab in elsewhere",
    });

    assert_eq!(
        serde_json::from_value::<CommsError>(json).expect("error"),
        CommsError::Unknown {
            code: "not_member".to_string(),
            message: "not a member of thread t-aaaaab in elsewhere".to_string(),
        }
    );
}

#[test]
fn unknown_code_without_a_message_still_says_what_to_do() {
    let decoded = serde_json::from_value::<CommsError>(serde_json::json!({"code": "thread_frozen"})).expect("error");

    assert!(matches!(&decoded, CommsError::Unknown { code, .. } if code == "thread_frozen"));
    assert!(decoded.to_string().contains("thread_frozen"), "{decoded}");
    assert!(decoded.to_string().contains("upgrade basemind"), "{decoded}");
}

#[test]
fn a_known_code_decodes_without_a_message() {
    assert_eq!(
        serde_json::from_value::<CommsError>(serde_json::json!({"code": "not_member", "thread": "t-aaaaab"}))
            .expect("error"),
        CommsError::NotMember {
            thread: thread("t-aaaaab"),
        }
    );
}

#[test]
fn a_refusal_without_a_code_is_not_a_refusal() {
    assert!(serde_json::from_value::<CommsError>(serde_json::json!({"message": "x"})).is_err());
    assert!(serde_json::from_value::<CommsError>(serde_json::json!("not_member")).is_err());
    assert!(serde_json::from_value::<CommsError>(serde_json::json!({"code": 7})).is_err());
}

#[test]
fn echoed_caller_values_are_bounded() {
    let CommsError::InvalidCursor { cursor } = CommsError::invalid_cursor(&"c".repeat(10_000)) else {
        panic!("invalid_cursor builds InvalidCursor");
    };
    assert_eq!(cursor.len(), 160);

    let long_path = RelPath::from(format!("../{}", "p".repeat(10_000)));
    let CommsError::InvalidPath { path } = CommsError::invalid_path(&long_path) else {
        panic!("invalid_path builds InvalidPath");
    };
    assert_eq!(path.len(), 160);
    assert!(path.starts_with("../ppp"));
}

#[test]
fn refusals_say_what_to_do_next() {
    let actionable = [
        (
            CommsError::NotMember {
                thread: thread("t-aaaaab"),
            },
            "join: true",
        ),
        (
            CommsError::MultipleUpdates {
                fields: vec!["join".to_string(), "archive".to_string()],
            },
            "[join, archive]",
        ),
        (CommsError::QueryTooShort { min: 3 }, "at least 3 characters"),
        (
            CommsError::MissingField {
                field: "thread".to_string(),
                needed_by: "archive".to_string(),
            },
            "`archive` needs `thread`",
        ),
        (CommsError::not_applicable("query", "join"), "drop `query`"),
        (CommsError::CursorNotApplicable, "without `cursor`"),
        (CommsError::OnlySelfRecipient, "drop `to`"),
        (
            CommsError::TooManyRecipients {
                field: "invite".to_string(),
                given: 40,
                max: 32,
            },
            "maximum of 32",
        ),
        (
            CommsError::MemberCapReached {
                thread: thread("t-aaaaab"),
                cap: 256,
            },
            "more than 256 members: invite fewer",
        ),
        (
            CommsError::from(InvalidId::new(IdKind::Thread, "backend")),
            "\"backend\"",
        ),
    ];
    for (error, token) in actionable {
        assert!(error.to_string().contains(token), "{error} lacks {token:?}");
    }
}

#[test]
fn backend_refusal_names_the_status_command() {
    let refusal = CommsError::Backend {
        detail: "disk full".to_string(),
    };
    assert!(refusal.to_string().contains("`basemind agent status`"), "{refusal}");
}
