use serde::Serialize;
use serde::de::DeserializeOwned;

use super::*;

fn thread(s: &str) -> ThreadId {
    ThreadId::parse(s).expect("thread id")
}

fn message(s: &str) -> MessageId {
    MessageId::parse(s).expect("message id")
}

fn actor(s: &str) -> Actor {
    Actor::parse(s).expect("actor")
}

fn actors(names: &[&str]) -> Vec<Actor> {
    names.iter().map(|name| actor(name)).collect()
}

/// `value` through named-map msgpack and back, the encoding the daemon protocol uses.
fn through_msgpack<T: Serialize + DeserializeOwned>(value: &T) -> T {
    let packed = rmp_serde::to_vec_named(value).expect("msgpack");
    rmp_serde::from_slice(&packed).expect("decoded")
}

fn inbox_message(id: &str, subject: &str, from: &str, body: &str) -> InboxMessage {
    InboxMessage {
        id: message(id),
        subject: subject.to_string(),
        from: actor(from),
        ts_micros: 1_700_000_000_000_002,
        reply_to: None,
        body: body.to_string(),
        body_bytes: body.len() as u64,
        truncated: false,
    }
}

#[test]
fn limits_are_the_adr_values() {
    assert_eq!(MAX_INLINE_BODY_BYTES, 4096);
    assert_eq!(RESPONSE_BUDGET_BYTES, 16_384);
    assert_eq!(MAX_WAIT_SECS, 40);
    assert_eq!(DEFAULT_LIMIT, 20);
    assert_eq!(MAX_LIMIT, 100);
    assert_eq!(MESSAGE_TTL.as_secs(), 2_592_000);
    assert_eq!(SEEN_TTL.as_secs(), 2_592_000);
    assert_eq!(MAX_MESSAGES_PER_THREAD, 1000);
    assert_eq!(MAX_ACTIVE_THREADS_PER_REPO, 256);
    assert_eq!(MAX_RECIPIENTS, 32);
    assert_eq!(MAX_THREAD_PATHS, 16);
    assert_eq!(MIN_QUERY_CHARS, 3);
    assert_eq!(REPLAY_TTL.as_secs(), 3600);
    assert_eq!(COMMS_STORE_VER, 1000);
    assert_eq!(MAX_POST_BODY_BYTES, 16_384);
    assert_eq!(MAX_THREAD_MEMBERS, 256);
    assert_eq!(MAX_SUBJECT_BYTES, 200);
}

#[test]
fn caller_carries_what_the_daemon_derived() {
    let repo = RepoKey::parse("github.com/Foo/bar").expect("repo key");
    let plain = Caller::new(actor("a.sub"), repo.clone());
    assert_eq!(plain.replay_key, None);

    let key = ReplayKey::parse("relay-1").expect("replay key");
    let keyed = plain.clone().with_replay_key(key.clone());
    assert_eq!(
        keyed,
        Caller {
            actor: actor("a.sub"),
            repo,
            replay_key: Some(key),
        }
    );
}

#[test]
fn inbox_body_kind_names_what_the_read_moves() {
    let body = |unread, mark| InboxBody {
        unread,
        mark,
        ..InboxBody::default()
    };
    assert_eq!(InboxBody::default().kind(), Ok(ReadKind::Marking));
    assert_eq!(body(true, Mark::Notify).kind(), Ok(ReadKind::Announcing));
    assert_eq!(body(true, Mark::Peek).kind(), Ok(ReadKind::Peek));
    assert_eq!(body(false, Mark::Read).kind(), Ok(ReadKind::History));
    assert_eq!(body(false, Mark::Peek).kind(), Ok(ReadKind::History));
    assert_eq!(
        body(false, Mark::Notify).kind(),
        Err(CommsError::not_applicable("unread", "notify"))
    );

    assert!(ReadKind::Marking.writes());
    assert!(ReadKind::Announcing.writes());
    for kind in [ReadKind::ById, ReadKind::Peek, ReadKind::History] {
        assert!(!kind.writes(), "{kind:?}");
    }
    assert_eq!(ReadKind::Peek.paged(), Some(PagedRead::Peek));
    assert_eq!(ReadKind::History.paged(), Some(PagedRead::History));
    assert_eq!(ReadKind::Marking.paged(), None);
}

