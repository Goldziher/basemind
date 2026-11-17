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
                        CommsRequest::Ping => CommsResponse::Pong,
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
    // ~keep The clock alone collides when two tests start in the same tick, and the second bind
    // then fails with AlreadyExists; the counter makes the suffix unique within the process.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = u128::from(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 1000);
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    micros * 1000 + seq
}

/// The reported failure: a request abandoned after its frame was written (the delivery-notice
/// probe's 200 ms timeout) leaves its reply on the socket. The next request must still get ITS
/// answer, not the stale frame ("unexpected response shape").
#[tokio::test]
async fn dropped_request_does_not_poison_the_next_response() {
    let (paths, server, dir) = scripted_broker(vec![std::time::Duration::from_millis(200)]);
    let mut client = CommsClient::connect_with_respawn(&paths, ClientHello::anonymous(), |_| Ok(()))
        .await
        .expect("connect");

    let abandoned = tokio::time::timeout(std::time::Duration::from_millis(30), client.ping()).await;
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
    let mut client = CommsClient::connect_with_respawn(&paths, ClientHello::anonymous(), |_| Ok(()))
        .await
        .expect("connect")
        .with_request_timeout(std::time::Duration::from_millis(100));

    let started = std::time::Instant::now();
    let err = client.ping().await.expect_err("no reply must time out");
    assert!(
        matches!(err, CommsClientError::Unresponsive { what: "ping", .. }),
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
    let Err(err) = CommsClient::connect_bounded(
        &paths,
        ClientHello::anonymous(),
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

/// A broker that answers `Hello` on every link, counts each correlated request it reads, and
/// hangs up without replying: a daemon that committed the request and died before its answer.
fn hanging_up_broker() -> (
    CommsPaths,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
) {
    let dir = std::env::temp_dir().join(format!("bm-client-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&dir).expect("dir");
    let socket_path = dir.join("c.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind");
    let paths = CommsPaths {
        comms_dir: dir.clone(),
        socket_path,
    };
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = std::sync::Arc::clone(&seen);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut codec = LengthDelimitedCodec::new();
            let mut buf = BytesMut::new();
            loop {
                let frame = loop {
                    if let Some(f) = codec.decode(&mut buf).expect("decode") {
                        break Some(f);
                    }
                    if stream.read_buf(&mut buf).await.unwrap_or(0) == 0 {
                        break None;
                    }
                };
                let Some(frame) = frame else { break };
                match rmp_serde::from_slice::<CommsRequest>(&frame).expect("req") {
                    CommsRequest::Hello { .. } => {
                        let out = CommsOut::Response(CommsResponse::Welcome {
                            proto_ver: PROTO_VER,
                            daemon_version: "test".to_string(),
                        });
                        let mut framed = BytesMut::new();
                        codec
                            .encode(Bytes::from(rmp_serde::to_vec_named(&out).expect("encode")), &mut framed)
                            .expect("frame");
                        stream.write_all(&framed).await.expect("write");
                    }
                    _ => {
                        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                }
            }
        }
    });
    (paths, seen, handle, dir)
}

/// A CLI post carries no replay key, so a link lost after the daemon read it must not send it
/// again: the daemon may have committed it.
#[tokio::test]
async fn a_post_whose_link_broke_is_not_sent_twice() {
    let (paths, seen, server, dir) = hanging_up_broker();
    let mut client = CommsClient::connect_with_respawn(&paths, ClientHello::anonymous(), |_| Ok(()))
        .await
        .expect("connect");
    let body = super::super::v2::api::PostBody {
        target: super::super::v2::api::PostTarget::Thread(
            super::super::v2::api::ThreadId::parse("t-aaaaaa").expect("thread id"),
        ),
        body: "once".to_string(),
    };
    assert!(client.agent_post(None, body).await.is_err());
    assert_eq!(
        seen.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the post reached the daemon once"
    );

    assert!(client.ping().await.is_err());
    assert!(
        seen.load(std::sync::atomic::Ordering::SeqCst) > 2,
        "a ping, which changes nothing, is sent again after the link breaks"
    );

    server.abort();
    let _ = std::fs::remove_dir_all(dir);
}
