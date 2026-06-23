//! End-to-end smoke test for `inbox_ack` and the richer front-matter (scope + seq) against a
//! REAL detached broker daemon.
//!
//! The daemon is the actual `basemind comms daemon` process (spawned via the test binary's
//! `CARGO_BIN_EXE_basemind`), isolated to a tempdir via `BASEMIND_COMMS_DIR`. We then drive the
//! library [`CommsClient`] directly — the client API exposes `ack_inbox`, `post_message` (with
//! `scope`), `read_inbox`, and `read_history`, which the `basemind comms` CLI does not fully
//! surface. This pins the W7 contract end to end:
//!
//! * `inbox_ack` by `message_ids` advances ONLY the acking agent's read cursor — the acked
//!   messages drop out of that agent's next `inbox_read`, but
//! * `room_history` STILL returns them (the shared append-only log is untouched), and
//! * a second agent's inbox is unaffected (per-agent cursor isolation),
//! * the `to_seq` bulk mode clears a room straight to a seq,
//! * front-matter now carries `ts_micros`, `tags`, `scope`, and `seq`, and
//! * `room_post` with `scope` round-trips.

#![cfg(feature = "comms")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use basemind::comms::client::CommsClient;
use basemind::comms::ids::{AgentId, RoomId};
use basemind::comms::model::RoomScope;
use basemind::comms::singleton::{CommsPaths, comms_socket_path, probe_alive};

const BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// Owns the spawned daemon process so it is always reaped: on drop it asks the daemon to drain
/// via the `comms stop` RPC, then waits on (and as a fallback kills) the child to avoid a zombie.
struct Daemon {
    child: Child,
    comms_dir: PathBuf,
    socket: PathBuf,
}

