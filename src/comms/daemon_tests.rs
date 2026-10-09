//! Unit tests for the comms [`Broker`](super::Broker). Split out of `daemon.rs` (via a
//! `#[cfg(test)] #[path = "daemon_tests.rs"] mod tests;` declaration) to keep `daemon.rs` under
//! the 1000-line `rust-max-lines` cap. `super` here resolves to the `daemon` module.

use super::*;
use crate::comms::model::MessageBody;
use crate::comms::model::message_reference;
use crate::comms::store;

#[path = "daemon_ack_subscribe_tests.rs"]
mod ack_subscribe_tests;
#[path = "daemon_cleanup_tests.rs"]
mod cleanup_tests;
#[path = "daemon_lifecycle_tests.rs"]
mod lifecycle_tests;
#[path = "daemon_maintenance_tests.rs"]
mod maintenance_tests;

fn temp_broker() -> (tempfile::TempDir, Arc<Broker>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(CommsStore::open(dir.path()).expect("store"));
    (dir, Arc::new(Broker::new(store)))
}

fn agent(s: &str) -> AgentId {
    AgentId::parse(s).expect("agent")
}

async fn hello(broker: &Broker, tx: &mpsc::Sender<CommsOut>, who: &str) -> Session {
    let mut session = Session::default();
    broker
        .handle(
            CommsRequest::Hello {
                agent: agent(who),
                proto_ver: PROTO_VER,
                remote: None,
                cwd: None,
            },
            &mut session,
            tx,
        )
        .await;
    session
}

/// Start a thread addressed by subject + members (two dimensions), returning its id.
async fn start_thread(
    broker: &Broker,
    session: &mut Session,
    tx: &mpsc::Sender<CommsOut>,
    members: &[&str],
) -> ThreadId {
    let resp = broker
        .handle(
            CommsRequest::ThreadStart {
                subject: Some("topic".to_string()),
                path: None,
                members: members.iter().map(|m| agent(m)).collect::<Vec<_>>(),
            },
            session,
            tx,
        )
        .await;
    match resp {
        CommsResponse::Thread(t) => t.id,
        other => panic!("expected Thread, got {other:?}"),
    }
}

async fn join(broker: &Broker, session: &mut Session, tx: &mpsc::Sender<CommsOut>, thread: &ThreadId) {
    broker
        .handle(CommsRequest::ThreadJoin { thread: thread.clone() }, session, tx)
        .await;
}

async fn post(
    broker: &Broker,
    session: &mut Session,
    tx: &mpsc::Sender<CommsOut>,
    thread: &ThreadId,
    subject: &str,
) -> String {
    match broker
        .handle(
            CommsRequest::ThreadPost {
                idempotency_key: None,
                thread: thread.clone(),
                subject: subject.to_string(),
                tags: vec![],
                reply_to: None,
                body: subject.as_bytes().to_vec(),
            },
            session,
            tx,
        )
        .await
    {
        CommsResponse::Posted { message_id } => message_id,
        other => panic!("expected Posted, got {other:?}"),
    }
}

#[tokio::test]
async fn compact_message_reference_fetches_acks_and_replies() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let mut bob = hello(&broker, &tx, "bob").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    join(&broker, &mut bob, &tx, &thread).await;
    let message_id = post(&broker, &mut alice, &tx, &thread, "compact").await;
    let reference = message_reference(&message_id);

    let body = broker
        .handle(
            CommsRequest::GetBody {
                message_id: reference.clone(),
            },
            &mut bob,
            &tx,
        )
        .await;
    assert!(matches!(body, CommsResponse::Body { body: Some(body) } if body == b"compact"));

    let ack = broker
        .handle(
            CommsRequest::AckInbox {
                message_ids: vec![reference.clone()],
                thread: None,
                to_seq: None,
            },
            &mut bob,
            &tx,
        )
        .await;
    assert!(matches!(ack, CommsResponse::Acked { acked: 1, .. }));

    let reply = broker
        .handle(
            CommsRequest::ThreadPost {
                idempotency_key: None,
                thread: thread.clone(),
                subject: "reply".to_string(),
                tags: vec![],
                reply_to: Some(reference),
                body: b"reply".to_vec(),
            },
            &mut bob,
            &tx,
        )
        .await;
    let reply_id = match reply {
        CommsResponse::Posted { message_id } => message_id,
        other => panic!("expected Posted, got {other:?}"),
    };
    let meta = broker.store.resolve_ids(&[reply_id]).expect("resolve reply id");
    let history = broker.store.history(&thread, 0, 10).expect("history");
    assert_eq!(meta.len(), 1);
    assert_eq!(history.messages[1].1.reply_to.as_deref(), Some(message_id.as_str()));
}