#[test]
fn only_a_read_that_marks_nothing_takes_a_cursor() {
    let with_cursor = |unread, mark| InboxBody {
        unread,
        mark,
        cursor: Some(Cursor::from_wire("anything")),
        ..InboxBody::default()
    };
    assert_eq!(
        with_cursor(true, Mark::Read).kind(),
        Err(CommsError::CursorNotApplicable)
    );
    assert_eq!(
        with_cursor(true, Mark::Notify).kind(),
        Err(CommsError::CursorNotApplicable)
    );
    assert_eq!(with_cursor(true, Mark::Peek).kind(), Ok(ReadKind::Peek));
    assert_eq!(with_cursor(false, Mark::Read).kind(), Ok(ReadKind::History));
}

#[test]
fn read_by_id_refuses_every_list_field() {
    let by_id = InboxBody {
        select: InboxSelect::Message(message("t-abcdef.2")),
        ..InboxBody::default()
    };
    assert_eq!(by_id.kind(), Ok(ReadKind::ById));
    assert_eq!(by_id.thread(), None);

    let refused = |field: &str, body: InboxBody| {
        assert_eq!(body.kind(), Err(CommsError::not_applicable(field, "id")), "{field}");
    };
    refused(
        "cursor",
        InboxBody {
            cursor: Some(Cursor::from_wire("anything")),
            ..by_id.clone()
        },
    );
    refused(
        "limit",
        InboxBody {
            limit: Some(5),
            ..by_id.clone()
        },
    );
    refused(
        "unread",
        InboxBody {
            unread: false,
            ..by_id.clone()
        },
    );
    refused(
        "peek",
        InboxBody {
            mark: Mark::Peek,
            ..by_id.clone()
        },
    );
    refused(
        "notify",
        InboxBody {
            mark: Mark::Notify,
            ..by_id
        },
    );
}

#[test]
fn inline_body_cuts_at_the_limit_on_a_character_boundary() {
    let fits = "a".repeat(MAX_INLINE_BODY_BYTES);
    assert_eq!(inline_body(&fits), (fits.as_str(), false));

    let over = "a".repeat(MAX_INLINE_BODY_BYTES + 1);
    assert_eq!(inline_body(&over), (fits.as_str(), true));

    let wide = "€".repeat(1366);
    let (cut, truncated) = inline_body(&wide);
    assert_eq!(cut.len(), 4095);
    assert_eq!(cut, "€".repeat(1365));
    assert!(truncated);
}

#[test]
fn message_cost_counts_body_subject_sender_and_id() {
    let message = inbox_message("t-abcdef.12", "topic", "host-repo-1a2b3c.reviewer", "hello");
    assert_eq!(message_cost(&message), 5 + 5 + 25 + 11);

    let empty_subject = inbox_message("t-abcdef.1", "", "b", "");
    assert_eq!(message_cost(&empty_subject), 1 + 10);
}

#[test]
fn deliverable_always_takes_the_first_and_stops_at_the_first_that_does_not_fit() {
    assert_eq!(deliverable([], 20), 0);
    assert_eq!(deliverable([RESPONSE_BUDGET_BYTES + 1, 1], 20), 1);
    assert_eq!(deliverable([RESPONSE_BUDGET_BYTES, 1], 20), 1);
    assert_eq!(deliverable([RESPONSE_BUDGET_BYTES - 1, 1, 1], 20), 2);
    assert_eq!(
        deliverable([8000, 9000, 1, 1], 20),
        1,
        "a smaller later message is not pulled forward"
    );
    assert_eq!(deliverable([1, 1, 1], 2), 2);
    assert_eq!(deliverable([1, 1, 1], 0), 1);
    assert_eq!(deliverable([usize::MAX, usize::MAX], 20), 1);
}

#[test]
fn effective_limit_defaults_and_clamps() {
    assert_eq!(effective_limit(None), 20);
    assert_eq!(effective_limit(Some(0)), 1);
    assert_eq!(effective_limit(Some(5)), 5);
    assert_eq!(effective_limit(Some(1000)), 100);
}

#[test]
fn wait_above_forty_seconds_is_refused() {
    assert_eq!(checked_wait(0), Ok(Duration::ZERO));
    assert_eq!(checked_wait(40), Ok(Duration::from_secs(40)));
    assert_eq!(checked_wait(41), Err(CommsError::WaitOutOfRange { given: 41, max: 40 }));
}

