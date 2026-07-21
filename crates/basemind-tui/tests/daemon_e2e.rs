//! End-to-end coverage of `--daemon`: spawn the real binary in daemon mode as a controlled child,
//! connect to its per-workspace socket with a `UdsAgentClient`, and drive a scripted turn over the
//! wire. Proves the daemon runner binds the socket and serves the engine cross-process — the same
//! path `--attach` uses, but with a child we own so teardown is clean (no orphaned daemon).

#![cfg(all(feature = "replay", unix))]

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use basemind_agent::{AgentClient, AgentCommand, AgentEvent};
use basemind_agent_ipc::{UdsAgentClient, agent_socket_path};

/// A one-turn scripted scenario with a distinctive reply to assert on.
const SCENARIO: &str = r#"{ "user": "hi", "turns": [ { "text": "daemon reply ZR7" } ] }"#;

/// Kills the daemon child on drop, so a failed assertion never leaks the process.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn daemon_serves_a_scripted_session_over_the_socket() {
    let data_home = tempfile::tempdir().expect("data home tempdir");
    let root = tempfile::tempdir().expect("repo root tempdir");
    // SAFETY: this is the only test in this test binary and no other thread reads the variable;
    // isolating BASEMIND_DATA_HOME keeps the daemon socket inside the tempdir. ~keep
    unsafe { std::env::set_var("BASEMIND_DATA_HOME", data_home.path()) };

    let mut scenario_file = tempfile::NamedTempFile::new().expect("scenario tempfile");
    scenario_file.write_all(SCENARIO.as_bytes()).expect("write scenario");

    // The child inherits BASEMIND_DATA_HOME, so it derives the same socket path this test computes. ~keep
    let socket = agent_socket_path(root.path());
    let child = Command::new(env!("CARGO_BIN_EXE_basemind-tui"))
        .arg("--daemon")
        .arg("--replay")
        .arg(scenario_file.path())
        .arg("--root")
        .arg(root.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    let _guard = ChildGuard(child);

    let deadline = Instant::now() + Duration::from_secs(20);
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "daemon never created its socket at {}",
            socket.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let mut client = UdsAgentClient::connect(&socket).await.expect("connect to the daemon");
    client
        .send_command(AgentCommand::UserMessage { text: "hi".into() })
        .await
        .expect("send user message");

    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(15), client.next_event())
            .await
            .expect("an event arrives")
            .expect("the stream stays open through the turn");
        let finished = matches!(event, AgentEvent::TurnFinished { .. });
        events.push(event);
        if finished {
            break;
        }
    }

    let streamed: String = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        streamed, "daemon reply ZR7",
        "scenario streamed over the daemon: {events:?}"
    );
}
