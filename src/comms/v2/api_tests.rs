use std::path::Path;

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

fn invalid(kind: IdKind, value: &str) -> InvalidId {
    InvalidId {
        kind,
        value: value.to_string(),
    }
}

#[test]
fn thread_id_encodes_thirty_bits_most_significant_digit_first() {
    assert_eq!(ThreadId::from_bits(0).as_str(), "t-aaaaaa");
    assert_eq!(ThreadId::from_bits(1).as_str(), "t-aaaaab");
    assert_eq!(ThreadId::from_bits(25).as_str(), "t-aaaaaz");
    assert_eq!(ThreadId::from_bits(26).as_str(), "t-aaaaa2");
    assert_eq!(ThreadId::from_bits(31).as_str(), "t-aaaaa7");
    assert_eq!(ThreadId::from_bits(32).as_str(), "t-aaaaba");
    assert_eq!(ThreadId::from_bits(ThreadId::SPACE - 1).as_str(), "t-777777");
    assert_eq!(ThreadId::SPACE, 1_073_741_824);
}

#[test]
fn thread_id_ignores_bits_above_the_thirtieth() {
    assert_eq!(ThreadId::from_bits(ThreadId::SPACE), ThreadId::from_bits(0));
    assert_eq!(ThreadId::from_bits(u32::MAX).as_str(), "t-777777");
}

#[test]
fn thread_id_bits_inverts_from_bits() {
    for bits in [0, 1, 31, 32, 1_000_000, ThreadId::SPACE - 1] {
        assert_eq!(ThreadId::from_bits(bits).bits(), bits);
    }
    assert_eq!(thread("t-aaaaba").bits(), 32);
}

#[test]
fn thread_id_parse_accepts_only_the_exact_shape() {
    assert_eq!(thread("t-abc234").as_str(), "t-abc234");
    for bad in [
        "",
        "t-",
        "t-aaaaa",
        "t-aaaaaaa",
        "T-aaaaaa",
        "t-AAAAAA",
        "t-aaaaa1",
        "t-aaaaa8",
        "t-aaaaa.",
        "x-aaaaaa",
        "taaaaaaa",
        " t-aaaaaa",
    ] {
        assert_eq!(ThreadId::parse(bad), Err(invalid(IdKind::Thread, bad)), "{bad:?}");
    }
}

#[test]
fn message_id_parses_and_displays_thread_and_sequence() {
    let id = message("t-abcdef.12");
    assert_eq!(id.thread(), &thread("t-abcdef"));
    assert_eq!(id.seq(), 12);
    assert_eq!(id.to_string(), "t-abcdef.12");
    assert_eq!(MessageId::new(thread("t-abcdef"), 12), Ok(id));
}

#[test]
fn message_id_accepts_the_largest_sequence_and_refuses_overflow() {
    assert_eq!(message("t-abcdef.18446744073709551615").seq(), u64::MAX);
    let overflow = "t-abcdef.18446744073709551616";
    assert_eq!(MessageId::parse(overflow), Err(invalid(IdKind::Message, overflow)));
}

#[test]
fn message_id_parse_refuses_non_canonical_forms() {
    for bad in [
        "",
        "t-abcdef",
        "t-abcdef.",
        "t-abcdef.0",
        "t-abcdef.012",
        "t-abcdef.-1",
        "t-abcdef.+1",
        "t-abcdef.1.2",
        "t-abcdef.1 ",
        "t-abcde.1",
        ".1",
        "12",
    ] {
        assert_eq!(MessageId::parse(bad), Err(invalid(IdKind::Message, bad)), "{bad:?}");
    }
}

#[test]
fn message_id_refuses_sequence_zero() {
    assert_eq!(
        MessageId::new(thread("t-abcdef"), 0),
        Err(invalid(IdKind::Message, "t-abcdef.0"))
    );
}

#[test]
fn message_ids_order_by_numeric_sequence() {
    assert!(message("t-abcdef.2") < message("t-abcdef.10"));
    assert!(message("t-aaaaab.9") < message("t-aaaaac.1"));
}

