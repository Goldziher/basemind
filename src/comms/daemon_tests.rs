//! Unit tests for the comms [`Broker`](super::Broker). Split out of `daemon.rs` (via a
//! `#[cfg(test)] #[path = "daemon_tests.rs"] mod tests;` declaration) to keep `daemon.rs` under
//! the 1000-line `rust-max-lines` cap. `super` here resolves to the `daemon` module.

use super::*;

fn temp_broker() -> (tempfile::TempDir, Arc<Broker>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(CommsStore::open(dir.path()).expect("store"));
    (dir, Arc::new(Broker::new(store)))
}

fn agent(s: &str) -> AgentId {
    AgentId::parse(s).expect("agent")
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
                session_id: None,
                parent_agent: None,
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
            CommsRequest::Post {
                room: RoomId::parse("r").expect("r"),
                subject: "s".to_string(),
                tags: vec![],
                reply_to: None,
                scope: vec![],
                body: b"b".to_vec(),
            },
            &mut session,
            &tx,
        )
        .await;
    assert!(matches!(resp, CommsResponse::Error { code, .. } if code == "no_hello"));
}

#[tokio::test]
async fn subscribe_then_post_fans_out_notification() {
    let (_d, broker) = temp_broker();
    let (tx, mut rx) = mpsc::channel(8);
    let mut session = Session::default();
    // Hello with no cwd → Global default room.
    broker
        .handle(
            CommsRequest::Hello {
                agent: agent("a"),
                proto_ver: PROTO_VER,
                remote: None,
                cwd: None,
                session_id: None,
                parent_agent: None,
            },
            &mut session,
            &tx,
        )
        .await;
    let room = RoomId::parse("r").expect("r");
    broker
        .handle(
            CommsRequest::CreateRoom {
                room: room.clone(),
                scope: RoomScope::Global,
                title: None,
            },
            &mut session,
            &tx,
        )
        .await;
    let sub_resp = broker
        .handle(
            CommsRequest::Subscribe { room: room.clone() },
            &mut session,
            &tx,
        )
        .await;
    assert!(matches!(sub_resp, CommsResponse::Subscribed { .. }));
    assert_eq!(broker.subscriber_count(), 1);

    let posted = broker
        .handle(
            CommsRequest::Post {
                room: room.clone(),
                subject: "hi".to_string(),
                tags: vec![],
                reply_to: None,
                scope: vec![],
                body: b"hello".to_vec(),
            },
            &mut session,
            &tx,
        )
        .await;
    assert!(matches!(posted, CommsResponse::Posted { .. }));

    let note = rx.recv().await.expect("notification");
    match note {
        CommsOut::Notification(CommsNotification::Message(meta)) => {
            assert_eq!(meta.subject, "hi");
            assert_eq!(meta.room, room);
        }
        other => panic!("expected a Message notification, got {other:?}"),
    }
}

#[test]
fn sanitize_id_maps_to_alphabet() {
    assert_eq!(sanitize_id("github.com/foo/bar"), "github.com-foo-bar");
    assert!(RoomId::parse(sanitize_id("a b!c")).is_ok());
}

/// Drive Hello → CreateRoom → Join for an agent, returning a session bound to it.
async fn hello_join(
    broker: &Broker,
    tx: &mpsc::Sender<CommsOut>,
    who: &str,
    room: &RoomId,
) -> Session {
    let mut session = Session::default();
    broker
        .handle(
            CommsRequest::Hello {
                agent: agent(who),
                proto_ver: PROTO_VER,
                remote: None,
                cwd: None,
                session_id: None,
                parent_agent: None,
            },
            &mut session,
            tx,
        )
        .await;
    broker
        .handle(
            CommsRequest::CreateRoom {
                room: room.clone(),
                scope: RoomScope::Global,
                title: None,
            },
            &mut session,
            tx,
        )
        .await;
    broker
        .handle(CommsRequest::Join { room: room.clone() }, &mut session, tx)
        .await;
    session
}

