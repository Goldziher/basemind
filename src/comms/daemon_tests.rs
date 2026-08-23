//! Unit tests for the comms [`Broker`](super::Broker). Split out of `daemon.rs` (via a
//! `#[cfg(test)] #[path = "daemon_tests.rs"] mod tests;` declaration) to keep `daemon.rs` under
//! the 1000-line `rust-max-lines` cap. `super` here resolves to the `daemon` module.

use super::*;
use crate::comms::model::MessageBody;
use crate::comms::model::message_reference;
use crate::comms::store;

#[path = "daemon_lifecycle_tests.rs"]
mod lifecycle_tests;

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

/// `AckInbox { message_ids }` advances ONLY the acking agent's cursor.
#[tokio::test]
async fn ack_by_ids_advances_only_the_acking_agents_cursor() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(64);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob", "carol"]).await;
    let mut bob = hello(&broker, &tx, "bob").await;
    let mut carol = hello(&broker, &tx, "carol").await;

    let m1 = post(&broker, &mut alice, &tx, &thread, "first").await;
    let _m2 = post(&broker, &mut alice, &tx, &thread, "second").await;

    assert_eq!(inbox(&broker, &mut bob, &tx).await.len(), 2);
    let resp = broker
        .handle(
            CommsRequest::AckInbox {
                message_ids: vec![m1.clone()],
                thread: None,
                to_seq: None,
            },
            &mut bob,
            &tx,
        )
        .await;
    match resp {
        CommsResponse::Acked {
            acked,
            cursors_advanced,
        } => {
            assert_eq!(acked, 1);
            assert_eq!(cursors_advanced, vec![(thread.as_str().to_string(), 1)]);
        }
        other => panic!("expected Acked, got {other:?}"),
    }

    let bob_after = inbox(&broker, &mut bob, &tx).await;
    assert_eq!(bob_after.len(), 1);
    assert_eq!(bob_after[0].meta.subject, "second");

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
        CommsResponse::History { messages, .. } => assert_eq!(messages.len(), 2),
        other => panic!("expected History, got {other:?}"),
    }

    assert_eq!(inbox(&broker, &mut carol, &tx).await.len(), 2);
}

/// The bulk `thread` + `to_seq` mode clears the whole thread from the agent's inbox.
#[tokio::test]
async fn ack_to_seq_bulk_clears_thread() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(64);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let mut bob = hello(&broker, &tx, "bob").await;
    for i in 0..3 {
        post(&broker, &mut alice, &tx, &thread, &format!("m{i}")).await;
    }
    assert_eq!(inbox(&broker, &mut bob, &tx).await.len(), 3);

    let resp = broker
        .handle(
            CommsRequest::AckInbox {
                message_ids: vec![],
                thread: Some(thread.clone()),
                to_seq: Some(3),
            },
            &mut bob,
            &tx,
        )
        .await;
    assert!(matches!(resp, CommsResponse::Acked { acked: 0, .. }));
    assert!(inbox(&broker, &mut bob, &tx).await.is_empty());
}

/// An ack with neither mode supplied is rejected with a stable `empty_ack` code.
#[tokio::test]
async fn ack_with_no_input_is_rejected() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut bob = hello(&broker, &tx, "bob").await;
    let resp = broker
        .handle(
            CommsRequest::AckInbox {
                message_ids: vec![],
                thread: None,
                to_seq: None,
            },
            &mut bob,
            &tx,
        )
        .await;
    assert!(matches!(resp, CommsResponse::Error { code, .. } if code == "empty_ack"));
}

#[tokio::test]
async fn message_body_requires_thread_membership() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let message_id = post(&broker, &mut alice, &tx, &thread, "private").await;
    let mut mallory = hello(&broker, &tx, "mallory").await;

    let response = broker
        .handle(CommsRequest::GetBody { message_id }, &mut mallory, &tx)
        .await;

    assert!(matches!(response, CommsResponse::Error { code, .. } if code == "not_member"));
}

#[tokio::test]
async fn ack_rejects_messages_from_threads_the_agent_has_not_joined() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let message_id = post(&broker, &mut alice, &tx, &thread, "private").await;
    let mut mallory = hello(&broker, &tx, "mallory").await;

    let response = broker
        .handle(
            CommsRequest::AckInbox {
                message_ids: vec![message_id],
                thread: None,
                to_seq: None,
            },
            &mut mallory,
            &tx,
        )
        .await;

    assert!(matches!(response, CommsResponse::Error { code, .. } if code == "not_member"));
    assert_eq!(broker.store.read_cursor(&agent("mallory"), &thread).expect("cursor"), 0);
}