#[test]
fn sub_name_grammar() {
    let longest = "x".repeat(MAX_SUB_NAME_LEN);
    for ok in ["a", "reviewer-1", "a_b", "0", longest.as_str()] {
        assert_eq!(SubName::parse(ok).expect("sub name").as_str(), ok);
    }
    let too_long = "x".repeat(MAX_SUB_NAME_LEN + 1);
    for bad in ["", "A", "a.b", "a b", "a:b", "é", too_long.as_str()] {
        assert_eq!(SubName::parse(bad), Err(invalid(IdKind::Sub, bad)), "{bad:?}");
    }
}

#[test]
fn principal_is_the_agent_id_grammar_without_the_separator() {
    let longest = "p".repeat(128);
    for ok in ["host-repo-1a2b3c", "team:backend", "A1_", longest.as_str()] {
        assert_eq!(Principal::parse(ok).expect("principal").as_str(), ok);
    }
    let too_long = "p".repeat(129);
    for bad in ["", "a.b", "a b", "a/b", "é", too_long.as_str()] {
        assert_eq!(Principal::parse(bad), Err(invalid(IdKind::Principal, bad)), "{bad:?}");
    }
}

#[test]
fn actor_composes_principal_and_sub() {
    let principal = Principal::parse("host-repo-1a2b3c").expect("principal");
    let sub = SubName::parse("reviewer").expect("sub name");

    let bare = Actor::new(&principal, None).expect("actor");
    assert_eq!(bare.as_str(), "host-repo-1a2b3c");
    assert_eq!(bare.principal(), "host-repo-1a2b3c");
    assert_eq!(bare.sub(), None);

    let composed = Actor::new(&principal, Some(&sub)).expect("actor");
    assert_eq!(composed.as_str(), "host-repo-1a2b3c.reviewer");
    assert_eq!(composed.principal(), "host-repo-1a2b3c");
    assert_eq!(composed.sub(), Some("reviewer"));
    assert_eq!(composed.agent_id().as_str(), "host-repo-1a2b3c.reviewer");
    assert_eq!(composed.to_string(), "host-repo-1a2b3c.reviewer");
    assert_eq!(Actor::parse("host-repo-1a2b3c.reviewer"), Ok(composed));
}

#[test]
fn actor_refuses_a_composed_id_over_the_id_length() {
    let sub = SubName::parse("reviewer").expect("sub name");
    let fits = Principal::parse("p".repeat(119)).expect("principal");
    assert_eq!(Actor::new(&fits, Some(&sub)).expect("actor").as_str().len(), 128);

    let over = Principal::parse("p".repeat(120)).expect("principal");
    let composed = format!("{}.reviewer", "p".repeat(120));
    assert_eq!(
        Actor::new(&over, Some(&sub)),
        Err(InvalidId::new(IdKind::Actor, &composed))
    );
    assert_eq!(
        Actor::parse(composed.as_str()),
        Err(InvalidId::new(IdKind::Actor, &composed))
    );
}

#[test]
fn actor_parse_refuses_malformed_compositions() {
    for bad in ["", "p.", ".sub", "p.a.b", "p.Sub", "p .a", "p.a b", "p/q"] {
        assert_eq!(Actor::parse(bad), Err(invalid(IdKind::Actor, bad)), "{bad:?}");
    }
}

#[test]
fn ids_serialize_as_strings_and_validate_on_deserialize() {
    assert_eq!(serde_json::to_string(&actor("p.sub")).expect("json"), "\"p.sub\"");
    assert_eq!(
        serde_json::to_string(&message("t-abcdef.3")).expect("json"),
        "\"t-abcdef.3\""
    );
    assert_eq!(
        serde_json::to_string(&thread("t-abcdef")).expect("json"),
        "\"t-abcdef\""
    );
    assert_eq!(
        serde_json::from_str::<Actor>("\"p.sub\"").expect("actor"),
        actor("p.sub")
    );
    assert_eq!(
        serde_json::from_str::<MessageId>("\"t-abcdef.3\"").expect("message id"),
        message("t-abcdef.3")
    );
    assert!(serde_json::from_str::<Actor>("\"p.a.b\"").is_err());
    assert!(serde_json::from_str::<MessageId>("\"t-abcdef.0\"").is_err());
    assert!(serde_json::from_str::<ThreadId>("\"backend\"").is_err());
    assert!(serde_json::from_str::<SubName>("\"Bad\"").is_err());
}