#[test]
fn query_is_normalised_and_needs_three_characters() {
    assert_eq!(normalize_query("  AbC "), Ok("abc".to_string()));
    assert_eq!(normalize_query("ééé"), Ok("ééé".to_string()));
    assert_eq!(normalize_query("ab"), Err(CommsError::QueryTooShort { min: 3 }));
    assert_eq!(normalize_query("  ab  "), Err(CommsError::QueryTooShort { min: 3 }));
    assert_eq!(normalize_query("éé"), Err(CommsError::QueryTooShort { min: 3 }));
    assert!(subject_matches("Store Migration plan", "migration"));
    assert!(!subject_matches("Store plan", "migration"));
}

#[test]
fn thread_path_is_normalised_to_a_relative_prefix() {
    assert_eq!(
        normalize_thread_path(&RelPath::from("src/comms/")),
        Ok(RelPath::from("src/comms"))
    );
    assert_eq!(
        normalize_thread_path(&RelPath::from("Cargo.toml")),
        Ok(RelPath::from("Cargo.toml"))
    );
    for bad in ["", "/", "/abs/path", "a/../b", "..", "./a", "a//b"] {
        assert_eq!(
            normalize_thread_path(&RelPath::from(bad)),
            Err(CommsError::InvalidPath { path: bad.to_string() }),
            "{bad:?}"
        );
    }
}

#[test]
fn thread_paths_are_sorted_deduplicated_and_capped() {
    let paths = [
        RelPath::from("src/comms/"),
        RelPath::from("docs"),
        RelPath::from("src/comms"),
    ];
    assert_eq!(
        normalize_thread_paths(&paths),
        Ok(vec![RelPath::from("docs"), RelPath::from("src/comms")])
    );

    let at_cap: Vec<RelPath> = (0..MAX_THREAD_PATHS).map(|n| RelPath::from(format!("m{n}"))).collect();
    assert_eq!(normalize_thread_paths(&at_cap).map(|paths| paths.len()), Ok(16));
    let over: Vec<RelPath> = (0..=MAX_THREAD_PATHS).map(|_| RelPath::from("same")).collect();
    assert_eq!(
        normalize_thread_paths(&over),
        Err(CommsError::TooManyPaths { given: 17, max: 16 })
    );
}

#[test]
fn path_prefix_matches_whole_components_in_either_direction() {
    let dir = RelPath::from("src/comms");
    let file = RelPath::from("src/comms/store.rs");
    let sibling = RelPath::from("src/comms2");
    assert!(is_path_prefix(&dir, &file));
    assert!(is_path_prefix(&dir, &dir));
    assert!(!is_path_prefix(&file, &dir));
    assert!(!is_path_prefix(&dir, &sibling));

    assert!(path_selects(&file, std::slice::from_ref(&dir)));
    assert!(path_selects(&RelPath::from("src"), std::slice::from_ref(&dir)));
    assert!(!path_selects(&sibling, std::slice::from_ref(&dir)));
    assert!(!path_selects(&dir, &[]));
}

#[test]
fn subject_is_one_trimmed_bounded_line() {
    assert_eq!(
        normalize_subject("  store migration  "),
        Ok("store migration".to_string())
    );
    assert_eq!(normalize_subject(&"s".repeat(200)), Ok("s".repeat(200)));
    let too_long = "s".repeat(201);
    for bad in ["", "   ", "two\nlines", too_long.as_str()] {
        assert_eq!(
            normalize_subject(bad),
            Err(CommsError::InvalidSubject { max: 200 }),
            "{bad:?}"
        );
    }
}

#[test]
fn body_must_be_non_blank_and_bounded() {
    assert_eq!(check_body("hello"), Ok(()));
    assert_eq!(check_body(""), Err(CommsError::EmptyBody));
    assert_eq!(check_body(" \n\t"), Err(CommsError::EmptyBody));
    assert_eq!(check_body(&"b".repeat(MAX_POST_BODY_BYTES)), Ok(()));
    assert_eq!(
        check_body(&"b".repeat(MAX_POST_BODY_BYTES + 1)),
        Err(CommsError::BodyTooLarge {
            len: 16_385,
            max: 16_384,
        })
    );
}

