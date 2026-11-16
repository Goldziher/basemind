//! The endpoint exchanges against a real named pipe.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::windows::named_pipe::ServerOptions;

use super::*;

fn pipe_name(case: &str) -> String {
    format!(r"\\.\pipe\basemind-endpoint-test-{}-{case}", std::process::id())
}

/// Serve `name` as a daemon whose first `mute` connections are accepted, never read and kept
/// open, and whose later ones answer each request with `Pong`. Returns the runtime that serves it
/// and the count of connections accepted so far.
fn serve(name: &str, mute: usize) -> (tokio::runtime::Runtime, Arc<AtomicUsize>) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let accepted = Arc::new(AtomicUsize::new(0));
    let (seen, name) = (Arc::clone(&accepted), name.to_string());
    let (ready, created) = std::sync::mpsc::channel();
    runtime.spawn(async move {
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .expect("create the pipe");
        let _ = ready.send(());
        let mut unanswered = Vec::new();
        loop {
            server.connect().await.expect("accept a client");
            let next = ServerOptions::new().create(&name).expect("create the next instance");
            let mut connected = std::mem::replace(&mut server, next);
            if seen.fetch_add(1, Ordering::SeqCst) < mute {
                unanswered.push(connected);
                continue;
            }
            tokio::spawn(async move {
                let mut prefix = [0u8; 4];
                if connected.read_exact(&mut prefix).await.is_err() {
                    return;
                }
                let mut request = vec![0u8; u32::from_be_bytes(prefix) as usize];
                if connected.read_exact(&mut request).await.is_err() {
                    return;
                }
                let reply = rmp_serde::to_vec_named(&CommsOut::Response(CommsResponse::Pong)).expect("encode");
                let len = u32::try_from(reply.len()).expect("a short reply");
                let _ = connected.write_all(&len.to_be_bytes()).await;
                let _ = connected.write_all(&reply).await;
                let _ = connected.flush().await;
                let _ = connected.read(&mut prefix).await;
            });
        }
    });
    created
        .recv_timeout(Duration::from_secs(10))
        .expect("the pipe is created");
    (runtime, accepted)
}

/// The case the cancellable read exists for: a daemon reads nothing on one connection and keeps
/// it open. That exchange is given up on in bounded time, and the next one is still made, so a
/// daemon that answers `Ping` is seen to answer.
#[test]
fn a_daemon_that_left_one_exchange_unanswered_is_still_asked_the_next() {
    let name = pipe_name("unanswered");
    let (_serving, accepted) = serve(&name, 1);
    let endpoint = Path::new(&name);

    let started = Instant::now();
    assert_eq!(probe_once(endpoint), Liveness::Silent, "the first ping gets no answer");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "and is given up on: {:?}",
        started.elapsed()
    );
    assert_eq!(
        probe_once(endpoint),
        Liveness::Answers,
        "the next ping is sent, not skipped"
    );
    assert_eq!(accepted.load(Ordering::SeqCst), 2, "both pings reached the pipe");

    let ping = rmp_serde::to_vec_named(&CommsRequest::Ping).expect("encode");
    assert_eq!(
        exchange(endpoint, ping, Duration::from_secs(5)),
        Some(CommsResponse::Pong),
        "a full exchange is answered too"
    );
    assert!(endpoint_accepts(endpoint));
}

#[test]
fn an_exchange_with_a_daemon_that_never_answers_ends_at_its_timeout() {
    let name = pipe_name("mute");
    let (_serving, _accepted) = serve(&name, usize::MAX);
    let endpoint = Path::new(&name);
    let ping = rmp_serde::to_vec_named(&CommsRequest::Ping).expect("encode");

    let started = Instant::now();
    assert_eq!(exchange(endpoint, ping, Duration::from_millis(300)), None);
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
}

#[test]
fn a_pipe_that_is_not_there_is_silent() {
    let name = pipe_name("absent");
    let endpoint = Path::new(&name);
    assert_eq!(probe_once(endpoint), Liveness::Silent);
    assert!(!endpoint_accepts(endpoint));
    let ping = rmp_serde::to_vec_named(&CommsRequest::Ping).expect("encode");
    assert_eq!(exchange(endpoint, ping, Duration::from_millis(300)), None);
}