#[test]
fn repo_key_from_root_is_path_keyed() {
    let key = RepoKey::from_root(Path::new("/work/repo")).expect("repo key");
    assert_eq!(key.as_str(), "path:/work/repo");
    assert!(key.is_path_keyed());

    let remote = RepoKey::parse("github.com/Foo/bar").expect("repo key");
    assert!(!remote.is_path_keyed());
}

#[test]
fn repo_key_refuses_empty_control_and_oversized_values() {
    let too_long = "r".repeat(MAX_REPO_KEY_LEN + 1);
    assert_eq!(RepoKey::parse(""), Err(invalid(IdKind::Repo, "")));
    assert_eq!(RepoKey::parse("a\nb"), Err(invalid(IdKind::Repo, "a\nb")));
    assert_eq!(
        RepoKey::parse(too_long.as_str()),
        Err(InvalidId::new(IdKind::Repo, &too_long))
    );
    assert_eq!(
        RepoKey::parse("r".repeat(MAX_REPO_KEY_LEN))
            .expect("repo key")
            .as_str()
            .len(),
        4096
    );
}

#[test]
fn replay_key_is_bounded_printable_ascii() {
    assert_eq!(
        ReplayKey::parse("relay-9f3a").expect("replay key").as_str(),
        "relay-9f3a"
    );
    let too_long = "k".repeat(MAX_REPLAY_KEY_LEN + 1);
    for bad in ["", "has space", "tab\t", "é", too_long.as_str()] {
        assert_eq!(ReplayKey::parse(bad), Err(invalid(IdKind::ReplayKey, bad)), "{bad:?}");
    }
}

#[test]
fn invalid_id_echo_is_cut_at_a_character_boundary() {
    assert_eq!(InvalidId::new(IdKind::Sub, &"a".repeat(200)).value, "a".repeat(160));
    assert_eq!(InvalidId::new(IdKind::Sub, &"€".repeat(60)).value, "€".repeat(53));
    assert_eq!(InvalidId::new(IdKind::Sub, "Bad").value, "Bad");
}

#[test]
fn invalid_id_message_names_the_value_and_the_grammar() {
    assert_eq!(
        SubName::parse("Bad").expect_err("refused").to_string(),
        "invalid sub-identity name \"Bad\": expected 1 to 32 characters of a-z, 0-9, `_` or `-`"
    );
}

#[test]
fn limits_are_the_adr_values() {
    assert_eq!(MAX_INLINE_BODY_BYTES, 4096);
    assert_eq!(RESPONSE_BUDGET_BYTES, 16_384);
    assert_eq!(MAX_WAIT_SECS, 40);
    assert_eq!(DEFAULT_LIMIT, 20);
    assert_eq!(MESSAGE_TTL.as_secs(), 2_592_000);
    assert_eq!(MAX_MESSAGES_PER_THREAD, 1000);
    assert_eq!(MAX_ACTIVE_THREADS_PER_REPO, 256);
    assert_eq!(MIN_QUERY_CHARS, 3);
    assert_eq!(REPLAY_TTL.as_secs(), 3600);
    assert_eq!(COMMS_STORE_VER, 1000);
}

