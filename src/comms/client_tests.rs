use super::*;

/// Dialing a socket path that no daemon is listening on must surface an actionable error
/// naming that the comms daemon is not running and how to start it — not a bare OS string.
#[tokio::test]
async fn dial_missing_socket_reports_daemon_not_running_with_start_hint() {
    let dir = std::env::temp_dir().join(format!("basemind-comms-test-{}", std::process::id()));
    let paths = CommsPaths {
        comms_dir: dir.clone(),
        socket_path: dir.join("definitely-absent.sock"),
    };

    let err = match CommsClient::dial(&paths).await {
        Ok(_) => panic!("dialing an absent socket must fail"),
        Err(err) => err,
    };
    let msg = err.to_string();

    assert!(
        msg.contains("comms daemon is not running"),
        "error should name that the daemon is not running, got: {msg}"
    );
    assert!(
        msg.contains("basemind comms start"),
        "error should name the start command, got: {msg}"
    );
    assert!(
        !msg.starts_with("comms transport error: No such file or directory"),
        "error must not be the bare OS string, got: {msg}"
    );
}

/// Nothing drains `pending_notifications` for a client that only issues ordinary requests, so
/// the buffer itself has to be the bound: past the cap the queue must stay at the cap and shed
/// its OLDEST entries, never grow with the broker's push volume.
#[test]
fn buffering_notifications_past_the_cap_holds_at_the_cap() {
    let mut queue = std::collections::VecDeque::new();

    for _ in 0..PENDING_NOTIFICATION_CAP {
        buffer_notification(&mut queue, CommsNotification::Shutdown);
    }
    assert_eq!(
        queue.len(),
        PENDING_NOTIFICATION_CAP,
        "the cap is reached but not exceeded"
    );

    for _ in 0..(PENDING_NOTIFICATION_CAP * 3) {
        buffer_notification(&mut queue, CommsNotification::Shutdown);
    }
    assert_eq!(
        queue.len(),
        PENDING_NOTIFICATION_CAP,
        "four times the cap of pushes must still leave exactly the cap resident"
    );
}

/// The eviction has to be oldest-first: the newest notifications are the ones a `wait` caller
/// would act on, so shedding those instead would make the bound cost correctness. The single
/// `Message` is enqueued FIRST, so it is the one entry the cap must drop.
#[test]
fn buffering_evicts_the_oldest_notification_first() {
    let mut queue = std::collections::VecDeque::new();

    buffer_notification(&mut queue, CommsNotification::Message(test_message_meta()));
    for _ in 0..(PENDING_NOTIFICATION_CAP - 1) {
        buffer_notification(&mut queue, CommsNotification::Shutdown);
    }
    assert!(
        matches!(queue.front(), Some(CommsNotification::Message(_))),
        "precondition: the queue is exactly full and the Message is still the oldest entry"
    );

    buffer_notification(&mut queue, CommsNotification::Shutdown);

    assert_eq!(queue.len(), PENDING_NOTIFICATION_CAP, "the cap still holds");
    assert!(
        matches!(queue.front(), Some(CommsNotification::Shutdown)),
        "the oldest entry (the Message) must be the one dropped"
    );
    assert!(
        !queue.iter().any(|n| matches!(n, CommsNotification::Message(_))),
        "the evicted entry must be gone from the queue entirely"
    );
}

