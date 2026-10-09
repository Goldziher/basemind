//! Ack, subscription, and inbox-scan tests split from `daemon_tests.rs` to keep both source files
//! under the line cap.

use super::*;

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
                idempotency_key: None,
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

#[tokio::test]
async fn inbox_scan_is_bounded_by_the_page_limit_not_the_backlog() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(32);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let mut bob = hello(&broker, &tx, "bob").await;
    for n in 0..700 {
        post(&broker, &mut alice, &tx, &thread, &format!("m{n}")).await;
    }
    let read = |cursor| CommsRequest::Inbox {
        remote: None,
        cwd: None,
        cursor,
        limit: Some(1),
        mark_read: false,
        since_micros: None,
    };

    let CommsResponse::Inbox {
        messages,
        unread,
        next_cursor: Some(cursor),
    } = broker.handle(read(None), &mut bob, &tx).await
    else {
        panic!("expected a paginated inbox");
    };
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].meta.subject, "m0");
    // 1 returned + a 500-row slack scanned + 1 for the truncation marker; the true backlog is 699.
    assert_eq!(
        unread, 501,
        "the unread count is a lower bound once the scan cap is hit"
    );

    let CommsResponse::Inbox { messages, .. } = broker.handle(read(Some(cursor)), &mut bob, &tx).await else {
        panic!("expected the next page");
    };
    assert_eq!(
        messages[0].meta.subject, "m1",
        "the cursor resumes right after the first page"
    );
}

#[tokio::test]
async fn correlated_call_is_answered_with_a_reply_echoing_its_id() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut session = Session::default();
    let framed = broker
        .handle_framed(
            CommsRequest::Call {
                id: 41,
                request: Box::new(CommsRequest::Ping),
            },
            &mut session,
            &tx,
        )
        .await;
    assert_eq!(
        framed,
        CommsOut::Reply {
            id: 41,
            response: CommsResponse::Pong
        }
    );
    let bare = broker.handle_framed(CommsRequest::Ping, &mut session, &tx).await;
    assert_eq!(bare, CommsOut::Response(CommsResponse::Pong));
}
