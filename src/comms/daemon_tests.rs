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

    // Only subject → rejected.
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

    // Only the creator as a member (no explicit extra member) + subject → still one dimension.
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

    // subject + path → two dimensions, accepted.
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

    // Carol is neither a member nor path-matched.
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

    // Alice (the creator/member) sees it.
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

    // Bob was added as a member at thread_start, so both land in his inbox.
    let bob_inbox = inbox(&broker, &mut bob, &tx).await;
    assert_eq!(bob_inbox.len(), 2);

    // History returns them oldest-first.
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

    // Alice's own posts are excluded from her inbox.
    assert!(inbox(&broker, &mut alice, &tx).await.is_empty());
}

/// Inbox reflects ONLY joined threads: a message in a thread the agent has not joined never
/// surfaces.
#[tokio::test]
async fn inbox_reflects_only_joined_threads() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(64);
    let mut alice = hello(&broker, &tx, "alice").await;
    // Thread with alice + bob only.
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    post(&broker, &mut alice, &tx, &thread, "hello").await;

    // Carol is not a member.
    let mut carol = hello(&broker, &tx, "carol").await;
    assert!(
        inbox(&broker, &mut carol, &tx).await.is_empty(),
        "non-member sees nothing"
    );

    // After Carol joins, she starts seeing future posts.
    join(&broker, &mut carol, &tx, &thread).await;
    post(&broker, &mut alice, &tx, &thread, "after-join").await;
    let carol_inbox = inbox(&broker, &mut carol, &tx).await;
    assert!(carol_inbox.iter().any(|m| m.meta.subject == "after-join"));
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

    // Bob (member, not creator) cannot archive.
    let denied = broker
        .handle(CommsRequest::ThreadArchive { thread: thread.clone() }, &mut bob, &tx)
        .await;
    assert!(
        matches!(denied, CommsResponse::Error { code, .. } if code == "not_creator"),
        "a non-creator member must not archive"
    );

    // Alice (creator) can.
    let ok = broker
        .handle(CommsRequest::ThreadArchive { thread: thread.clone() }, &mut alice, &tx)
        .await;
    assert!(matches!(ok, CommsResponse::Ok));

    // Archived → drops out of the active listing.
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

    // But include_archived surfaces it.
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

    // Shared log is untouched.
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

    // Carol's inbox is unaffected.
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
    // A third thread alice is NOT a member of.
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

