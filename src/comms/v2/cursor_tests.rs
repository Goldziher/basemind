use super::*;

fn actor(s: &str) -> Actor {
    Actor::parse(s).expect("actor")
}

fn thread(s: &str) -> ThreadId {
    ThreadId::parse(s).expect("thread id")
}

fn repo(s: &str) -> RepoKey {
    RepoKey::parse(s).expect("repo key")
}

fn is_invalid(result: Result<impl std::fmt::Debug, CommsError>) -> bool {
    matches!(result, Err(CommsError::InvalidCursor { .. }))
}

#[test]
fn inbox_cursor_round_trips_its_position() {
    let (a, one) = (actor("a"), thread("t-abcdef"));
    for order in [0, 1, 7, u64::MAX] {
        let all = Cursor::for_inbox(&a, None, PagedRead::Peek, order);
        assert_eq!(all.inbox_position(&a, None, PagedRead::Peek), Ok(order));
        let scoped = Cursor::for_inbox(&a, Some(&one), PagedRead::History, order);
        assert_eq!(scoped.inbox_position(&a, Some(&one), PagedRead::History), Ok(order));
    }
}

#[test]
fn inbox_cursor_is_refused_for_another_actor_selection_or_mode() {
    let (a, b) = (actor("a"), actor("b"));
    let (one, two) = (thread("t-abcdef"), thread("t-abcdeg"));
    let cursor = Cursor::for_inbox(&a, Some(&one), PagedRead::Peek, 7);

    assert_eq!(cursor.inbox_position(&a, Some(&one), PagedRead::Peek), Ok(7));
    assert!(is_invalid(cursor.inbox_position(&b, Some(&one), PagedRead::Peek)));
    assert!(is_invalid(cursor.inbox_position(&a, Some(&two), PagedRead::Peek)));
    assert!(is_invalid(cursor.inbox_position(&a, None, PagedRead::Peek)));
    assert!(is_invalid(cursor.inbox_position(&a, Some(&one), PagedRead::History)));
    assert!(is_invalid(cursor.inbox_position(
        &actor("a.helper"),
        Some(&one),
        PagedRead::Peek
    )));
}

#[test]
fn threads_cursor_round_trips_activity_and_id() {
    let (a, home) = (actor("a"), repo("github.com/acme/app"));
    let listing = ThreadListing {
        actor: &a,
        repo: &home,
        query: Some("migration"),
        path: None,
        archived: false,
    };
    for order in [0, 1, 1_700_000_000_000_001, u64::MAX] {
        let cursor = Cursor::for_threads(&listing, order);
        assert_eq!(cursor.threads_position(&listing), Ok(order));
    }
}

#[test]
fn threads_cursor_is_refused_for_another_listing() {
    let (a, b) = (actor("a"), actor("b"));
    let (home, other) = (repo("github.com/acme/app"), repo("path:/work/elsewhere"));
    let path = RelPath::from("src/comms");
    let listing = ThreadListing {
        actor: &a,
        repo: &home,
        query: Some("migration"),
        path: None,
        archived: false,
    };
    let cursor = Cursor::for_threads(&listing, 9);

    for foreign in [
        ThreadListing { actor: &b, ..listing },
        ThreadListing {
            repo: &other,
            ..listing
        },
        ThreadListing {
            query: Some("migrations"),
            ..listing
        },
        ThreadListing { query: None, ..listing },
        ThreadListing {
            path: Some(&path),
            ..listing
        },
        ThreadListing {
            archived: true,
            ..listing
        },
    ] {
        assert!(is_invalid(cursor.threads_position(&foreign)), "{foreign:?}");
    }
    assert!(is_invalid(cursor.inbox_position(&a, None, PagedRead::Peek)));
}

#[test]
fn absent_and_empty_selections_are_different_lists() {
    let (a, home) = (actor("a"), repo("github.com/acme/app"));
    let unfiltered = ThreadListing {
        actor: &a,
        repo: &home,
        query: None,
        path: None,
        archived: false,
    };
    let empty_query = ThreadListing {
        query: Some(""),
        ..unfiltered
    };
    let cursor = Cursor::for_threads(&unfiltered, 9);

    assert!(is_invalid(cursor.threads_position(&empty_query)));
}

#[test]
fn malformed_cursor_is_refused_with_a_bounded_echo() {
    let a = actor("a");
    let good = Cursor::for_inbox(&a, None, PagedRead::Peek, 7);
    let text = good.as_str();
    let upper = text.to_uppercase();
    let truncated = &text[..text.len() - 1];
    let extended = format!("{text}0");
    let signed = format!("{}+{}", &text[..20], &text[21..]);

    for bad in ["", "i", "7", "not-a-cursor", "é", &upper, truncated, &extended, &signed] {
        let cursor = Cursor::from_wire(bad);
        assert_eq!(
            cursor.inbox_position(&a, None, PagedRead::Peek),
            Err(CommsError::invalid_cursor(bad)),
            "{bad:?}"
        );
    }

    let huge = Cursor::from_wire("z".repeat(10_000));
    let Err(CommsError::InvalidCursor { cursor }) = huge.inbox_position(&a, None, PagedRead::Peek) else {
        panic!("an oversized cursor is refused");
    };
    assert_eq!(cursor.len(), 160);
}

#[test]
fn cursor_is_a_plain_string_on_the_wire() {
    let cursor = Cursor::for_inbox(&actor("a"), None, PagedRead::History, 7);
    let json = serde_json::to_value(&cursor).expect("json");

    assert_eq!(json, serde_json::Value::String(cursor.to_string()));
    assert_eq!(serde_json::from_value::<Cursor>(json).expect("cursor"), cursor);
}
