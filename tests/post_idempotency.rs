//! A keyed `post` is stored exactly once even when the daemon dies after committing it and the
//! client retries against a restarted daemon (the reply was lost, so the client cannot know).

#![cfg(feature = "comms")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use basemind::comms::client::CommsClient;
use basemind::comms::ids::AgentId;
use basemind::comms::singleton::{CommsPaths, comms_socket_path, probe_alive};

const BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// Owns a daemon this test spawned; only that PID is ever killed.
struct Daemon {
    child: Child,
    socket: PathBuf,
}

impl Daemon {
    fn start(comms_dir: &Path) -> Self {
        let socket = comms_socket_path(comms_dir);
        let child = Command::new(BIN)
            .args(["comms", "daemon"])
            .env("BASEMIND_COMMS_DIR", comms_dir)
            .env("BASEMIND_DATA_HOME", comms_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn comms daemon");
        let daemon = Self { child, socket };
        let deadline = Instant::now() + Duration::from_secs(180);
        while Instant::now() < deadline {
            if probe_alive(&daemon.socket) {
                return daemon;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("comms daemon did not become ready");
    }

    /// Hard-kill, as a crash after commit and before the reply would.
    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn connect(socket: &Path, root: &Path) -> CommsClient {
    let paths = CommsPaths {
        comms_dir: socket.parent().expect("socket parent").to_path_buf(),
        socket_path: socket.to_path_buf(),
    };
    CommsClient::connect(
        &paths,
        AgentId::parse("agent-idem").expect("agent id"),
        None,
        Some(root.to_path_buf()),
    )
    .await
    .expect("connect")
}

#[tokio::test]
async fn keyed_post_is_stored_once_across_a_daemon_kill_and_retry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let comms_dir = dir.path().join("comms");
    std::fs::create_dir_all(&comms_dir).expect("comms dir");
    let socket = comms_socket_path(&comms_dir);

    let first = Daemon::start(&comms_dir);
    let mut client = connect(&socket, dir.path()).await;
    let thread = client
        .start_thread(Some("idem".to_string()), Some("src/**".to_string()), vec![])
        .await
        .expect("start thread");
    let original = client
        .post_message_keyed(
            thread.id.clone(),
            "hello".into(),
            b"body".to_vec(),
            vec![],
            None,
            "key-1".into(),
        )
        .await
        .expect("first post");
    // The reply is "lost": the daemon dies and the client only knows its connection broke.
    first.kill();
    drop(client);

    let second = Daemon::start(&comms_dir);
    let mut retry = connect(&socket, dir.path()).await;
    let again = retry
        .post_message_keyed(
            thread.id.clone(),
            "hello".into(),
            b"body".to_vec(),
            vec![],
            None,
            "key-1".into(),
        )
        .await
        .expect("retry post");
    assert_eq!(again, original, "the retry returns the original id");
    let other = retry
        .post_message_keyed(
            thread.id.clone(),
            "hello".into(),
            b"body".to_vec(),
            vec![],
            None,
            "key-2".into(),
        )
        .await
        .expect("distinct key post");
    assert_ne!(other, original);

    let (messages, _) = retry
        .read_history(thread.id.clone(), None, 50, None)
        .await
        .expect("history");
    assert_eq!(messages.len(), 2, "one stored copy per distinct key: {messages:?}");
    second.kill();
}
