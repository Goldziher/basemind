//! End-to-end coverage of `--daemon`: spawn the real binary in daemon mode as a controlled child,
//! connect to its per-workspace socket with a `UdsAgentClient`, and drive a scripted turn over the
//! wire. Proves the daemon runner binds the socket and serves the engine cross-process — the same
//! path `--attach` uses, but with a child we own so teardown is clean (no orphaned daemon).

#![cfg(all(feature = "replay", unix))]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use basemind::config::ConfigV1;
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};
use basemind_agent::{AgentClient, AgentCommand, AgentEvent};
use basemind_agent_ipc::UdsAgentClient;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// A one-turn scripted scenario with a distinctive reply to assert on.
const SCENARIO: &str = r#"{ "user": "hi", "turns": [ { "text": "daemon reply ZR7" } ] }"#;
const CHILD_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const BOOTSTRAP_SECS: &str = "5";
const IDLE_REAP_SECS: &str = "1";
const IDLE_CHECK_SECS: &str = "1";

/// Kills the daemon child on drop, so a failed assertion never leaks the process.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

impl ChildGuard {
    fn wait_for_natural_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + CHILD_TIMEOUT;
        loop {
            match self.0.try_wait().expect("poll daemon child") {
                Some(status) => return status,
                None => assert!(Instant::now() < deadline, "daemon did not exit naturally"),
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

fn isolated_socket_path(data_home: &Path, root: &Path) -> PathBuf {
    let key = basemind::store_layout::workspace_key(root);
    data_home.join("agent").join(format!("{}.sock", &key[..16]))
}

async fn wait_for_socket_state(socket: &Path, exists: bool) {
    let deadline = Instant::now() + CHILD_TIMEOUT;
    while socket.exists() != exists {
        assert!(
            Instant::now() < deadline,
            "socket {} did not become {}",
            socket.display(),
            if exists { "present" } else { "absent" }
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn apply_short_lifecycle_env(command: &mut Command, data_home: &Path) {
    command
        .env("BASEMIND_DATA_HOME", data_home)
        .env("BASEMIND_AGENT_BOOTSTRAP_SECS", BOOTSTRAP_SECS)
        .env("BASEMIND_AGENT_IDLE_REAP_SECS", IDLE_REAP_SECS)
        .env("BASEMIND_AGENT_IDLE_CHECK_SECS", IDLE_CHECK_SECS);
}

fn scan_workspace(root: &Path, data_home: &Path) {
    let previous_data_home = std::env::var_os("BASEMIND_DATA_HOME");
    // SAFETY: this test binary's other tests pass BASEMIND_DATA_HOME directly to child processes;
    // none reads it from this process, and this helper restores the prior value before returning. ~keep
    unsafe { std::env::set_var("BASEMIND_DATA_HOME", data_home) };
    let result = Store::open(root, VIEW_WORKING)
        .map_err(|error| format!("open scan store: {error}"))
        .and_then(|mut store| {
            scan(
                root,
                &mut store,
                &ConfigV1::with_defaults(),
                ScanSource::WorkingTree,
                EmbedMode::Inline,
            )
            .map(|_| ())
            .map_err(|error| format!("scan workspace: {error}"))
        });
    match previous_data_home {
        Some(value) => unsafe { std::env::set_var("BASEMIND_DATA_HOME", value) },
        None => unsafe { std::env::remove_var("BASEMIND_DATA_HOME") },
    }
    result.expect("workspace scan succeeds");
}

#[tokio::test]
async fn daemon_serves_a_scripted_session_over_the_socket() {
    let data_home = tempfile::tempdir().expect("data home tempdir");
    let root = tempfile::tempdir().expect("repo root tempdir");

    let mut scenario_file = tempfile::NamedTempFile::new().expect("scenario tempfile");
    scenario_file.write_all(SCENARIO.as_bytes()).expect("write scenario");

    let socket = isolated_socket_path(data_home.path(), root.path());
    let mut command = Command::new(env!("CARGO_BIN_EXE_basemind-tui"));
    command
        .arg("--daemon")
        .arg("--replay")
        .arg(scenario_file.path())
        .arg("--root")
        .arg(root.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    apply_short_lifecycle_env(&mut command, data_home.path());
    let mut guard = ChildGuard(command.spawn().expect("spawn daemon"));

    wait_for_socket_state(&socket, true).await;

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

    client
        .send_command(AgentCommand::Shutdown)
        .await
        .expect("detach client");
    drop(client);
    let status = guard.wait_for_natural_exit();
    assert!(status.success(), "daemon exits cleanly after the idle window: {status}");
    wait_for_socket_state(&socket, false).await;
}

#[tokio::test]
async fn daemon_exits_cleanly_on_sigterm() {
    let data_home = tempfile::tempdir().expect("data home tempdir");
    let root = tempfile::tempdir().expect("repo root tempdir");
    let mut scenario_file = tempfile::NamedTempFile::new().expect("scenario tempfile");
    scenario_file.write_all(SCENARIO.as_bytes()).expect("write scenario");
    let socket = isolated_socket_path(data_home.path(), root.path());

    let mut command = Command::new(env!("CARGO_BIN_EXE_basemind-tui"));
    command
        .arg("--daemon")
        .arg("--replay")
        .arg(scenario_file.path())
        .arg("--root")
        .arg(root.path())
        .env("BASEMIND_DATA_HOME", data_home.path())
        .env("BASEMIND_AGENT_BOOTSTRAP_SECS", "60")
        .env("BASEMIND_AGENT_IDLE_REAP_SECS", "60")
        .env("BASEMIND_AGENT_IDLE_CHECK_SECS", "60")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut guard = ChildGuard(command.spawn().expect("spawn daemon"));
    wait_for_socket_state(&socket, true).await;

    let signal = Command::new("kill")
        .arg("-TERM")
        .arg(guard.0.id().to_string())
        .status()
        .expect("send SIGTERM");
    assert!(signal.success(), "kill command sends SIGTERM: {signal}");

    let status = guard.wait_for_natural_exit();
    assert!(status.success(), "daemon exits cleanly after SIGTERM: {status}");
    wait_for_socket_state(&socket, false).await;
}

#[tokio::test]
async fn attach_spawns_a_detached_daemon_that_reaps_after_the_ui_exits() {
    let data_home = tempfile::tempdir().expect("data home tempdir");
    let root = tempfile::tempdir().expect("repo root tempdir");
    let mut scenario_file = tempfile::NamedTempFile::new().expect("scenario tempfile");
    scenario_file.write_all(SCENARIO.as_bytes()).expect("write scenario");
    std::fs::write(root.path().join("lib.rs"), "pub fn indexed() {}\n").expect("seed scanned workspace");
    scan_workspace(root.path(), data_home.path());
    let socket = isolated_socket_path(data_home.path(), root.path());
    assert!(!socket.exists(), "test starts without a daemon socket");

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open attach pty");
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_basemind-tui"));
    command.arg("--attach");
    command.arg("--replay");
    command.arg(scenario_file.path());
    command.arg("--root");
    command.arg(root.path());
    command.env("TERM", "xterm-256color");
    command.env("BASEMIND_DATA_HOME", data_home.path());
    command.env("BASEMIND_AGENT_BOOTSTRAP_SECS", BOOTSTRAP_SECS);
    command.env("BASEMIND_AGENT_IDLE_REAP_SECS", IDLE_REAP_SECS);
    command.env("BASEMIND_AGENT_IDLE_CHECK_SECS", IDLE_CHECK_SECS);

    let mut attach = pair.slave.spawn_command(command).expect("spawn attach process");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
    let mut writer = pair.master.take_writer().expect("take pty writer");
    let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
    let reader_parser = Arc::clone(&parser);
    let reader_thread = std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => reader_parser.lock().expect("parser lock").process(&buffer[..read]),
            }
        }
    });

    wait_for_socket_state(&socket, true).await;
    let ui_deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        let screen = parser.lock().expect("parser lock").screen().contents();
        if screen.contains("ready") {
            break;
        }
        if Instant::now() >= ui_deadline {
            let _ = attach.kill();
            let _ = attach.wait();
            panic!("attach UI did not reach ready state: {screen}");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    writer.write_all(b"\x03").expect("send Ctrl-C to attach");
    writer.flush().expect("flush Ctrl-C");

    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        match attach.try_wait().expect("poll attach process") {
            Some(status) => {
                assert!(status.success(), "attach exits cleanly: {status}");
                break;
            }
            None if Instant::now() < deadline => {}
            None => {
                let _ = attach.kill();
                let _ = attach.wait();
                panic!("attach did not exit after Ctrl-C");
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    drop(writer);
    reader_thread.join().expect("join pty reader");
    wait_for_socket_state(&socket, false).await;
    scan_workspace(root.path(), data_home.path());
}