#[tokio::test]
async fn compact_message_reference_reports_malformed_missing_and_ambiguous() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let first = post(&broker, &mut alice, &tx, &thread, "first").await;

    for (reference, expected) in [("m-x", "malformed_message_ref"), ("m-dead", "missing_message_ref")] {
        let response = broker
            .handle(
                CommsRequest::GetBody {
                    message_id: reference.to_string(),
                },
                &mut alice,
                &tx,
            )
            .await;
        assert!(matches!(response, CommsResponse::Error { code, .. } if code == expected));
    }

    let mut mallory = hello(&broker, &tx, "mallory").await;
    let unauthorized = broker
        .handle(
            CommsRequest::GetBody {
                message_id: message_reference(&first),
            },
            &mut mallory,
            &tx,
        )
        .await;
    assert!(matches!(unauthorized, CommsResponse::Error { code, .. } if code == "not_member"));

    let mut prefixes = std::collections::BTreeMap::new();
    let mut collision = None;
    for n in 0..10_000 {
        let id = format!("collision-{n}");
        let prefix = message_reference(&id)[..6].to_string();
        if let Some(previous) = prefixes.insert(prefix.clone(), id.clone()) {
            collision = Some((prefix, previous, id));
            break;
        }
    }
    let (prefix, first_id, second_id) = collision.expect("test fixture should find a four-hex-digit collision");
    for id in [first_id, second_id] {
        let meta = store::build_meta(
            id,
            thread.clone(),
            agent("alice"),
            "collision".to_string(),
            vec![],
            None,
            b"collision",
        );
        broker
            .store
            .post(&thread, meta, MessageBody(b"collision".to_vec()))
            .expect("post collision fixture");
    }
    let response = broker
        .handle(CommsRequest::GetBody { message_id: prefix }, &mut alice, &tx)
        .await;
    assert!(matches!(response, CommsResponse::Error { code, .. } if code == "ambiguous_message_ref"));
}

async fn inbox(broker: &Broker, session: &mut Session, tx: &mpsc::Sender<CommsOut>) -> Vec<SeqMeta> {
    match broker
        .handle(
            CommsRequest::Inbox {
                remote: None,
                cwd: None,
                cursor: None,
                limit: None,
                mark_read: false,
                since_micros: None,
            },
            session,
            tx,
        )
        .await
    {
        CommsResponse::Inbox { messages, .. } => messages,
        other => panic!("expected Inbox, got {other:?}"),
    }
}

#[tokio::test]
async fn hello_rejects_proto_skew() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut session = Session::default();
    let resp = broker
        .handle(
            CommsRequest::Hello {
                agent: agent("a"),
                proto_ver: PROTO_VER + 1,
                remote: None,
                cwd: None,
            },
            &mut session,
            &tx,
        )
        .await;
    assert!(matches!(resp, CommsResponse::Error { code, .. } if code == "proto_skew"));
}

#[tokio::test]
async fn post_requires_hello() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut session = Session::default();
    let resp = broker
        .handle(
            CommsRequest::ThreadPost {
                idempotency_key: None,
                thread: ThreadId::parse("t").expect("t"),
                subject: "s".to_string(),
                tags: vec![],
                reply_to: None,
                body: b"b".to_vec(),
            },
            &mut session,
            &tx,
        )
        .await;
    assert!(matches!(resp, CommsResponse::Error { code, .. } if code == "no_hello"));
}

/// `thread_start` with fewer than two dimensions is rejected; two-of-three succeeds.
#[tokio::test]
async fn thread_start_requires_two_of_three_dimensions() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;

    let one_dim = broker
        .handle(
            CommsRequest::ThreadStart {
                subject: Some("just-a-topic".to_string()),
                path: None,
                members: vec![],
            },
            &mut alice,
            &tx,
        )
        .await;
    assert!(
        matches!(one_dim, CommsResponse::Error { code, .. } if code == "insufficient_dimensions"),
        "a single dimension must be rejected"
    );

    let creator_only = broker
        .handle(
            CommsRequest::ThreadStart {
                subject: Some("topic".to_string()),
                path: None,
                members: vec![agent("alice")],
            },
            &mut alice,
            &tx,
        )
        .await;
    assert!(
        matches!(creator_only, CommsResponse::Error { code, .. } if code == "insufficient_dimensions"),
        "creator-only membership does not count as the members dimension"
    );

    let ok = broker
        .handle(
            CommsRequest::ThreadStart {
                subject: Some("topic".to_string()),
                path: Some("src/**".to_string()),
                members: vec![],
            },
            &mut alice,
            &tx,
        )
        .await;
    assert!(matches!(ok, CommsResponse::Thread(_)), "subject+path is two dimensions");
}

