use super::*;
use crate::comms::v2::api::{DEFAULT_LIMIT, MAX_RECIPIENTS, MAX_THREAD_PATHS, ReadKind};
use crate::comms::v2::ids::{IdKind, InvalidId};

fn thread(s: &str) -> ThreadId {
    ThreadId::parse(s).expect("thread id")
}

fn message(s: &str) -> MessageId {
    MessageId::parse(s).expect("message id")
}

fn actor(s: &str) -> Actor {
    Actor::parse(s).expect("actor")
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn threads(fields: ThreadsFields) -> Result<ThreadsCall, CommsError> {
    ThreadsCall::from_fields(fields)
}

fn on_thread() -> ThreadsFields {
    ThreadsFields {
        thread: Some("t-abcdef".to_string()),
        ..ThreadsFields::default()
    }
}

#[test]
fn post_target_takes_exactly_one_of_thread_reply_to_subject() {
    assert_eq!(
        PostTarget::from_fields(Some("t-abcdef"), None, None, &[], &[]),
        Ok(PostTarget::Thread(thread("t-abcdef")))
    );
    assert_eq!(
        PostTarget::from_fields(None, Some("t-abcdef.2"), None, &[], &[]),
        Ok(PostTarget::ReplyTo(message("t-abcdef.2")))
    );
    assert_eq!(
        PostTarget::from_fields(
            None,
            None,
            Some("  topic "),
            &strings(&["b"]),
            &strings(&["src/comms/", "docs"])
        ),
        Ok(PostTarget::NewThread {
            subject: "topic".to_string(),
            to: vec![actor("b")],
            paths: vec![RelPath::from("docs"), RelPath::from("src/comms")],
        })
    );
    assert_eq!(
        PostTarget::from_fields(None, None, None, &[], &[]),
        Err(CommsError::InvalidPostTarget { given: Vec::new() })
    );
    assert_eq!(
        PostTarget::from_fields(Some("t-abcdef"), None, Some("topic"), &[], &[]),
        Err(CommsError::InvalidPostTarget {
            given: strings(&["thread", "subject"]),
        })
    );
}

#[test]
fn post_target_reads_an_empty_string_as_a_field_left_out() {
    assert_eq!(
        PostTarget::from_fields(Some("t-abcdef"), Some(""), Some(""), &[], &[]),
        Ok(PostTarget::Thread(thread("t-abcdef")))
    );
    assert_eq!(
        PostTarget::from_fields(Some(""), Some(""), Some(""), &[], &[]),
        Err(CommsError::InvalidPostTarget { given: Vec::new() })
    );
}

#[test]
fn post_target_refuses_creation_only_fields_on_an_existing_thread() {
    assert_eq!(
        PostTarget::from_fields(Some("t-abcdef"), None, None, &strings(&["b"]), &[]),
        Err(CommsError::CreationOnly {
            field: "to".to_string(),
        })
    );
    assert_eq!(
        PostTarget::from_fields(None, Some("t-abcdef.2"), None, &[], &strings(&["src"])),
        Err(CommsError::CreationOnly {
            field: "paths".to_string(),
        })
    );
}

#[test]
fn post_target_validates_what_it_is_given() {
    assert_eq!(
        PostTarget::from_fields(Some("backend"), None, None, &[], &[]),
        Err(CommsError::InvalidId(InvalidId::new(IdKind::Thread, "backend")))
    );
    assert_eq!(
        PostTarget::from_fields(None, None, Some("topic"), &strings(&["not an id"]), &[]),
        Err(CommsError::InvalidId(InvalidId::new(IdKind::Actor, "not an id")))
    );
    assert_eq!(
        PostTarget::from_fields(None, None, Some("two\nlines"), &[], &[]),
        Err(CommsError::InvalidSubject { max: 200 })
    );
    assert_eq!(
        PostTarget::from_fields(None, None, Some("topic"), &[], &strings(&["../other"])),
        Err(CommsError::InvalidPath {
            path: "../other".to_string(),
        })
    );

    let crowd: Vec<String> = (0..=MAX_RECIPIENTS).map(|n| format!("r{n}")).collect();
    assert_eq!(
        PostTarget::from_fields(None, None, Some("topic"), &crowd, &[]),
        Err(CommsError::TooManyRecipients {
            field: "to".to_string(),
            given: 33,
            max: 32,
        })
    );
    let sprawl: Vec<String> = (0..=MAX_THREAD_PATHS).map(|n| format!("m{n}")).collect();
    assert_eq!(
        PostTarget::from_fields(None, None, Some("topic"), &[], &sprawl),
        Err(CommsError::TooManyPaths { given: 17, max: 16 })
    );
}

#[test]
fn inbox_select_takes_thread_or_id_never_both() {
    assert_eq!(InboxSelect::from_fields(None, None), Ok(InboxSelect::All));
    assert_eq!(InboxSelect::from_fields(Some(""), Some("")), Ok(InboxSelect::All));
    assert_eq!(
        InboxSelect::from_fields(Some("t-abcdef"), Some("")),
        Ok(InboxSelect::Thread(thread("t-abcdef")))
    );
    assert_eq!(
        InboxSelect::from_fields(None, Some("t-abcdef.2")),
        Ok(InboxSelect::Message(message("t-abcdef.2")))
    );
    assert_eq!(
        InboxSelect::from_fields(Some("t-abcdef"), Some("t-abcdef.2")),
        Err(CommsError::not_applicable("thread", "id"))
    );
    assert_eq!(
        InboxSelect::from_fields(None, Some("t-abcdef")),
        Err(CommsError::InvalidId(InvalidId::new(IdKind::Message, "t-abcdef")))
    );
}

#[test]
fn inbox_call_defaults_to_an_unread_marking_read_without_a_wait() {
    assert_eq!(
        InboxCall::from_fields(InboxFields::default()),
        Ok(InboxCall {
            body: InboxBody::default(),
            wait: Duration::ZERO,
        })
    );
    assert_eq!(
        InboxCall::from_fields(InboxFields {
            thread: Some("t-abcdef".to_string()),
            peek: Some(true),
            wait_secs: Some(40),
            limit: Some(5),
            cursor: Some(String::new()),
            ..InboxFields::default()
        }),
        Ok(InboxCall {
            body: InboxBody {
                select: InboxSelect::Thread(thread("t-abcdef")),
                unread: true,
                mark: Mark::Peek,
                limit: Some(5),
                cursor: None,
            },
            wait: Duration::from_secs(40),
        })
    );
}

#[test]
fn inbox_call_refuses_fields_that_do_not_apply() {
    let refused = |fields: InboxFields| InboxCall::from_fields(fields).expect_err("refused");
    let by_id = || InboxFields {
        id: Some("t-abcdef.2".to_string()),
        ..InboxFields::default()
    };

    assert_eq!(
        refused(InboxFields {
            wait_secs: Some(41),
            ..InboxFields::default()
        }),
        CommsError::WaitOutOfRange { given: 41, max: 40 }
    );
    assert_eq!(
        refused(InboxFields {
            cursor: Some("anything".to_string()),
            ..InboxFields::default()
        }),
        CommsError::CursorNotApplicable
    );
    assert_eq!(
        refused(InboxFields {
            wait_secs: Some(5),
            unread: Some(false),
            ..InboxFields::default()
        }),
        CommsError::not_applicable("wait_secs", "unread")
    );
    assert_eq!(
        refused(InboxFields {
            wait_secs: Some(5),
            ..by_id()
        }),
        CommsError::not_applicable("wait_secs", "id")
    );
    assert_eq!(
        refused(InboxFields {
            limit: Some(5),
            ..by_id()
        }),
        CommsError::not_applicable("limit", "id")
    );
    assert_eq!(
        refused(InboxFields {
            peek: Some(true),
            ..by_id()
        }),
        CommsError::not_applicable("peek", "id")
    );
    assert_eq!(
        refused(InboxFields {
            unread: Some(false),
            ..by_id()
        }),
        CommsError::not_applicable("unread", "id")
    );

    let explicit_defaults = InboxFields {
        unread: Some(true),
        peek: Some(false),
        wait_secs: Some(0),
        ..by_id()
    };
    assert_eq!(
        InboxCall::from_fields(explicit_defaults).map(|call| call.body.select),
        Ok(InboxSelect::Message(message("t-abcdef.2")))
    );
}

#[test]
fn threads_call_without_fields_lists_the_callers_threads() {
    assert_eq!(
        threads(ThreadsFields::default()),
        Ok(ThreadsCall::Query(ThreadQuery::default()))
    );
    assert_eq!(
        threads(ThreadsFields {
            query: Some(String::new()),
            path: Some(String::new()),
            thread: Some(String::new()),
            cursor: Some(String::new()),
            ..ThreadsFields::default()
        }),
        Ok(ThreadsCall::Query(ThreadQuery::default()))
    );
}

#[test]
fn threads_call_builds_a_listing_from_its_filters() {
    assert_eq!(
        threads(ThreadsFields {
            query: Some(" Migration ".to_string()),
            path: Some("src/comms/".to_string()),
            archived: true,
            limit: Some(5),
            cursor: Some("t0123".to_string()),
            ..ThreadsFields::default()
        }),
        Ok(ThreadsCall::Query(ThreadQuery::List(ThreadFilter {
            query: Some("Migration".to_string()),
            path: Some(RelPath::from("src/comms")),
            archived: true,
            limit: Some(5),
            cursor: Some(Cursor::from_wire("t0123")),
        })))
    );
    assert_eq!(
        threads(ThreadsFields {
            query: Some("mi".to_string()),
            ..ThreadsFields::default()
        }),
        Err(CommsError::QueryTooShort { min: 3 })
    );
    assert_eq!(
        threads(ThreadsFields {
            path: Some("/abs".to_string()),
            ..ThreadsFields::default()
        }),
        Err(CommsError::InvalidPath {
            path: "/abs".to_string(),
        })
    );
}

#[test]
fn threads_call_with_thread_alone_describes_it() {
    assert_eq!(
        threads(on_thread()),
        Ok(ThreadsCall::Query(ThreadQuery::Describe(thread("t-abcdef"))))
    );
    assert_eq!(
        threads(ThreadsFields {
            thread: Some("backend".to_string()),
            ..ThreadsFields::default()
        }),
        Err(CommsError::InvalidId(InvalidId::new(IdKind::Thread, "backend")))
    );
}

#[test]
fn threads_call_takes_at_most_one_update() {
    let update = |action| {
        Ok(ThreadsCall::Update(ThreadUpdate {
            thread: thread("t-abcdef"),
            action,
        }))
    };
    assert_eq!(
        threads(ThreadsFields {
            join: true,
            ..on_thread()
        }),
        update(ThreadAction::Join)
    );
    assert_eq!(
        threads(ThreadsFields {
            leave: true,
            ..on_thread()
        }),
        update(ThreadAction::Leave)
    );
    assert_eq!(
        threads(ThreadsFields {
            archive: true,
            ..on_thread()
        }),
        update(ThreadAction::Archive)
    );
    assert_eq!(
        threads(ThreadsFields {
            invite: Some(strings(&["b", "c.sub"])),
            ..on_thread()
        }),
        update(ThreadAction::Invite(vec![actor("b"), actor("c.sub")]))
    );
    assert_eq!(
        threads(ThreadsFields {
            join: true,
            invite: Some(strings(&["b"])),
            archive: true,
            ..on_thread()
        }),
        Err(CommsError::MultipleUpdates {
            fields: strings(&["join", "invite", "archive"]),
        })
    );
}

#[test]
fn threads_call_checks_an_invitation() {
    assert_eq!(
        threads(ThreadsFields {
            invite: Some(Vec::new()),
            ..on_thread()
        }),
        Err(CommsError::EmptyInvite)
    );
    assert_eq!(
        threads(ThreadsFields {
            invite: Some(strings(&["p.a.b"])),
            ..on_thread()
        }),
        Err(CommsError::InvalidId(InvalidId::new(IdKind::Actor, "p.a.b")))
    );
    let crowd: Vec<String> = (0..=MAX_RECIPIENTS).map(|n| format!("r{n}")).collect();
    assert_eq!(
        threads(ThreadsFields {
            invite: Some(crowd),
            ..on_thread()
        }),
        Err(CommsError::TooManyRecipients {
            field: "invite".to_string(),
            given: 33,
            max: 32,
        })
    );
}

#[test]
fn threads_call_refuses_an_update_without_a_thread() {
    for (action, fields) in [
        (
            "join",
            ThreadsFields {
                join: true,
                ..ThreadsFields::default()
            },
        ),
        (
            "leave",
            ThreadsFields {
                leave: true,
                thread: Some(String::new()),
                ..ThreadsFields::default()
            },
        ),
        (
            "invite",
            ThreadsFields {
                invite: Some(strings(&["b"])),
                ..ThreadsFields::default()
            },
        ),
        (
            "archive",
            ThreadsFields {
                archive: true,
                ..ThreadsFields::default()
            },
        ),
    ] {
        assert_eq!(
            threads(fields),
            Err(CommsError::MissingField {
                field: "thread".to_string(),
                needed_by: action.to_string(),
            }),
            "{action}"
        );
    }
}

#[test]
fn threads_call_refuses_listing_fields_beside_an_update_or_a_thread() {
    type SetField = fn(&mut ThreadsFields);
    let listing_fields: [(&str, SetField); 5] = [
        ("query", |fields| fields.query = Some("migration".to_string())),
        ("path", |fields| fields.path = Some("src".to_string())),
        ("cursor", |fields| fields.cursor = Some("t0123".to_string())),
        ("archived", |fields| fields.archived = true),
        ("limit", |fields| fields.limit = Some(5)),
    ];
    for (field, set) in listing_fields {
        let mut describing = on_thread();
        set(&mut describing);
        assert_eq!(
            threads(describing),
            Err(CommsError::not_applicable(field, "thread")),
            "{field} beside thread"
        );

        let mut archiving = ThreadsFields {
            archive: true,
            ..on_thread()
        };
        set(&mut archiving);
        assert_eq!(
            threads(archiving),
            Err(CommsError::not_applicable(field, "archive")),
            "{field} beside archive"
        );
    }
}

#[test]
fn field_at_its_default_value_counts_as_left_out() {
    let by_id = InboxCall::from_fields(InboxFields {
        id: Some("t-abcdef.2".to_string()),
        unread: Some(true),
        peek: Some(false),
        wait_secs: Some(0),
        limit: Some(DEFAULT_LIMIT),
        cursor: Some(String::new()),
        ..InboxFields::default()
    })
    .expect("defaults beside id");
    assert_eq!(by_id.body.kind(), Ok(ReadKind::ById));

    let described = threads(ThreadsFields {
        limit: Some(DEFAULT_LIMIT),
        ..on_thread()
    });
    assert_eq!(
        described,
        Ok(ThreadsCall::Query(ThreadQuery::Describe(thread("t-abcdef"))))
    );
}