/// An inbox subscription with `thread: Some(t)` wakes ONLY for posts to `t`, staying silent for a
/// post to another joined thread.
#[tokio::test]
async fn subscribe_inbox_filter_restricts_to_one_thread() {
    let (_d, broker) = temp_broker();
    let (tx, mut rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread1 = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let thread2 = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let mut bob = hello(&broker, &tx, "bob").await;

    let sub_resp = broker
        .handle(
            CommsRequest::SubscribeInbox {
                thread: Some(thread1.clone()),
            },
            &mut alice,
            &tx,
        )
        .await;
    assert!(matches!(sub_resp, CommsResponse::Subscribed { .. }));

    post(&broker, &mut bob, &tx, &thread2, "other thread").await;
    assert!(
        matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "the filter must not wake on a different joined thread"
    );

    post(&broker, &mut bob, &tx, &thread1, "filtered thread").await;
    match rx.recv().await.expect("notification for the filtered thread") {
        CommsOut::Notification(CommsNotification::Message(meta)) => {
            assert_eq!(meta.thread, thread1, "the filter wakes on its own thread");
        }
        other => panic!("expected a Message notification, got {other:?}"),
    }
}

/// An inbox subscription never wakes on the subscriber's OWN post, mirroring `on_inbox`'s
/// self-exclusion.
#[tokio::test]
async fn subscribe_inbox_skips_self_authored() {
    let (_d, broker) = temp_broker();
    let (tx, mut rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let mut bob = hello(&broker, &tx, "bob").await;

    let sub_resp = broker
        .handle(CommsRequest::SubscribeInbox { thread: None }, &mut alice, &tx)
        .await;
    assert!(matches!(sub_resp, CommsResponse::Subscribed { .. }));

    post(&broker, &mut alice, &tx, &thread, "alice's own post").await;
    assert!(
        matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "a self-authored post must not wake the author's own inbox sink"
    );

    post(&broker, &mut bob, &tx, &thread, "bob's post").await;
    match rx.recv().await.expect("notification for bob's post") {
        CommsOut::Notification(CommsNotification::Message(meta)) => {
            assert_eq!(meta.subject, "bob's post");
        }
        other => panic!("expected a Message notification, got {other:?}"),
    }
}

#[tokio::test]
async fn idle_reaper_tracks_links_and_activity() {
    let (_d, broker) = temp_broker();

    assert!(broker.is_idle_for(Duration::ZERO).await);
    broker.link_connected();
    assert!(!broker.is_idle_for(Duration::ZERO).await);
    broker.link_disconnected();
    assert!(broker.is_idle_for(Duration::ZERO).await);
    assert!(!broker.is_idle_for(Duration::from_secs(3600)).await);
    broker.begin_drain().await;
    assert!(!broker.is_idle_for(Duration::ZERO).await);
}

/// A link whose task PANICS must still give its refcount back — otherwise `link_count` never returns
/// to zero, `is_idle_for` is false forever, and that daemon can never reap again for the rest of its
/// life. This is the immortal-daemon bug: one panicking request handler permanently pins the process.
///
/// It is exactly why the refcount is an RAII [`LinkGuard`] and not a `link_disconnected()` call after
/// the serve loop — a plain statement after the loop is skipped by an unwind, a `Drop` is not.
#[tokio::test]
async fn a_panicking_link_task_gives_its_refcount_back_and_the_daemon_can_still_reap() {
    use crate::comms::transport::{CommsLink, PeerCred, serve_link};

    /// A link that blows up the moment the serve loop polls it.
    struct PanickingLink;

    impl CommsLink for PanickingLink {
        async fn recv(&mut self) -> std::io::Result<Option<CommsRequest>> {
            panic!("handler blew up mid-request");
        }
        async fn send(&mut self, _out: CommsOut) -> std::io::Result<()> {
            Ok(())
        }
        fn peer_cred(&self) -> PeerCred {
            PeerCred::default()
        }
    }

    let (_d, broker) = temp_broker();
    let guard = broker.register_link();
    assert!(
        !broker.is_idle_for(Duration::ZERO).await,
        "a registered link means the daemon is not idle"
    );

    let joined = tokio::spawn(serve_link(broker.clone(), PanickingLink, guard)).await;
    assert!(joined.is_err(), "the link task must actually have panicked");

    assert!(
        broker.is_idle_for(Duration::ZERO).await,
        "the panicking link must have released its refcount on unwind — if it leaks, link_count \
         never returns to zero and this daemon is immortal"
    );
    assert!(
        broker.try_begin_idle_drain(Duration::ZERO).await,
        "and the reaper must still be able to drain it"
    );
}

/// Daemon-internal work with NO client attached still blocks the reap. This is the clause that keeps
/// the idle reaper from tearing down the process mid-blob-GC: that sweep holds no link, so without
/// the work refcount a daemon running it would look perfectly idle.
#[tokio::test]
async fn work_in_flight_blocks_the_idle_reap_even_with_no_links() {
    let (_d, broker) = temp_broker();

    assert!(broker.is_idle_for(Duration::ZERO).await, "no links, no work: idle");
    assert_eq!(broker.work_inflight(), 0);

    {
        let _working = broker.begin_work();
        assert_eq!(broker.work_inflight(), 1);
        assert!(
            !broker.is_idle_for(Duration::ZERO).await,
            "work in flight must defeat idleness even though zero links are connected"
        );
        assert!(
            !broker.try_begin_idle_drain(Duration::ZERO).await,
            "the reaper must refuse to start a drain while work is in flight"
        );
    }

    assert_eq!(broker.work_inflight(), 0, "the guard releases the count on drop");
    assert!(
        broker.is_idle_for(Duration::ZERO).await,
        "once the work finishes the daemon is idle again"
    );
    assert!(
        broker.try_begin_idle_drain(Duration::ZERO).await,
        "and now the reaper may claim the drain"
    );
    assert!(
        !broker.try_begin_idle_drain(Duration::ZERO).await,
        "only one caller ever owns the drain — a second attempt is a no-op"
    );
}

/// The destructive global blob GC must not sweep while a rescan is in flight: a rescan writes new
/// content-addressed blobs before its `index.msgpack` (which the GC reference-counts) is rewritten,
/// so a mid-rescan sweep would see those blobs as orphans and reap them. `on_rescan` holds the
/// blob-GC READ lock for the whole scan; `run_blob_gc` takes the WRITE lock — so the sweep blocks
/// until the rescan releases.
#[tokio::test]
async fn blob_gc_waits_for_an_in_flight_rescan() {
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();

    // Hold the READ side exactly as `on_rescan` does, simulating a scan in flight.
    let rescan_guard = broker.blob_gc_lock.read().await;

    // The GC sweep must block on the WRITE side while the read is held.
    let mut gc = std::pin::pin!(broker.run_blob_gc());
    tokio::select! {
        biased;
        _ = &mut gc => panic!("blob GC swept while a rescan held the blob-GC read lock"),
        _ = tokio::time::sleep(Duration::from_millis(150)) => {}
    }

    // Once the rescan releases the read lock, the sweep proceeds.
    drop(rescan_guard);
    gc.await.expect("blob GC runs once no rescan holds the read lock");
}

/// End-to-end correctness (not just lock timing): racing a real full rescan against the destructive
/// global blob sweep, repeatedly, must never leave the index pointing at a reaped blob. A rescan
/// writes fresh content-addressed blobs but only rewrites `index.msgpack` (which the sweep
/// reference-counts) at completion, so its just-written blobs are unreferenced for the whole scan;
/// without the `blob_gc_lock` serialization a sweep landing mid-scan would reap them. The invariant
/// checked here is the outcome, which holds under any interleaving: every blob the final index
/// references still exists on disk.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_rescan_and_blob_gc_never_reaps_a_referenced_blob() {
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);

    let ws = tempfile::tempdir().expect("workspace");
    for i in 0..12 {
        std::fs::write(
            ws.path().join(format!("m{i}.rs")),
            format!("pub fn f{i}() -> u32 {{ {i} }}\npub struct S{i};\n"),
        )
        .expect("write source");
    }
    let root = ws.path().to_path_buf();

    // Interleave the two under load: each round races a full rescan against a full sweep.
    for _ in 0..6 {
        let mut session = Session::default();
        let rescan = broker.handle(
            CommsRequest::Rescan {
                root: root.clone(),
                paths: None,
                full: true,
                embed: false,
            },
            &mut session,
            &tx,
        );
        let (rescan_resp, gc_res) = tokio::join!(rescan, broker.run_blob_gc());
        assert!(
            matches!(rescan_resp, CommsResponse::Rescanned { .. }),
            "each raced rescan must succeed, got {rescan_resp:?}"
        );
        gc_res.expect("blob GC must succeed under a concurrent rescan");
    }

    // Every blob the surviving index references must still be present — no live blob was reaped.
    let basemind_dir = crate::store::workspace_cache_dir(&root);
    let referenced = crate::store_gc::collect_referenced_hashes(&basemind_dir).expect("collect referenced hashes");
    assert!(
        !referenced.is_empty(),
        "the scanned workspace must reference at least one blob"
    );
    let blobs_dir = crate::store::global_blobs_dir();
    for stem in &referenced {
        let prefix = format!("{stem}.");
        let present = std::fs::read_dir(&blobs_dir)
            .expect("read blobs dir")
            .flatten()
            .any(|entry| entry.file_name().to_str().is_some_and(|name| name.starts_with(&prefix)));
        assert!(present, "referenced blob {stem} was reaped by a concurrent GC sweep");
    }
}