#[test]
fn new_thread_recipients_drop_the_creator_and_duplicates() {
    let creator = actor("a");
    assert_eq!(new_thread_recipients(&creator, &[]), Ok(Vec::new()));
    assert_eq!(
        new_thread_recipients(&creator, &actors(&["c", "a", "b", "c"])),
        Ok(actors(&["b", "c"]))
    );
    assert_eq!(
        new_thread_recipients(&creator, &actors(&["a"])),
        Err(CommsError::OnlySelfRecipient)
    );
    assert_eq!(
        new_thread_recipients(&creator, &actors(&["a.helper"])),
        Ok(actors(&["a.helper"])),
        "a sub-identity is another agent"
    );
}

#[test]
fn recipients_are_capped_at_thirty_two() {
    let names: Vec<Actor> = (0..=MAX_RECIPIENTS).map(|n| actor(&format!("r{n}"))).collect();
    assert_eq!(check_recipient_count("invite", &names[..MAX_RECIPIENTS]), Ok(()));
    assert_eq!(
        check_recipient_count("invite", &names),
        Err(CommsError::TooManyRecipients {
            field: "invite".to_string(),
            given: 33,
            max: 32,
        })
    );
    assert_eq!(
        new_thread_recipients(&actor("a"), &names),
        Err(CommsError::TooManyRecipients {
            field: "to".to_string(),
            given: 33,
            max: 32,
        })
    );
}

#[test]
fn shared_identity_notice_names_the_principal_and_the_fix() {
    let notice = shared_identity_notice(&actor("host-repo-1a2b3c"), 2).expect("two sessions, no `as`");
    assert!(
        notice.starts_with("2 live sessions share the identity host-repo-1a2b3c"),
        "{notice}"
    );
    assert!(notice.contains("BASEMIND_AGENT_ID"), "{notice}");
    assert!(notice.contains("`as`"), "{notice}");
}

#[test]
fn shared_identity_notice_is_left_out_for_one_session_and_for_a_call_with_as() {
    assert_eq!(shared_identity_notice(&actor("host-repo-1a2b3c"), 1), None);
    assert_eq!(shared_identity_notice(&actor("host-repo-1a2b3c.helper"), 5), None);
}

#[test]
fn cap_archive_notice_names_the_archived_thread() {
    let notice = cap_archive_notice(&thread("t-abcdef"));
    assert!(notice.contains("t-abcdef") && notice.contains("archived"), "{notice}");
}

#[test]
fn rejected_id_notice_names_both_ids_and_bounds_the_rejected_one() {
    let notice = rejected_id_notice("bad id", "host-repo-1a2b3c");
    assert!(notice.contains("\"bad id\""), "{notice}");
    assert!(notice.contains("acts as host-repo-1a2b3c"), "{notice}");
    assert!(notice.contains("BASEMIND_AGENT_ID"), "{notice}");

    let oversized = rejected_id_notice(&"x".repeat(10_000), "host-repo-1a2b3c");
    assert!(oversized.len() < 500, "{}", oversized.len());
}

#[test]
fn post_body_is_a_named_map_without_the_caller() {
    let body = PostBody {
        target: PostTarget::NewThread {
            subject: "topic".to_string(),
            to: vec![actor("b")],
            paths: vec![RelPath::from("src/comms")],
        },
        body: "hello".to_string(),
    };
    let json = serde_json::json!({
        "target": {"new_thread": {"subject": "topic", "to": ["b"], "paths": ["src/comms"]}},
        "body": "hello",
    });
    assert_eq!(serde_json::to_value(&body).expect("json"), json);
    assert_eq!(serde_json::from_value::<PostBody>(json).expect("body"), body);
    assert_eq!(through_msgpack(&body), body);
}

#[test]
fn inbox_body_defaults_match_the_tool_defaults() {
    let body = serde_json::from_value::<InboxBody>(serde_json::json!({})).expect("body");
    assert_eq!(body, InboxBody::default());
    assert_eq!(body.select, InboxSelect::All);
    assert!(body.unread);
    assert_eq!(body.mark, Mark::Read);
    assert_eq!(body.limit, None);
    assert_eq!(body.cursor, None);

    let query = serde_json::from_value::<ThreadQuery>(serde_json::json!({"list": {}})).expect("query");
    assert_eq!(query, ThreadQuery::default());
}