#[tokio::test]
async fn subscribe_then_post_fans_out_notification() {
    let (_d, broker) = temp_broker();
    let (tx, mut rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;

    let sub_resp = broker
        .handle(CommsRequest::Subscribe { thread: thread.clone() }, &mut alice, &tx)
        .await;
    assert!(matches!(sub_resp, CommsResponse::Subscribed { .. }));
    assert_eq!(broker.subscriber_count(), 1);

    let mut bob = hello(&broker, &tx, "bob").await;
    let posted = broker
        .handle(
            CommsRequest::ThreadPost {
                thread: thread.clone(),
                subject: "hi".to_string(),
                tags: vec![],
                reply_to: None,
                body: b"hello".to_vec(),
            },
            &mut bob,
            &tx,
        )
        .await;
    assert!(matches!(posted, CommsResponse::Posted { .. }));

    let note = rx.recv().await.expect("notification");
    match note {
        CommsOut::Notification(CommsNotification::Message(meta)) => {
            assert_eq!(meta.subject, "hi");
            assert_eq!(meta.thread, thread);
        }
        other => panic!("expected a Message notification, got {other:?}"),
    }
}

/// A passive inbox subscription (no `thread` filter) wakes on a post to EITHER of two joined
/// threads, but stays silent for a post to a thread the subscriber is not a member of.
#[tokio::test]
async fn subscribe_inbox_wakes_on_any_joined_thread() {
    let (_d, broker) = temp_broker();
    let (tx, mut rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread1 = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let thread2 = start_thread(&broker, &mut alice, &tx, &["carol"]).await;
    let mut bob = hello(&broker, &tx, "bob").await;
    let mut carol = hello(&broker, &tx, "carol").await;
    let thread3 = start_thread(&broker, &mut bob, &tx, &["carol"]).await;

    let sub_resp = broker
        .handle(CommsRequest::SubscribeInbox { thread: None }, &mut alice, &tx)
        .await;
    assert!(matches!(sub_resp, CommsResponse::Subscribed { .. }));

    post(&broker, &mut bob, &tx, &thread1, "from thread1").await;
    match rx.recv().await.expect("notification for thread1") {
        CommsOut::Notification(CommsNotification::Message(meta)) => {
            assert_eq!(meta.thread, thread1, "wakes on any joined thread (thread1)");
        }
        other => panic!("expected a Message notification, got {other:?}"),
    }

    post(&broker, &mut carol, &tx, &thread2, "from thread2").await;
    match rx.recv().await.expect("notification for thread2") {
        CommsOut::Notification(CommsNotification::Message(meta)) => {
            assert_eq!(meta.thread, thread2, "wakes on any joined thread (thread2)");
        }
        other => panic!("expected a Message notification, got {other:?}"),
    }

    post(&broker, &mut carol, &tx, &thread3, "from thread3").await;
    assert!(
        matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "a post to a thread alice never joined must not wake her inbox sink"
    );
}

/// An inbox waiter learns about a newly-created path-scoped thread without joining it or polling
/// `thread_list`. Discovery notifications carry thread metadata only; message delivery still
/// requires explicit membership.
#[tokio::test]
async fn subscribe_inbox_notifies_matching_discoverable_thread() {
    let (_d, broker) = temp_broker();
    let (tx, mut rx) = mpsc::channel(8);
    let workspace = tempfile::tempdir().expect("workspace");
    let mut bob = Session::default();
    broker
        .handle(
            CommsRequest::Hello {
                agent: agent("bob"),
                proto_ver: PROTO_VER,
                remote: None,
                cwd: Some(workspace.path().to_path_buf()),
            },
            &mut bob,
            &tx,
        )
        .await;
    let response = broker
        .handle(CommsRequest::SubscribeInbox { thread: None }, &mut bob, &tx)
        .await;
    assert!(matches!(response, CommsResponse::Subscribed { .. }));

    let mut alice = hello(&broker, &tx, "alice").await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        broker.handle(
            CommsRequest::ThreadStart {
                subject: Some("build coordination".to_string()),
                path: Some(format!("{}/**", workspace.path().display())),
                members: Vec::new(),
            },
            &mut alice,
            &tx,
        ),
    )
    .await
    .expect("thread creation must not block");
    let created = match response {
        CommsResponse::Thread(thread) => thread,
        other => panic!("expected Thread, got {other:?}"),
    };

    let note = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("matching subscriber must be notified")
        .expect("discovery notification");
    assert_eq!(
        note,
        CommsOut::Notification(CommsNotification::ThreadDiscovered(created))
    );
}

#[path = "daemon_maintenance_tests.rs"]
mod maintenance_tests;