#[test]
fn validate_dimensions_counts_explicit_members_only() {
    let creator = agent("alice");
    assert!(validate_dimensions(Some("s"), None, &[], &creator).is_err());
    assert!(validate_dimensions(Some("s"), None, std::slice::from_ref(&creator), &creator).is_err());
    assert!(validate_dimensions(Some("s"), Some("src/**"), &[], &creator).is_ok());
    assert!(validate_dimensions(Some("s"), None, &[agent("bob")], &creator).is_ok());
    assert!(validate_dimensions(None, Some("src/**"), &[agent("bob")], &creator).is_ok());
}

#[test]
fn sanitize_id_maps_to_alphabet() {
    assert_eq!(sanitize_id("github.com/foo/bar"), "github.com-foo-bar");
    assert!(ThreadId::parse(sanitize_id("a b!c")).is_ok());
}

/// A non-member whose cwd doesn't match a thread's path does NOT see it in `thread_list` — no
/// global leak. A member sees theirs.
#[tokio::test]
async fn thread_list_does_not_leak_non_matching_threads() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;

    let mut carol = hello(&broker, &tx, "carol").await;
    let carol_list = match broker
        .handle(
            CommsRequest::ThreadList {
                remote: None,
                cwd: None,
                subject_contains: None,
                include_archived: false,
            },
            &mut carol,
            &tx,
        )
        .await
    {
        CommsResponse::Threads(t) => t,
        other => panic!("expected Threads, got {other:?}"),
    };
    assert!(carol_list.is_empty(), "a non-member with no path match sees nothing");

    let alice_list = match broker
        .handle(
            CommsRequest::ThreadList {
                remote: None,
                cwd: None,
                subject_contains: None,
                include_archived: false,
            },
            &mut alice,
            &tx,
        )
        .await
    {
        CommsResponse::Threads(t) => t,
        other => panic!("expected Threads, got {other:?}"),
    };
    assert_eq!(alice_list.len(), 1);
    assert_eq!(alice_list[0].id, thread);
}

/// join → post → history round-trips, and the poster's own message is excluded from its inbox
/// while a fellow member sees it.
#[tokio::test]
async fn join_post_history_and_inbox_round_trip() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(64);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let mut bob = hello(&broker, &tx, "bob").await;

    let _m1 = post(&broker, &mut alice, &tx, &thread, "first").await;
    let _m2 = post(&broker, &mut alice, &tx, &thread, "second").await;

    let bob_inbox = inbox(&broker, &mut bob, &tx).await;
    assert_eq!(bob_inbox.len(), 2);

    match broker
        .handle(
            CommsRequest::ThreadHistory {
                thread: thread.clone(),
                cursor: None,
                limit: None,
                since_micros: None,
            },
            &mut bob,
            &tx,
        )
        .await
    {
        CommsResponse::History { messages, .. } => {
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[0].meta.subject, "first");
        }
        other => panic!("expected History, got {other:?}"),
    }

    assert!(inbox(&broker, &mut alice, &tx).await.is_empty());
}

#[tokio::test]
async fn history_applies_recency_before_the_page_limit() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(16);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let cutoff = crate::comms::model::now_micros();

    for (index, timestamp) in [cutoff - 2, cutoff - 1, cutoff + 1].into_iter().enumerate() {
        let subject = format!("message-{index}");
        let body = subject.as_bytes().to_vec();
        let mut meta = store::build_meta(
            format!("filtered-history-{index}"),
            thread.clone(),
            agent("alice"),
            subject,
            vec![],
            None,
            &body,
        );
        meta.ts_micros = timestamp;
        broker
            .store
            .post(&thread, meta, MessageBody(body))
            .expect("store history fixture");
    }

    match broker
        .handle(
            CommsRequest::ThreadHistory {
                thread,
                cursor: None,
                limit: Some(2),
                since_micros: Some(cutoff),
            },
            &mut alice,
            &tx,
        )
        .await
    {
        CommsResponse::History { messages, next_cursor } => {
            assert_eq!(messages.len(), 1, "old rows must not consume the requested page");
            assert_eq!(messages[0].meta.subject, "message-2");
            assert!(next_cursor.is_none());
        }
        other => panic!("expected History, got {other:?}"),
    }
}

/// Inbox reflects ONLY joined threads: a message in a thread the agent has not joined never
/// surfaces.
#[tokio::test]
async fn inbox_reflects_only_joined_threads() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(64);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    post(&broker, &mut alice, &tx, &thread, "hello").await;

    let mut carol = hello(&broker, &tx, "carol").await;
    assert!(
        inbox(&broker, &mut carol, &tx).await.is_empty(),
        "non-member sees nothing"
    );

    join(&broker, &mut carol, &tx, &thread).await;
    post(&broker, &mut alice, &tx, &thread, "after-join").await;
    let carol_inbox = inbox(&broker, &mut carol, &tx).await;
    assert!(carol_inbox.iter().any(|m| m.meta.subject == "after-join"));
}

