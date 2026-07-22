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

/// Stand up an isolated UDS broker on a fresh tempdir socket.
async fn isolated_broker() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("c.sock");
    let paths = CommsPaths {
        comms_dir: dir.path().to_path_buf(),
        socket_path: socket_path.clone(),
    };
    let store = Arc::new(CommsStore::open(dir.path()).expect("open comms store"));
    let broker = Arc::new(Broker::new(store));
    let listener = {
        let std_listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind temp socket");
        std_listener.set_nonblocking(true).expect("nonblocking");
        tokio::net::UnixListener::from_std(std_listener).expect("adopt listener")
    };
    let (shutdown, shutdown_rx) = watch::channel(false);
    let frontend = UdsFrontend::from_listener(listener, socket_path.clone());
    let serve = tokio::spawn(async move { Box::new(frontend).serve(broker, shutdown_rx).await });
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