#[test]
fn unit_and_newtype_variants_round_trip_through_msgpack() {
    for target in [
        PostTarget::Thread(thread("t-abcdef")),
        PostTarget::ReplyTo(message("t-abcdef.2")),
    ] {
        assert_eq!(through_msgpack(&target), target);
    }
    for select in [
        InboxSelect::All,
        InboxSelect::Thread(thread("t-abcdef")),
        InboxSelect::Message(message("t-abcdef.2")),
    ] {
        assert_eq!(through_msgpack(&select), select);
        let body = InboxBody {
            select,
            ..InboxBody::default()
        };
        assert_eq!(through_msgpack(&body), body);
    }
    for mark in [Mark::Read, Mark::Peek, Mark::Notify] {
        assert_eq!(through_msgpack(&mark), mark);
    }
    for wait in [WaitOutcome::NotWaited, WaitOutcome::Arrived, WaitOutcome::TimedOut] {
        assert_eq!(through_msgpack(&wait), wait);
    }
    for action in [
        ThreadAction::Join,
        ThreadAction::Leave,
        ThreadAction::Invite(actors(&["b", "c.sub"])),
        ThreadAction::Archive,
    ] {
        let update = ThreadUpdate {
            thread: thread("t-abcdef"),
            action,
        };
        assert_eq!(through_msgpack(&update), update);
    }
    for query in [
        ThreadQuery::default(),
        ThreadQuery::Describe(thread("t-abcdef")),
        ThreadQuery::List(ThreadFilter {
            query: Some("migration".to_string()),
            path: Some(RelPath::from("src/comms")),
            archived: true,
            limit: Some(5),
            cursor: Some(Cursor::from_wire("t0123")),
        }),
    ] {
        assert_eq!(through_msgpack(&query), query);
    }
}

#[test]
fn unit_variants_are_plain_strings_on_the_wire() {
    assert_eq!(
        serde_json::to_value(InboxSelect::All).expect("json"),
        serde_json::json!("all")
    );
    assert_eq!(
        serde_json::to_value(ThreadAction::Join).expect("json"),
        serde_json::json!("join")
    );
    assert_eq!(
        serde_json::to_value(InboxSelect::Thread(thread("t-abcdef"))).expect("json"),
        serde_json::json!({"thread": "t-abcdef"})
    );
    assert_eq!(
        serde_json::to_value(ThreadQuery::Describe(thread("t-abcdef"))).expect("json"),
        serde_json::json!({"describe": "t-abcdef"})
    );
}

#[test]
fn replies_round_trip_through_msgpack() {
    let inbox = InboxReply {
        messages: vec![InboxMessage {
            reply_to: Some(message("t-abcdef.1")),
            ..inbox_message("t-abcdef.2", "topic", "b", "hello")
        }],
        read_to: vec![message("t-abcdef.2")],
        more: true,
        next_cursor: Some(Cursor::for_inbox(&actor("a"), None, PagedRead::Peek, 7)),
        notices: shared_identity_notice(&actor("a"), 2).into_iter().collect(),
        wait: WaitOutcome::Arrived,
    };
    assert_eq!(through_msgpack(&inbox), inbox);

    let post = PostReply {
        id: message("t-abcdef.3"),
        thread: thread("t-abcdef"),
        created: false,
        pending: actors(&["typo"]),
        notices: Vec::new(),
    };
    assert_eq!(through_msgpack(&post), post);

    let threads = ThreadsReply {
        threads: vec![ThreadSummary {
            id: thread("t-abcdef"),
            subject: "topic".to_string(),
            repo: RepoKey::parse("github.com/Foo/bar").expect("repo key"),
            paths: vec![RelPath::from("src/comms")],
            open: true,
            archived: false,
            creator: actor("a"),
            member: true,
            unread: 3,
            head: 9,
            last_activity_micros: 1_700_000_000_000_009,
            peers: vec![actor("b").principal_id()],
        }],
        pending: Vec::new(),
        next_cursor: None,
        notices: Vec::new(),
    };
    assert_eq!(through_msgpack(&threads), threads);
}

#[test]
fn a_reply_without_the_newer_fields_still_decodes() {
    let reply = serde_json::from_value::<PostReply>(serde_json::json!({"id": "t-abcdef.3", "thread": "t-abcdef"}))
        .expect("reply");
    assert_eq!(reply.notices, Vec::<String>::new());
    assert!(!reply.created);

    let inbox = serde_json::from_value::<InboxReply>(serde_json::json!({})).expect("reply");
    assert!(!inbox.more);
    assert_eq!(inbox.wait, WaitOutcome::NotWaited);
}
