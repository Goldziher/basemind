//! Hermetic [`CommsRoom`] test: an isolated UDS broker on a tempdir socket with two distinct room
//! peers over one repo root, exercising the real broker + client transport with **no singleton
//! daemon and no network**. Reuses the isolated-broker spin-up pattern from the root crate's
//! `wait_inbox_tests` (`src/comms/frontend_inproc.rs`), driven through the [`CommsRoom`] seam via
//! [`CommsRoom::connect_with_paths`].
#![cfg(feature = "comms")]

use std::sync::Arc;
use std::time::Duration;

use basemind::comms::daemon::Broker;
use basemind::comms::frontend_uds::UdsFrontend;
use basemind::comms::ids::AgentId;
use basemind::comms::singleton::CommsPaths;
use basemind::comms::store::CommsStore;
use basemind::comms::transport::CommsFrontend;
use basemind_agent::AgentEvent;
use basemind_agent::room::{CommsRoom, RoomClient};
use tempfile::TempDir;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

/// How long to wait for an expected streamed event / roster entry before failing.
const DEADLINE: Duration = Duration::from_secs(5);

/// An isolated broker plus the handles needed to tear it down.
struct Harness {
    paths: CommsPaths,
    shutdown: watch::Sender<bool>,
    serve: JoinHandle<std::io::Result<()>>,
    _dir: TempDir,
}

impl Harness {
    /// Signal shutdown and abort the serve task. Aborting (rather than awaiting) is deliberate: a
    /// room's detached incoming task holds a `wait_inbox` long-poll open, so awaiting `serve` would
    /// block teardown until that server-side wait caps out — a test artifact, not a real stall.
    fn teardown(self) {
        let _ = self.shutdown.send(true);
        self.serve.abort();
    }
}

/// Spawn an isolated broker serving on `socket_path`, backed by a fresh store under `store_dir`.
async fn spawn_serve(
    store_dir: &std::path::Path,
    socket_path: &std::path::Path,
) -> (watch::Sender<bool>, JoinHandle<std::io::Result<()>>) {
    let store = Arc::new(CommsStore::open(store_dir).expect("open comms store"));
    let broker = Arc::new(Broker::new(store));
    spawn_serve_broker(broker, socket_path).await
}

/// Serve an already-built broker on `socket_path`. Sharing one broker (and its store) across two
/// successive calls is how the reconnect test restarts the front-end without reopening — and thus
/// re-locking — the store, while giving the new broker a fresh subscriber registry so a stream only
/// resumes by genuinely reconnecting.
async fn spawn_serve_broker(
    broker: Arc<Broker>,
    socket_path: &std::path::Path,
) -> (watch::Sender<bool>, JoinHandle<std::io::Result<()>>) {
    let _ = std::fs::remove_file(socket_path);
    let listener = {
        let std_listener = std::os::unix::net::UnixListener::bind(socket_path).expect("bind temp socket");
        std_listener.set_nonblocking(true).expect("nonblocking");
        tokio::net::UnixListener::from_std(std_listener).expect("adopt listener")
    };
    let (shutdown, shutdown_rx) = watch::channel(false);
    let frontend = UdsFrontend::from_listener(listener, socket_path.to_path_buf());
    let serve = tokio::spawn(async move { Box::new(frontend).serve(broker, shutdown_rx).await });
    (shutdown, serve)
}

/// Stand up an isolated UDS broker on a fresh tempdir socket.
async fn isolated_broker() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("c.sock");
    let paths = CommsPaths {
        comms_dir: dir.path().to_path_buf(),
        socket_path: socket_path.clone(),
    };
    let (shutdown, serve) = spawn_serve(dir.path(), &socket_path).await;
    Harness {
        paths,
        shutdown,
        serve,
        _dir: dir,
    }
}