impl Daemon {
    /// Spawn the real `basemind comms daemon` against `comms_dir` and wait until its socket
    /// answers a ping. Panics if it does not come up within a few seconds — but the `Child` is
    /// owned by `Self` first, so its `Drop` reaps the process even on the panic path.
    fn start(comms_dir: &Path) -> Self {
        let socket = comms_socket_path(comms_dir);
        let child = Command::new(BIN)
            .args(["comms", "daemon"])
            .env("BASEMIND_COMMS_DIR", comms_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn comms daemon");
        let daemon = Self {
            child,
            comms_dir: comms_dir.to_path_buf(),
            socket,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if probe_alive(&daemon.socket) {
                return daemon;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // `daemon` (and thus the child) is dropped + reaped by the unwind before the panic
        // propagates.
        panic!("comms daemon did not become ready");
    }

    fn socket(&self) -> &Path {
        &self.socket
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .args(["comms", "stop"])
            .env("BASEMIND_COMMS_DIR", &self.comms_dir)
            .output();
        // The stop RPC drains the daemon; give it a moment, then reap. Kill as a fallback so the
        // child is never left as a zombie if the RPC did not land.
        if self.child.try_wait().ok().flatten().is_none() {
            std::thread::sleep(Duration::from_millis(200));
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
    }
}

async fn connect(socket: &Path, agent: &str, root: &Path) -> CommsClient {
    let paths = CommsPaths {
        comms_dir: socket.parent().expect("socket parent").to_path_buf(),
        socket_path: socket.to_path_buf(),
    };
    CommsClient::connect(
        &paths,
        AgentId::parse(agent).expect("agent id"),
        None,
        Some(root.to_path_buf()),
    )
    .await
    .unwrap_or_else(|e| panic!("connect {agent}: {e}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn inbox_ack_advances_cursor_without_touching_shared_log_or_other_agents() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let room = RoomId::parse("team").expect("room");

    // Alice creates a global room and posts two messages; the first carries a `scope`.
    let mut alice = connect(&socket, "agent-alice", &root).await;
    alice
        .create_room(room.clone(), RoomScope::Global, Some("Team".to_string()))
        .await
        .expect("create room");

    let scope = vec!["src/**".to_string(), "docs/**".to_string()];
    let m1 = alice
        .post_message(
            room.clone(),
            "first".to_string(),
            b"body one".to_vec(),
            vec!["ops".to_string()],
            None,
            scope.clone(),
        )
        .await
        .expect("post m1");
    let _m2 = alice
        .post_message(
            room.clone(),
            "second".to_string(),
            b"body two".to_vec(),
            vec![],
            None,
            vec![],
        )
        .await
        .expect("post m2");

    // Bob and Carol both join the room (durable membership → inbox).
    let mut bob = connect(&socket, "agent-bob", &root).await;
    let mut carol = connect(&socket, "agent-carol", &root).await;
    bob.join_room(room.clone()).await.expect("bob joins");
    carol.join_room(room.clone()).await.expect("carol joins");

    // (5) Front-matter now carries ts_micros + tags + scope + seq.
    let (bob_inbox, _unread, _c) = bob
        .read_inbox(None, None, None, 100, false)
        .await
        .expect("bob inbox");
    assert_eq!(bob_inbox.len(), 2, "both messages are unread for Bob");
    let first = bob_inbox
        .iter()
        .find(|sm| sm.meta.id == m1)
        .expect("m1 in inbox");
    assert!(first.meta.ts_micros > 0, "ts_micros surfaced");
    assert_eq!(first.meta.tags, vec!["ops".to_string()], "tags surfaced");
    assert_eq!(first.meta.scope, scope, "scope round-trips through post");
    assert_eq!(first.seq, 1, "seq surfaced (first message in the room)");

    // (1) Bob acks the first message BY ID → his cursor advances past it.
    let (acked, cursors) = bob
        .ack_inbox(vec![m1.clone()], None, None)
        .await
        .expect("bob ack m1");
    assert_eq!(acked, 1, "one id resolved + acked");
    assert_eq!(
        cursors,
        vec![("team".to_string(), 1)],
        "cursor advanced to seq 1"
    );

    // Bob's next inbox no longer shows the acked message; only "second" remains.
    let (bob_after, _u, _c) = bob
        .read_inbox(None, None, None, 100, false)
        .await
        .expect("bob inbox after ack");
    assert_eq!(bob_after.len(), 1, "acked message dropped from Bob's inbox");
    assert_eq!(bob_after[0].meta.subject, "second");

    // (2) The shared log is intact — room_history STILL returns BOTH messages.
    let (history, _next) = bob
        .read_history(room.clone(), None, 100)
        .await
        .expect("history");
    assert_eq!(history.len(), 2, "ack must not delete from the shared log");

    // (3) Carol's inbox is unaffected by Bob's ack — per-agent isolation.
    let (carol_inbox, _u, _c) = carol
        .read_inbox(None, None, None, 100, false)
        .await
        .expect("carol inbox");
    assert_eq!(carol_inbox.len(), 2, "another agent's inbox is untouched");

    // (4) The to_seq bulk mode clears the whole room for Carol straight to seq 2.
    let (acked2, cursors2) = carol
        .ack_inbox(vec![], Some(room.clone()), Some(2))
        .await
        .expect("carol bulk ack");
    assert_eq!(acked2, 0, "bulk mode acks no specific ids");
    assert_eq!(cursors2, vec![("team".to_string(), 2)]);
    let (carol_after, _u, _c) = carol
        .read_inbox(None, None, None, 100, false)
        .await
        .expect("carol inbox after bulk ack");
    assert!(carol_after.is_empty(), "to_seq bulk-acked the whole room");

    // An ack with neither mode is rejected by the broker.
    let err = bob.ack_inbox(vec![], None, None).await;
    assert!(err.is_err(), "empty ack must be rejected");
}

/// Regression: a long-lived client must transparently recover when the daemon dies mid-session.
///
/// Reproduces the `Broken pipe (os error 32)` failure: the MCP server caches one `CommsClient`
/// for the whole session, so when the detached daemon is killed (crash / reap / `Stop`), the
/// cached stream goes stale and every subsequent `room_post` write hits EPIPE with no recovery.
/// The fix makes the client respawn the daemon + reconnect + retry once on a broken connection.
#[tokio::test(flavor = "multi_thread")]
async fn client_recovers_when_daemon_dies_mid_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();

    // Spawn the FIRST daemon and connect a client that knows how to respawn the real `basemind`
    // binary against this isolated comms dir (production `ensure_and_connect` would respawn the
    // test binary, which has no `comms daemon` subcommand — so inject the real binary spawn).
    let mut daemon = Daemon::start(&comms_dir);
    let paths = CommsPaths {
        comms_dir: comms_dir.clone(),
        socket_path: comms_socket_path(&comms_dir),
    };
    let spawn_dir = comms_dir.clone();
    let mut client = CommsClient::connect_with_respawn(
        &paths,
        AgentId::parse("agent-resilient").expect("agent id"),
        None,
        Some(root.clone()),
        move |_paths: &CommsPaths| {
            Command::new(BIN)
                .args(["comms", "daemon"])
                .env("BASEMIND_COMMS_DIR", &spawn_dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map(|_| ())
        },
    )
    .await
    .expect("connect with respawn");

    let room = RoomId::parse("team").expect("room");
    client
        .create_room(room.clone(), RoomScope::Global, Some("Team".to_string()))
        .await
        .expect("create room");
    let first = client
        .post_message(
            room.clone(),
            "before".to_string(),
            b"first".to_vec(),
            vec![],
            None,
            vec![],
        )
        .await
        .expect("post before death");
    assert!(!first.is_empty(), "first post returns an id");

    // Kill the daemon hard (no drain): the cached client stream is now stale. Wait until the
    // socket stops answering so the next post genuinely races a dead daemon.
    let _ = daemon.child.kill();
    let _ = daemon.child.wait();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && probe_alive(&paths.socket_path) {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !probe_alive(&paths.socket_path),
        "daemon must be dead before the recovery post"
    );

    // The post that previously failed with EPIPE forever: the client must respawn + reconnect +
    // retry, and the post must succeed against the fresh daemon.
    let second = client
        .post_message(
            room.clone(),
            "after".to_string(),
            b"second".to_vec(),
            vec![],
            None,
            vec![],
        )
        .await
        .expect("post after death must transparently recover");
    assert!(!second.is_empty(), "recovered post returns an id");
    assert_ne!(first, second, "recovered post is a distinct message");

    // The shared log is intact and reachable through the respawned daemon: both posts land.
    let (history, _next) = client
        .read_history(room.clone(), None, 100)
        .await
        .expect("history after recovery");
    assert_eq!(
        history.len(),
        2,
        "both the pre-death and post-recovery messages are in the log"
    );

    // Reap the respawned daemon (the original `Daemon` Drop targets the dead child).
    let _ = Command::new(BIN)
        .args(["comms", "stop"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .output();
}

/// A child agent connected with an explicit [`SessionContext`] presents its `session_id` on the
/// `Hello`, so the broker auto-joins it to the matching `RoomScope::Session` room — and a post by
/// the parent into that room lands in the child's inbox. This pins the agent-shells coupling end to
/// end: the env-sourced session context (here driven through the explicit-argument seam, avoiding
/// `set_var` races) makes the parent and child share a session-scoped room without either issuing
/// an explicit `join`.
#[tokio::test(flavor = "multi_thread")]
async fn child_with_session_context_auto_joins_parents_session_room() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();
    let paths = CommsPaths {
        comms_dir: socket.parent().expect("socket parent").to_path_buf(),
        socket_path: socket.clone(),
    };

    let session_id = "bmsh-1234-0";
    let room = RoomId::parse("shell-session-room").expect("room");

    // The parent creates a session-scoped room and joins it (mirrors `shell_spawn`).
    let mut parent = connect(&socket, "parent-agent", &root).await;
    parent
        .create_room(
            room.clone(),
            RoomScope::Session(session_id.to_string()),
            Some("shell session".to_string()),
        )
        .await
        .expect("create session room");
    parent.join_room(room.clone()).await.expect("parent joins");

    // The child connects carrying the SAME session_id via the explicit-argument seam: its `Hello`
    // presents the session context, and the broker auto-joins it to the matching session room.
    let mut child = CommsClient::connect_with_session(
        &paths,
        AgentId::parse("parent-agent-bmsh1234").expect("child agent"),
        None,
        Some(root.clone()),
        basemind::comms::client::SessionContext {
            session_id: Some(session_id.to_string()),
            parent_agent: Some("parent-agent".to_string()),
        },
    )
    .await
    .expect("child connect");

    // The parent posts into the session room → the child sees it WITHOUT an explicit join.
    parent
        .post_message(
            room.clone(),
            "hello-child".to_string(),
            b"work to do".to_vec(),
            vec![],
            None,
            vec![],
        )
        .await
        .expect("parent posts to child");

    let (child_inbox, _unread, _cursor) = child
        .read_inbox(None, None, None, 100, false)
        .await
        .expect("child inbox");
    assert_eq!(
        child_inbox.len(),
        1,
        "child auto-joined the session room and received the parent's post"
    );
    assert_eq!(child_inbox[0].meta.subject, "hello-child");

    // A control agent with NO session context is not auto-joined and sees nothing.
    let mut outsider = connect(&socket, "outsider-agent", &root).await;
    let (outsider_inbox, _u, _c) = outsider
        .read_inbox(None, None, None, 100, false)
        .await
        .expect("outsider inbox");
    assert!(
        outsider_inbox.is_empty(),
        "an agent with no session context must not join the session room"
    );
}