#[tokio::test]
async fn inbox_cursor_tracks_every_thread_without_repeats_and_counts_exact_backlog() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(32);
    let mut alice = hello(&broker, &tx, "alice").await;
    let first_thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let second_thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let mut bob = hello(&broker, &tx, "bob").await;
    for subject in ["a1", "a2", "a3"] {
        post(&broker, &mut alice, &tx, &first_thread, subject).await;
    }
    for subject in ["b1", "b2", "b3"] {
        post(&broker, &mut alice, &tx, &second_thread, subject).await;
    }

    let first = broker
        .handle(
            CommsRequest::Inbox {
                remote: None,
                cwd: None,
                cursor: None,
                limit: Some(2),
                mark_read: false,
                since_micros: None,
            },
            &mut bob,
            &tx,
        )
        .await;
    let (first_messages, cursor) = match first {
        CommsResponse::Inbox {
            messages,
            unread,
            next_cursor: Some(cursor),
        } => {
            assert_eq!(unread, 4);
            (messages, cursor)
        }
        other => panic!("expected paginated inbox, got {other:?}"),
    };

    let second = broker
        .handle(
            CommsRequest::Inbox {
                remote: None,
                cwd: None,
                cursor: Some(cursor),
                limit: Some(2),
                mark_read: false,
                since_micros: None,
            },
            &mut bob,
            &tx,
        )
        .await;
    match second {
        CommsResponse::Inbox { messages, unread, .. } => {
            assert_eq!(unread, 2);
            assert!(
                first_messages
                    .iter()
                    .all(|first| messages.iter().all(|second| second.meta.id != first.meta.id)),
                "successive pages must not repeat messages"
            );
        }
        other => panic!("expected inbox page, got {other:?}"),
    }
}

/// The creator can archive; a non-creator member cannot. An archived thread drops out of active
/// listings.
#[tokio::test]
async fn creator_can_archive_but_member_cannot() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let mut bob = hello(&broker, &tx, "bob").await;

    let denied = broker
        .handle(CommsRequest::ThreadArchive { thread: thread.clone() }, &mut bob, &tx)
        .await;
    assert!(
        matches!(denied, CommsResponse::Error { code, .. } if code == "not_creator"),
        "a non-creator member must not archive"
    );

    let ok = broker
        .handle(CommsRequest::ThreadArchive { thread: thread.clone() }, &mut alice, &tx)
        .await;
    assert!(matches!(ok, CommsResponse::Ok));

    let active = match broker
        .handle(
            CommsRequest::ThreadList {
                remote: None,
                cwd: None,
                subject_contains: None,
                include_archived: false,
            },
            &mut alice,
            &tx,
        )
        .await
    {
        CommsResponse::Threads(t) => t,
        other => panic!("expected Threads, got {other:?}"),
    };
    assert!(active.is_empty(), "an archived thread is not in the active listing");

    let with_archived = match broker
        .handle(
            CommsRequest::ThreadList {
                remote: None,
                cwd: None,
                subject_contains: None,
                include_archived: true,
            },
            &mut alice,
            &tx,
        )
        .await
    {
        CommsResponse::Threads(t) => t,
        other => panic!("expected Threads, got {other:?}"),
    };
    assert_eq!(with_archived.len(), 1);
    assert!(!with_archived[0].active);
}

/// Only the creator may add / remove members.
#[tokio::test]
async fn only_creator_manages_membership() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let mut bob = hello(&broker, &tx, "bob").await;

    let denied = broker
        .handle(
            CommsRequest::ThreadAddMember {
                thread: thread.clone(),
                member: agent("carol"),
            },
            &mut bob,
            &tx,
        )
        .await;
    assert!(matches!(denied, CommsResponse::Error { code, .. } if code == "not_creator"));

    let ok = broker
        .handle(
            CommsRequest::ThreadAddMember {
                thread: thread.clone(),
                member: agent("carol"),
            },
            &mut alice,
            &tx,
        )
        .await;
    assert!(matches!(ok, CommsResponse::Ok));

    let members = match broker
        .handle(CommsRequest::ThreadMembers { thread: thread.clone() }, &mut alice, &tx)
        .await
    {
        CommsResponse::Members { members } => members,
        other => panic!("expected Members, got {other:?}"),
    };
    assert!(members.contains(&agent("carol")));
}