/// The system auto-archive sweep flips an idle active thread; a fresh one stays active.
#[tokio::test]
async fn archive_idle_threads_flips_stale_active_threads() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;

    // Backdate its activity well past the TTL by rewriting the stored record.
    let mut record = broker.store.get_thread(&thread).unwrap().unwrap();
    record.last_activity = now_micros() - 30 * 24 * 60 * 60 * 1_000_000;
    broker.store.put_thread(&record).unwrap();

    let archived = broker.archive_idle_threads(THREAD_IDLE_TTL).expect("archive");
    assert_eq!(archived, 1);
    assert!(!broker.store.get_thread(&thread).unwrap().unwrap().active);
}

#[tokio::test]
async fn rescan_request_indexes_a_workspace_and_surfaces_it_as_accessed() {
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut session = Session::default();

    let ws = tempfile::tempdir().expect("workspace");
    std::fs::write(ws.path().join("lib.rs"), "pub fn indexed() -> u32 { 7 }\n").expect("write source");

    let resp = broker
        .handle(
            CommsRequest::Rescan {
                root: ws.path().to_path_buf(),
                paths: None,
                full: false,
                embed: false,
            },
            &mut session,
            &tx,
        )
        .await;
    match resp {
        CommsResponse::Rescanned { scanned, updated, .. } => {
            assert_eq!(scanned, 1);
            assert_eq!(updated, 1);
        }
        other => panic!("expected Rescanned, got {other:?}"),
    }

    let accessed = broker.handle(CommsRequest::AccessedPaths, &mut session, &tx).await;
    match accessed {
        CommsResponse::Accessed { workspaces } => {
            assert_eq!(workspaces.len(), 1);
            assert_eq!(workspaces[0].root, ws.path());
        }
        other => panic!("expected Accessed, got {other:?}"),
    }
}