/// A scripted broker on a Unix socket: answers `Hello` bare, then each correlated `Call` in
/// order, sleeping `delays[i]` before reply `i`. Returns the paths to dial.
fn scripted_broker(delays: Vec<std::time::Duration>) -> (CommsPaths, tokio::task::JoinHandle<()>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("bm-client-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&dir).expect("dir");
    let socket_path = dir.join("c.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind");
    let paths = CommsPaths {
        comms_dir: dir.clone(),
        socket_path,
    };
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut codec = LengthDelimitedCodec::new();
        let mut buf = BytesMut::new();
        let mut served = 0usize;
        loop {
            let frame = loop {
                if let Some(f) = codec.decode(&mut buf).expect("decode") {
                    break f;
                }
                if stream.read_buf(&mut buf).await.expect("read") == 0 {
                    return;
                }
            };
            let req: CommsRequest = rmp_serde::from_slice(&frame).expect("req");
            let out = match req {
                CommsRequest::Hello { .. } => CommsOut::Response(CommsResponse::Welcome {
                    proto_ver: PROTO_VER,
                    daemon_version: "test".to_string(),
                }),
                CommsRequest::Call { id, request } => {
                    if let Some(d) = delays.get(served) {
                        tokio::time::sleep(*d).await;
                    }
                    served += 1;
                    let response = match *request {
                        CommsRequest::ListAgents { .. } => CommsResponse::Agents(Vec::new()),
                        CommsRequest::AccessedPaths => CommsResponse::Accessed { workspaces: Vec::new() },
                        other => panic!("unscripted request {other:?}"),
                    };
                    CommsOut::Reply { id, response }
                }
                other => panic!("bare request {other:?}"),
            };
            let body = rmp_serde::to_vec_named(&out).expect("encode");
            let mut framed = BytesMut::new();
            codec.encode(Bytes::from(body), &mut framed).expect("frame");
            stream.write_all(&framed).await.expect("write");
        }
    });
    (paths, handle, dir)
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// The reported failure: a request abandoned after its frame was written (the delivery-notice
/// probe's 200 ms timeout) leaves its reply on the socket. The next request must still get ITS
/// answer, not the stale frame ("unexpected response shape").
#[tokio::test]
async fn dropped_request_does_not_poison_the_next_response() {
    let (paths, server, dir) = scripted_broker(vec![std::time::Duration::from_millis(200)]);
    let agent = AgentId::parse("agent-1".to_string()).expect("agent");
    let mut client = CommsClient::connect_with_respawn(&paths, agent, None, None, |_| Ok(()))
        .await
        .expect("connect");

    let abandoned = tokio::time::timeout(std::time::Duration::from_millis(30), client.list_agents(None)).await;
    assert!(abandoned.is_err(), "the first request must be cut off mid-flight");

    let workspaces = client
        .accessed_paths()
        .await
        .expect("second request gets its own reply");
    assert!(workspaces.is_empty());

    server.abort();
    let _ = std::fs::remove_dir_all(dir);
}

/// A broker that stops answering must surface a retryable `broker unresponsive` error within the
/// bound rather than hanging the caller forever, and the client must still work afterwards.
#[tokio::test]
async fn silent_broker_yields_a_retryable_unresponsive_error() {
    let (paths, server, dir) = scripted_broker(vec![std::time::Duration::from_secs(30)]);
    let agent = AgentId::parse("agent-1".to_string()).expect("agent");
    let mut client = CommsClient::connect_with_respawn(&paths, agent, None, None, |_| Ok(()))
        .await
        .expect("connect")
        .with_request_timeout(std::time::Duration::from_millis(100));

    let started = std::time::Instant::now();
    let err = client.list_agents(None).await.expect_err("no reply must time out");
    assert!(
        matches!(
            err,
            CommsClientError::Unresponsive {
                what: "list_agents",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(err.to_string().contains("broker unresponsive"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));

    server.abort();
    let _ = std::fs::remove_dir_all(dir);
}

/// A daemon that accepts the connection but never answers `Hello` must fail the connect inside
/// the handshake bound.
#[tokio::test]
async fn handshake_against_a_mute_listener_times_out() {
    let dir = std::env::temp_dir().join(format!("bm-mute-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&dir).expect("dir");
    let socket_path = dir.join("m.sock");
    let _listener = tokio::net::UnixListener::bind(&socket_path).expect("bind");
    let paths = CommsPaths {
        comms_dir: dir.clone(),
        socket_path,
    };
    let agent = AgentId::parse("agent-1".to_string()).expect("agent");
    let Err(err) = CommsClient::connect_bounded(
        &paths,
        agent,
        None,
        None,
        |_| Ok(()),
        std::time::Duration::from_millis(100),
    )
    .await
    else {
        panic!("a mute listener must time out");
    };
    assert!(
        matches!(err, CommsClientError::Unresponsive { what: "handshake", .. }),
        "{err:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

fn test_message_meta() -> crate::comms::model::MessageMeta {
    crate::comms::model::MessageMeta {
        id: "m1".to_string(),
        thread: ThreadId::parse("t1".to_string()).expect("valid thread id"),
        from: AgentId::parse("agent-1".to_string()).expect("valid agent id"),
        ts_micros: 0,
        subject: "s".to_string(),
        tags: Vec::new(),
        reply_to: None,
        body_len: 0,
        body_sha: String::new(),
    }
}