#[test]
fn post_target_takes_exactly_one_of_thread_reply_to_subject() {
    let paths = vec![RelPath::from("src/comms")];
    assert_eq!(
        PostTarget::from_fields(Some(thread("t-abcdef")), None, None, Vec::new(), Vec::new()),
        Ok(PostTarget::Thread(thread("t-abcdef")))
    );
    assert_eq!(
        PostTarget::from_fields(None, Some(message("t-abcdef.2")), None, Vec::new(), Vec::new()),
        Ok(PostTarget::ReplyTo(message("t-abcdef.2")))
    );
    assert_eq!(
        PostTarget::from_fields(None, None, Some("topic".to_string()), vec![actor("b")], paths.clone()),
        Ok(PostTarget::NewThread {
            subject: "topic".to_string(),
            to: vec![actor("b")],
            paths,
        })
    );
    assert_eq!(
        PostTarget::from_fields(None, None, None, Vec::new(), Vec::new()),
        Err(CommsError::PostTarget { given: Vec::new() })
    );
    assert_eq!(
        PostTarget::from_fields(
            Some(thread("t-abcdef")),
            None,
            Some("topic".to_string()),
            Vec::new(),
            Vec::new()
        ),
        Err(CommsError::PostTarget {
            given: vec!["thread".to_string(), "subject".to_string()],
        })
    );
}

#[test]
fn post_target_refuses_creation_only_fields_on_an_existing_thread() {
    assert_eq!(
        PostTarget::from_fields(Some(thread("t-abcdef")), None, None, vec![actor("b")], Vec::new()),
        Err(CommsError::CreationOnly {
            field: "to".to_string(),
        })
    );
    assert_eq!(
        PostTarget::from_fields(
            None,
            Some(message("t-abcdef.2")),
            None,
            Vec::new(),
            vec![RelPath::from("src")]
        ),
        Err(CommsError::CreationOnly {
            field: "paths".to_string(),
        })
    );
}

#[test]
fn thread_action_takes_at_most_one_update() {
    assert_eq!(ThreadAction::from_fields(false, false, None, false), Ok(None));
    assert_eq!(
        ThreadAction::from_fields(true, false, None, false),
        Ok(Some(ThreadAction::Join))
    );
    assert_eq!(
        ThreadAction::from_fields(false, true, None, false),
        Ok(Some(ThreadAction::Leave))
    );
    assert_eq!(
        ThreadAction::from_fields(false, false, Some(vec![actor("b")]), false),
        Ok(Some(ThreadAction::Invite(vec![actor("b")])))
    );
    assert_eq!(
        ThreadAction::from_fields(false, false, None, true),
        Ok(Some(ThreadAction::Archive))
    );
    assert_eq!(
        ThreadAction::from_fields(true, false, Some(vec![actor("b")]), true),
        Err(CommsError::MultipleUpdates {
            fields: vec!["join".to_string(), "invite".to_string(), "archive".to_string()],
        })
    );
    assert_eq!(
        ThreadAction::from_fields(false, false, Some(Vec::new()), false),
        Err(CommsError::EmptyInvite)
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
            len: 65_537,
            max: 65_536,
        })
    );
}

#[test]
fn refusals_say_what_to_do_next() {
    assert_eq!(
        CommsError::NotMember {
            thread: thread("t-aaaaab"),
        }
        .to_string(),
        "not a member of thread t-aaaaab: join it with agent_threads {thread, join: true}, or ask a member to invite you"
    );
    assert_eq!(
        CommsError::MultipleUpdates {
            fields: vec!["join".to_string(), "archive".to_string()],
        }
        .to_string(),
        "pass at most one of join, leave, invite, archive; this call set [join, archive]: make one call per change"
    );
    assert_eq!(
        CommsError::QueryTooShort { min: 3 }.to_string(),
        "`query` needs at least 3 characters: lengthen it, or search by `path`"
    );
    assert_eq!(
        CommsError::from(invalid(IdKind::Thread, "backend")).to_string(),
        "invalid thread id \"backend\": expected `t-` followed by 6 characters of a-z or 2-7, as returned by \
         agent_post or agent_threads"
    );
}

#[test]
fn shared_identity_notice_names_the_principal_and_the_fix() {
    assert_eq!(
        shared_identity_notice("host-repo-1a2b3c", 2),
        "2 live sessions share the identity host-repo-1a2b3c: they share one inbox and read position. \
         Set BASEMIND_AGENT_ID in one of them, or pass `as`, to separate them."
    );
}