/// Poll the broadcast receiver until `predicate` matches an event or the deadline elapses.
async fn wait_for_event(
    rx: &mut broadcast::Receiver<AgentEvent>,
    mut predicate: impl FnMut(&AgentEvent) -> bool,
) -> AgentEvent {
    let matcher = async {
        loop {
            match rx.recv().await {
                Ok(event) if predicate(&event) => return event,
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => panic!("event stream closed early"),
            }
        }
    };
    tokio::time::timeout(DEADLINE, matcher)
        .await
        .expect("expected event within the deadline")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_rooms_exchange_a_post_over_an_isolated_broker() {
    let harness = isolated_broker().await;
    // Same root → same `agent-room` thread addressing; distinct agent ids → two peers.
    let root = std::path::Path::new(".");

    let alice = CommsRoom::connect_with_paths(root, &harness.paths, AgentId::parse("alice").expect("agent"))
        .await
        .expect("connect alice room");
    let bob = CommsRoom::connect_with_paths(root, &harness.paths, AgentId::parse("bob").expect("agent"))
        .await
        .expect("connect bob room");

    // Bob streams incoming; alice posts. Bob must surface alice's post as a RoomMessage.
    let (events_tx, mut bob_events) = broadcast::channel(64);
    bob.spawn_incoming(events_tx);

    // Bob's startup roster (published once by spawn_incoming) must include both peers.
    let roster_event = wait_for_event(&mut bob_events, |event| matches!(event, AgentEvent::RoomRoster { .. })).await;
    if let AgentEvent::RoomRoster { peers } = roster_event {
        let ids: Vec<&str> = peers.iter().map(|peer| peer.id.as_str()).collect();
        assert!(ids.contains(&"alice"), "roster carries alice: {ids:?}");
        assert!(ids.contains(&"bob"), "roster carries bob: {ids:?}");
    }

    alice
        .post(Some("sync".to_string()), "HERMETIC-42".to_string())
        .await
        .expect("alice posts to the room");

    let message_event = wait_for_event(&mut bob_events, |event| matches!(event, AgentEvent::RoomMessage(_))).await;
    if let AgentEvent::RoomMessage(message) = message_event {
        assert_eq!(message.from, "alice", "the streamed post is attributed to alice");
        assert_eq!(message.body, "HERMETIC-42", "the streamed post carries alice's body");
    }

    // The same post is visible via a direct history read.
    let history = bob.history(None).await.expect("bob reads history");
    assert!(
        history
            .iter()
            .any(|message| message.from == "alice" && message.body == "HERMETIC-42"),
        "history carries alice's post: {history:?}"
    );

    harness.teardown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn roster_deltas_surface_a_peer_joining_and_leaving() {
    let harness = isolated_broker().await;
    let root = std::path::Path::new(".");

    let alice = CommsRoom::connect_with_paths(root, &harness.paths, AgentId::parse("alice").expect("agent"))
        .await
        .expect("connect alice room");
    let bob = CommsRoom::connect_with_paths(root, &harness.paths, AgentId::parse("bob").expect("agent"))
        .await
        .expect("connect bob room");
    // Silence the unused warning: alice only needs to exist as a thread member for the roster. ~keep
    let _ = &alice;

    let (events_tx, mut bob_events) = broadcast::channel(64);
    bob.spawn_incoming(events_tx);

    // Drain the startup roster (alice + bob) so later events are genuine deltas. ~keep
    let _ = wait_for_event(&mut bob_events, |event| matches!(event, AgentEvent::RoomRoster { .. })).await;

    // A third peer joins the room; the periodic roster refresh must surface a join delta. ~keep
    let carol = CommsRoom::connect_with_paths(root, &harness.paths, AgentId::parse("carol").expect("agent"))
        .await
        .expect("connect carol room");
    wait_for_event(
        &mut bob_events,
        |event| matches!(event, AgentEvent::RoomPeerJoined { peer } if peer.id == "carol"),
    )
    .await;

    // Carol leaves and drops; the next refresh must surface a matching leave delta. ~keep
    carol.leave().await.expect("carol leaves the room");
    drop(carol);
    wait_for_event(
        &mut bob_events,
        |event| matches!(event, AgentEvent::RoomPeerLeft { id } if id == "carol"),
    )
    .await;

    harness.teardown();
}

// This exercises gap #1 — the incoming stream's connect + capped-backoff reconnect loop — via the ~keep
// connect-retry path: the stream is started while the broker is DOWN, so its first dials fail and ~keep
// back off, then a broker comes up on the same injected socket and the stream must recover and ~keep
// deliver. It deliberately does NOT test a mid-stream `wait_inbox` break: an in-process broker's ~keep
// per-connection handler tasks are independent of its accept loop, so aborting the loop leaves an ~keep
// established connection alive (the client never errors), and the only clean server-side close is ~keep
// the 10s `drain_links` grace — too slow and racy for a deterministic hermetic test. The reconnect ~keep
// loop is the same machinery either way, so the connect-retry path is the faithful, fast surrogate. ~keep
//
// The two brokers share ONE store (via a shared `Arc<Broker>` over one `CommsStore`): reopening the ~keep
// store would deadlock on its exclusive flock (the first broker's still-connected client handlers ~keep
// hold it), and the fresh broker's own subscriber registry means the stream can only resume by ~keep
// genuinely reconnecting, not by a lingering subscription. ~keep
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_stream_recovers_by_reconnecting_to_a_broker_that_comes_up_late() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = dir.path().to_path_buf();
    let socket_path = dir.path().join("c.sock");
    let paths = CommsPaths {
        comms_dir: store_dir.clone(),
        socket_path: socket_path.clone(),
    };
    let root = std::path::Path::new(".");

    // One store, kept alive for the whole test so its flock is never released or reacquired. ~keep
    let store = Arc::new(CommsStore::open(&store_dir).expect("open comms store"));

    // Broker #1: connect alice + bob (both join the room thread), then tear the front-end down. ~keep
    let broker1 = Arc::new(Broker::new(store.clone()));
    let (shutdown1, serve1) = spawn_serve_broker(broker1, &socket_path).await;
    let alice = CommsRoom::connect_with_paths(root, &paths, AgentId::parse("alice").expect("agent"))
        .await
        .expect("connect alice room");
    let bob = CommsRoom::connect_with_paths(root, &paths, AgentId::parse("bob").expect("agent"))
        .await
        .expect("connect bob room");
    let _ = shutdown1.send(true);
    serve1.abort();
    let _ = std::fs::remove_file(&socket_path);

    // Start bob's incoming stream while NO broker is listening: its dials fail and back off. ~keep
    let (events_tx, mut bob_events) = broadcast::channel(64);
    bob.spawn_incoming(events_tx);

    // Broker #2: a fresh registry over the SAME store, bound to the SAME socket. Bob's incoming loop ~keep
    // must re-dial it, rejoin the (persisted) thread, and resume. ~keep
    let broker2 = Arc::new(Broker::new(store.clone()));
    let (shutdown2, serve2) = spawn_serve_broker(broker2, &socket_path).await;

    // Bob's startup roster only appears once his incoming stream has reconnected to broker #2. ~keep
    let _ = wait_for_event(&mut bob_events, |event| matches!(event, AgentEvent::RoomRoster { .. })).await;

    // Alice's shared client transparently reconnects to the live broker on her next post. ~keep
    alice
        .post(Some("after".to_string()), "RECOVERED-7".to_string())
        .await
        .expect("alice posts after the reconnect");

    let message_event = wait_for_event(
        &mut bob_events,
        |event| matches!(event, AgentEvent::RoomMessage(message) if message.body == "RECOVERED-7"),
    )
    .await;
    if let AgentEvent::RoomMessage(message) = message_event {
        assert_eq!(message.from, "alice", "the recovered post is attributed to alice");
        assert_eq!(message.body, "RECOVERED-7", "the recovered post carries alice's body");
    }

    let _ = shutdown2.send(true);
    serve2.abort();
    let _ = serve2.await;
}
