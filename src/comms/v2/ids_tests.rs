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
    let refused = SubName::parse("Bad").expect_err("refused");
    assert_eq!(refused, invalid(IdKind::Sub, "Bad"));
    assert!(refused.to_string().contains("\"Bad\""), "{refused}");
    assert!(refused.to_string().contains("a-z, 0-9"), "{refused}");
}

#[test]
fn bounded_echo_cuts_caller_input_at_a_character_boundary() {
    assert_eq!(bounded_echo("short"), "short");
    assert_eq!(bounded_echo(&"a".repeat(500)).len(), 160);
    assert_eq!(bounded_echo(&"€".repeat(60)), "€".repeat(53));
}