#[test]
fn post_request_is_a_named_map_on_the_wire() {
    let request = PostRequest {
        actor: actor("a.sub"),
        repo: RepoKey::parse("github.com/Foo/bar").expect("repo key"),
        target: PostTarget::NewThread {
            subject: "topic".to_string(),
            to: vec![actor("b")],
            paths: vec![RelPath::from("src/comms")],
        },
        body: "hello".to_string(),
        replay_key: Some(ReplayKey::parse("relay-1").expect("replay key")),
    };
    let json = serde_json::json!({
        "actor": "a.sub",
        "repo": "github.com/Foo/bar",
        "target": {"new_thread": {"subject": "topic", "to": ["b"], "paths": ["src/comms"]}},
        "body": "hello",
        "replay_key": "relay-1",
    });
    assert_eq!(serde_json::to_value(&request).expect("json"), json);
    assert_eq!(serde_json::from_value::<PostRequest>(json).expect("request"), request);

    let packed = rmp_serde::to_vec_named(&request).expect("msgpack");
    assert_eq!(rmp_serde::from_slice::<PostRequest>(&packed).expect("request"), request);
}

#[test]
fn inbox_request_defaults_match_the_tool_defaults() {
    let json = serde_json::json!({"actor": "a", "repo": "path:/work/repo"});
    let request = serde_json::from_value::<InboxRequest>(json).expect("request");
    let expected = InboxRequest::new(actor("a"), RepoKey::parse("path:/work/repo").expect("repo key"));
    assert_eq!(request, expected);
    assert_eq!(request.select, InboxSelect::All);
    assert!(request.unread);
    assert_eq!(request.mark, Mark::Read);
    assert_eq!(request.wait_secs, 0);
    assert_eq!(request.limit, None);
}

#[test]
fn replies_round_trip_through_msgpack() {
    let reply = InboxReply {
        messages: vec![InboxMessage {
            id: message("t-abcdef.2"),
            subject: "topic".to_string(),
            from: actor("b"),
            ts_micros: 1_700_000_000_000_002,
            reply_to: Some(message("t-abcdef.1")),
            body: "hello".to_string(),
            truncated: false,
        }],
        read_to: vec![message("t-abcdef.2")],
        next_cursor: Some("7".to_string()),
        notice: None,
        wait: WaitOutcome::Arrived,
    };
    let packed = rmp_serde::to_vec_named(&reply).expect("msgpack");
    assert_eq!(rmp_serde::from_slice::<InboxReply>(&packed).expect("reply"), reply);

    let update = ThreadUpdate {
        actor: actor("a"),
        repo: RepoKey::parse("github.com/Foo/bar").expect("repo key"),
        thread: thread("t-abcdef"),
        action: ThreadAction::Invite(vec![actor("b"), actor("c.sub")]),
    };
    let packed = rmp_serde::to_vec_named(&update).expect("msgpack");
    assert_eq!(rmp_serde::from_slice::<ThreadUpdate>(&packed).expect("update"), update);
}

#[test]
fn errors_carry_a_code_on_the_wire() {
    let not_member = CommsError::NotMember {
        thread: thread("t-aaaaab"),
    };
    assert_eq!(
        serde_json::to_value(&not_member).expect("json"),
        serde_json::json!({"code": "not_member", "thread": "t-aaaaab"})
    );

    let invalid_id = CommsError::from(invalid(IdKind::Sub, "Bad"));
    let json = serde_json::json!({"code": "invalid_id", "kind": "sub", "value": "Bad"});
    assert_eq!(serde_json::to_value(&invalid_id).expect("json"), json);
    assert_eq!(serde_json::from_value::<CommsError>(json).expect("error"), invalid_id);

    for error in [
        not_member,
        invalid_id,
        CommsError::EmptyBody,
        CommsError::StoreNewer {
            store: 1001,
            binary: 1000,
        },
    ] {
        let packed = rmp_serde::to_vec_named(&error).expect("msgpack");
        assert_eq!(rmp_serde::from_slice::<CommsError>(&packed).expect("error"), error);
    }
}