async fn post(
    broker: &Broker,
    session: &mut Session,
    tx: &mpsc::Sender<CommsOut>,
    room: &RoomId,
    subject: &str,
) -> String {
    match broker
        .handle(
            CommsRequest::Post {
                room: room.clone(),
                subject: subject.to_string(),
                tags: vec![],
                reply_to: None,
                scope: vec![],
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

async fn inbox(
    broker: &Broker,
    session: &mut Session,
    tx: &mpsc::Sender<CommsOut>,
) -> Vec<SeqMeta> {
    match broker
        .handle(
            CommsRequest::Inbox {
                remote: None,
                cwd: None,
                cursor: None,
                limit: None,
                mark_read: false,
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

/// `AckInbox { message_ids }` advances ONLY the acking agent's cursor: the acked messages
/// vanish from that agent's next inbox read, the shared `History` log still returns them, and
/// a second agent's inbox is untouched.
#[tokio::test]
async fn ack_by_ids_advances_only_the_acking_agents_cursor() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(64);
    let room = RoomId::parse("r").expect("r");

    // Alice posts two messages; Bob and Carol are inbox readers.
    let mut alice = hello_join(&broker, &tx, "alice", &room).await;
    let mut bob = hello_join(&broker, &tx, "bob", &room).await;
    let mut carol = hello_join(&broker, &tx, "carol", &room).await;
    let m1 = post(&broker, &mut alice, &tx, &room, "first").await;
    let _m2 = post(&broker, &mut alice, &tx, &room, "second").await;

    // Bob sees both, acks the first by id.
    assert_eq!(inbox(&broker, &mut bob, &tx).await.len(), 2);
    let resp = broker
        .handle(
            CommsRequest::AckInbox {
                message_ids: vec![m1.clone()],
                room: None,
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
            assert_eq!(cursors_advanced, vec![("r".to_string(), 1)]);
        }
        other => panic!("expected Acked, got {other:?}"),
    }

    // Bob's inbox no longer shows the acked message; only "second" remains.
    let bob_after = inbox(&broker, &mut bob, &tx).await;
    assert_eq!(bob_after.len(), 1);
    assert_eq!(bob_after[0].meta.subject, "second");

    // The shared log is intact — History still returns both messages.
    match broker
        .handle(
            CommsRequest::History {
                room: room.clone(),
                cursor: None,
                limit: None,
            },
            &mut bob,
            &tx,
        )
        .await
    {
        CommsResponse::History { messages, .. } => assert_eq!(messages.len(), 2),
        other => panic!("expected History, got {other:?}"),
    }

    // Carol's inbox is unaffected by Bob's ack — per-agent isolation.
    assert_eq!(inbox(&broker, &mut carol, &tx).await.len(), 2);
}

/// The bulk `room` + `to_seq` mode advances a room's cursor straight to `to_seq`, clearing
/// the whole room from the agent's inbox without enumerating ids.
#[tokio::test]
async fn ack_to_seq_bulk_clears_room() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(64);
    let room = RoomId::parse("r").expect("r");
    let mut alice = hello_join(&broker, &tx, "alice", &room).await;
    let mut bob = hello_join(&broker, &tx, "bob", &room).await;
    for i in 0..3 {
        post(&broker, &mut alice, &tx, &room, &format!("m{i}")).await;
    }
    assert_eq!(inbox(&broker, &mut bob, &tx).await.len(), 3);

    let resp = broker
        .handle(
            CommsRequest::AckInbox {
                message_ids: vec![],
                room: Some(room.clone()),
                to_seq: Some(3),
            },
            &mut bob,
            &tx,
        )
        .await;
    assert!(matches!(resp, CommsResponse::Acked { acked: 0, .. }));
    assert!(inbox(&broker, &mut bob, &tx).await.is_empty());
}

/// A `to_seq` at or below the current cursor (e.g. `to_seq = 0`, or re-acking an already-acked
/// position) must report an empty `cursors_advanced` — never a phantom advance.
#[tokio::test]
async fn ack_does_not_report_phantom_advance() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(64);
    let room = RoomId::parse("r").expect("r");
    let mut alice = hello_join(&broker, &tx, "alice", &room).await;
    let mut bob = hello_join(&broker, &tx, "bob", &room).await;
    post(&broker, &mut alice, &tx, &room, "m0").await;

    // to_seq = 0 cannot advance past the default cursor of 0.
    let resp = broker
        .handle(
            CommsRequest::AckInbox {
                message_ids: vec![],
                room: Some(room.clone()),
                to_seq: Some(0),
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
            assert_eq!(acked, 0);
            assert!(
                cursors_advanced.is_empty(),
                "to_seq=0 must not report a phantom advance"
            );
        }
        other => panic!("expected Acked, got {other:?}"),
    }

    // Advance to seq 1, then re-ack the same seq: no further advance is reported.
    let _ = broker
        .handle(
            CommsRequest::AckInbox {
                message_ids: vec![],
                room: Some(room.clone()),
                to_seq: Some(1),
            },
            &mut bob,
            &tx,
        )
        .await;
    let resp2 = broker
        .handle(
            CommsRequest::AckInbox {
                message_ids: vec![],
                room: Some(room.clone()),
                to_seq: Some(1),
            },
            &mut bob,
            &tx,
        )
        .await;
    match resp2 {
        CommsResponse::Acked {
            cursors_advanced, ..
        } => assert!(
            cursors_advanced.is_empty(),
            "re-acking an already-acked seq must not report an advance"
        ),
        other => panic!("expected Acked, got {other:?}"),
    }
}

/// An ack with neither mode supplied is rejected with a stable `empty_ack` code.
#[tokio::test]
async fn ack_with_no_input_is_rejected() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let room = RoomId::parse("r").expect("r");
    let mut bob = hello_join(&broker, &tx, "bob", &room).await;
    let resp = broker
        .handle(
            CommsRequest::AckInbox {
                message_ids: vec![],
                room: None,
                to_seq: None,
            },
            &mut bob,
            &tx,
        )
        .await;
    assert!(matches!(resp, CommsResponse::Error { code, .. } if code == "empty_ack"));
}

#[tokio::test]
async fn idle_reaper_tracks_links_and_activity() {
    use std::time::Duration;
    let (_d, broker) = temp_broker();

    // A fresh, unused broker is immediately idle past a zero window — the reaper would
    // self-terminate a daemon that was spawned but never used.
    assert!(
        broker.is_idle_for(Duration::ZERO).await,
        "an unused broker is idle past a zero window"
    );

    // A connected link is never idle, even past the window.
    broker.link_connected();
    assert!(
        !broker.is_idle_for(Duration::ZERO).await,
        "a connected link keeps the daemon alive"
    );

    // After the last link closes the broker is idle again.
    broker.link_disconnected();
    assert!(
        broker.is_idle_for(Duration::ZERO).await,
        "the broker is idle once every link has closed"
    );

    // Recent activity (the disconnect just touched it) keeps it out of a real reap window.
    assert!(
        !broker.is_idle_for(Duration::from_secs(3600)).await,
        "recent activity keeps the broker out of the reap window"
    );

    // A draining broker is never reaped — the clean-shutdown path is already underway.
    broker.begin_drain().await;
    assert!(
        !broker.is_idle_for(Duration::ZERO).await,
        "a draining broker is never reaped"
    );
}

/// Drive a `Hello` carrying a `session_id` and return the bound session. No cwd → the base
/// chain is path-empty, so only the explicit session room can match.
async fn hello_session(
    broker: &Broker,
    tx: &mpsc::Sender<CommsOut>,
    who: &str,
    session_id: Option<&str>,
) -> Session {
    let mut session = Session::default();
    broker
        .handle(
            CommsRequest::Hello {
                agent: agent(who),
                proto_ver: PROTO_VER,
                remote: None,
                cwd: None,
                session_id: session_id.map(|s| s.to_string()),
                parent_agent: None,
            },
            &mut session,
            tx,
        )
        .await;
    session
}

/// An agent whose `Hello` carries the room's `session_id` is auto-joined to a
/// `RoomScope::Session` room; an agent with a different / absent `session_id` is not. Verified
/// through the broker: a post by the matching parent lands in the matching child's inbox, and
/// never in the non-matching agent's inbox.
#[tokio::test]
async fn session_scoped_room_auto_joins_only_matching_session() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(64);
    let room = RoomId::parse("session-abc").expect("room");

    // A session-scoped room exists before anyone connects.
    broker
        .handle(
            CommsRequest::CreateRoom {
                room: room.clone(),
                scope: RoomScope::Session("abc".to_string()),
                title: None,
            },
            &mut Session::default(),
            &tx,
        )
        .await;

    // Parent + child share session "abc"; outsider has a different session.
    let mut parent = hello_session(&broker, &tx, "parent", Some("abc")).await;
    let mut child = hello_session(&broker, &tx, "child", Some("abc")).await;
    let mut outsider = hello_session(&broker, &tx, "outsider", Some("zzz")).await;

    // The matching session's agents were auto-joined to the room; the outsider was not.
    let subs = broker.store.subscribers(&room).expect("subs");
    assert!(
        subs.contains(&agent("parent")),
        "parent auto-joins session room"
    );
    assert!(
        subs.contains(&agent("child")),
        "child auto-joins session room"
    );
    assert!(
        !subs.contains(&agent("outsider")),
        "a different session id must not auto-join"
    );

    // Parent posts → child sees it in the inbox, outsider does not.
    let _ = post(&broker, &mut parent, &tx, &room, "hello-child").await;
    let child_inbox = inbox(&broker, &mut child, &tx).await;
    assert_eq!(child_inbox.len(), 1);
    assert_eq!(child_inbox[0].meta.subject, "hello-child");
    assert!(
        inbox(&broker, &mut outsider, &tx).await.is_empty(),
        "outsider's inbox stays empty — never joined the session room"
    );
}

/// An agent that presents NO `session_id` does not auto-join a session-scoped room even when
/// the room already exists.
#[tokio::test]
async fn absent_session_id_does_not_auto_join_session_room() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let room = RoomId::parse("session-abc").expect("room");
    broker
        .handle(
            CommsRequest::CreateRoom {
                room: room.clone(),
                scope: RoomScope::Session("abc".to_string()),
                title: None,
            },
            &mut Session::default(),
            &tx,
        )
        .await;

    let _sessionless = hello_session(&broker, &tx, "lonely", None).await;
    let subs = broker.store.subscribers(&room).expect("subs");
    assert!(
        !subs.contains(&agent("lonely")),
        "an agent with no session id must not join a session-scoped room"
    );
}
