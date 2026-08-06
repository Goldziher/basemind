//! End-to-end smoke test for the embedded-rmux `shells` feature.
//!
//! Drives the `basemind::shells` API directly (the MCP layer is a thin wrapper
//! over it): point the SDK's daemon-binary discovery at the separately-built
//! `basemind` binary — which carries the `--__internal-daemon` intercept that
//! the test-harness binary does not — sandbox the daemon on a per-test temp
//! socket, then prove spawn → capture → kill end-to-end.
//!
//! Gated on `feature = "shells"`. Unix-only: rmux's Unix-socket transport and a
//! POSIX shell are assumed.

#![cfg(all(feature = "shells", unix))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Once;
use std::time::{Duration, Instant};

use basemind::shells::session::{self, ShellCommand};
use basemind::shells::{SessionId, ShellRuntime};
use tempfile::TempDir;

static DAEMON_ENV: Once = Once::new();
const DAEMON_BIND_TIMEOUT: Duration = Duration::from_secs(15);
const DAEMON_EXIT_TIMEOUT: Duration = Duration::from_secs(10);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DAEMON_IDLE_REAP_SECS: &str = "60";
const DAEMON_IDLE_CHECK_SECS: &str = "1";

struct DaemonChild(Child);

impl Drop for DaemonChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn spawn_isolated_daemon(socket: &Path) -> DaemonChild {
    DaemonChild(
        Command::new(env!("CARGO_BIN_EXE_basemind"))
            .arg("--__internal-daemon")
            .arg(socket)
            .env("BASEMIND_SHELLS_IDLE_REAP_SECS", DAEMON_IDLE_REAP_SECS)
            .env("BASEMIND_SHELLS_IDLE_CHECK_SECS", DAEMON_IDLE_CHECK_SECS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn isolated shells daemon"),
    )
}

async fn await_daemon_bound(daemon: &mut DaemonChild, socket: &Path) {
    let deadline = Instant::now() + DAEMON_BIND_TIMEOUT;
    while !socket.exists() {
        assert_eq!(
            daemon.0.try_wait().expect("query daemon status while binding"),
            None,
            "daemon exited before binding {}",
            socket.display()
        );
        assert!(
            Instant::now() < deadline,
            "daemon did not bind {} before the deadline",
            socket.display()
        );
        tokio::time::sleep(DAEMON_POLL_INTERVAL).await;
    }
}

async fn await_daemon_exit(daemon: &mut DaemonChild, trigger: &str) {
    let deadline = Instant::now() + DAEMON_EXIT_TIMEOUT;
    loop {
        if let Some(status) = daemon.0.try_wait().expect("query daemon status after trigger") {
            assert!(
                status.success(),
                "daemon exited unsuccessfully after {trigger}: {status}"
            );
            return;
        }
        assert!(Instant::now() < deadline, "daemon did not exit after {trigger}");
        tokio::time::sleep(DAEMON_POLL_INTERVAL).await;
    }
}

/// Point the SDK's daemon-binary discovery at the built `basemind` executable, which carries the
/// `--__internal-daemon` intercept the test-harness binary lacks.
fn init_daemon_env() {
    DAEMON_ENV.call_once(|| {
        let daemon = PathBuf::from(env!("CARGO_BIN_EXE_basemind"));
        // SAFETY: `set_var` is not thread-safe under the 2024 edition; `Once` makes this the
        // single mutation, and it happens before any daemon child inherits the environment.
        unsafe {
            basemind::shells::daemon::point_sdk_daemon_at(&daemon);
            // The daemon is a cold re-exec of a large debug binary; paging it in on a busy machine
            // overruns the SDK's 5 s default startup deadline (a warm binary answers in ~50 ms).
            // Mirrors the same allowance in `mcp_smoke`'s shells test.
            std::env::set_var("RMUX_SDK_TIMEOUT_MS", "60000");
            // Poll for idleness every second so a shut-down daemon is observed leaving promptly. ~keep
            // The idle WINDOW is deliberately left at its ten-minute default: shortening it would ~keep
            // let the reaper fire between the daemon's bind and this test's first spawn — a cold ~keep
            // re-exec that can take seconds — and reap the daemon out from under the test. ~keep
            std::env::set_var("BASEMIND_SHELLS_IDLE_CHECK_SECS", "1");
        }
    });
}

/// The sandboxed socket path for one test's daemon.
fn socket_in(dir: &TempDir) -> PathBuf {
    init_daemon_env();
    dir.path().join("shells.sock")
}

/// Spawn one long-lived interactive session and return its id + rmux name.
async fn spawn_bash(runtime: &ShellRuntime) -> (SessionId, rmux_sdk::SessionName) {
    runtime
        .spawn(
            runtime.mint_session_id(),
            ShellCommand::Argv(vec!["bash".to_string()]),
            None,
            Vec::new(),
            200,
            50,
        )
        .await
        .expect("spawn bash session")
}

/// Poll a session's visible screen until `marker` appears, failing the test on timeout.
async fn await_marker(rmux: &rmux_sdk::Rmux, name: &rmux_sdk::SessionName, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let live = rmux.session(name.clone()).await.expect("open live session");
        let captured = session::capture(&live, None).await.expect("capture");
        if captured.contains(marker) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {marker} in {name:?}; last capture was {captured:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Poll until the daemon stops listing `name`, failing the test on timeout. A kill is
/// acknowledged before the session leaves the daemon's set, so callers must wait it out.
async fn await_gone(rmux: &rmux_sdk::Rmux, name: &rmux_sdk::SessionName) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let listed = session::list_sessions(rmux).await.expect("list after kill");
        if !listed.iter().any(|n| n == name) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "session {name:?} still listed after kill: {listed:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Kill every named session.
async fn kill_all(rmux: &rmux_sdk::Rmux, names: &[&rmux_sdk::SessionName]) {
    for name in names {
        if let Ok(live) = rmux.session((*name).clone()).await {
            let _ = session::kill_session(&live).await;
        }
    }
}

/// Process ids of the sandboxed daemon bound to `socket`, matched on the socket argument that is
/// unique to one test.
fn daemon_pids(socket: &std::path::Path) -> Vec<String> {
    let pattern = format!("--__internal-daemon {}", socket.display());
    // The `--` terminator is load-bearing: the pattern itself starts with `--`, which pgrep would
    // otherwise read as an unknown option and answer with an empty match.
    let Ok(output) = std::process::Command::new("pgrep")
        .arg("-f")
        .arg("--")
        .arg(&pattern)
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Leave no daemon behind: ask the sandboxed daemon to exit, then assert the process actually left.
///
/// No signal is sent, and that is the assertion. `run_internal_daemon` polls rmux over the very
/// socket rmux unlinks when it shuts down, so a daemon that treats every failed poll as "retry
/// later" can never observe its own server going away and spins forever — the leak that left one
/// resident daemon per test run. Detecting the absent endpoint and returning is what makes the
/// process exit on its own, so if this ever needs a `kill` again, that detection has regressed.
///
/// `Rmux::shutdown` consumes its handle, so the request goes out on a throwaway one, and it is
/// best-effort: a daemon whose last session just ended may have closed its transport already. The
/// exit is confirmed by the pid leaving, not by the request succeeding.
async fn shutdown_daemon(socket: &std::path::Path) {
    let rmux = rmux_sdk::Rmux::builder().unix_socket(socket.to_path_buf()).build();
    let _ = rmux.shutdown().await;

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let pids = daemon_pids(socket);
        if pids.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "sandboxed shells daemon {pids:?} on {} never exited; it must reap itself once its \
             rmux server is gone, without being signalled",
            socket.display()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn should_exit_cleanly_after_sigterm() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("shells.sock");
    let mut daemon = spawn_isolated_daemon(&socket);
    await_daemon_bound(&mut daemon, &socket).await;

    let daemon_pid = libc::pid_t::try_from(daemon.0.id()).expect("daemon pid fits pid_t");
    // SAFETY: this PID comes from the live child owned by `daemon`; the guard reaps that exact
    // process before the temporary socket directory is dropped.
    let signal_result = unsafe { libc::kill(daemon_pid, libc::SIGTERM) };
    assert_eq!(
        signal_result,
        0,
        "send SIGTERM to shells daemon: {}",
        std::io::Error::last_os_error()
    );
    await_daemon_exit(&mut daemon, "SIGTERM").await;
}

#[tokio::test]
async fn should_exit_when_bound_socket_is_unlinked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("shells.sock");
    let mut daemon = spawn_isolated_daemon(&socket);
    await_daemon_bound(&mut daemon, &socket).await;

    std::fs::remove_file(&socket).expect("unlink bound daemon socket");
    await_daemon_exit(&mut daemon, "its bound socket was unlinked").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_capture_kill_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = socket_in(&dir);
    let runtime = ShellRuntime::with_socket_path(socket.clone());

    let (_keepalive_id, keepalive_name) = spawn_bash(&runtime).await;

    let (session_id, name) = runtime
        .spawn(
            runtime.mint_session_id(),
            ShellCommand::Shell("echo basemind-hi; sleep 5".to_string()),
            None,
            Vec::new(),
            200,
            50,
        )
        .await
        .expect("spawn session");
    assert_ne!(name, keepalive_name, "sessions get distinct names");

    assert_eq!(
        runtime.resolve(&session_id).await.expect("resolve against the daemon"),
        Some(name.clone()),
        "minted session_id should resolve to the rmux session name"
    );

    let rmux = runtime.rmux().await.expect("rmux handle");
    await_marker(rmux, &name, "basemind-hi").await;

    let listed = session::list_sessions(rmux).await.expect("list sessions");
    assert!(
        listed.iter().any(|n| n == &name),
        "live session {name:?} should appear in list_sessions: {listed:?}"
    );

    let live = rmux.session(name.clone()).await.expect("open for kill");
    let killed = session::kill_session(&live).await.expect("kill");
    assert!(killed, "killing a live session returns true");

    await_gone(rmux, &name).await;
    assert_eq!(
        runtime.resolve(&session_id).await.expect("resolve after kill"),
        None,
        "a killed session_id no longer resolves"
    );

    kill_all(rmux, &[&keepalive_name]).await;
    shutdown_daemon(&socket).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broadcast_reaches_every_session_and_list_reports_alive() {
    const MARKER: &str = "S2-BROADCAST";

    let dir = tempfile::tempdir().expect("tempdir");
    let socket = socket_in(&dir);
    let runtime = ShellRuntime::with_socket_path(socket.clone());

    let (id_a, name_a) = spawn_bash(&runtime).await;
    let (id_b, name_b) = spawn_bash(&runtime).await;
    assert_ne!(name_a, name_b, "sessions get distinct names");

    let rmux = runtime.rmux().await.expect("rmux handle");

    let listed = runtime.list().await.expect("list sessions");
    assert_eq!(listed.len(), 2, "both spawned sessions are listed: {listed:?}");
    for id in [&id_a, &id_b] {
        let entry = listed
            .iter()
            .find(|info| &info.session_id == id)
            .unwrap_or_else(|| panic!("session {id} missing from list: {listed:?}"));
        assert!(entry.alive, "freshly spawned session {id} should be alive");
    }

    let delivered = runtime
        .broadcast(&[id_a.clone(), id_b.clone()], &format!("echo {MARKER}"), true)
        .await
        .expect("broadcast to both sessions");
    assert_eq!(delivered, 2, "broadcast delivered to both panes");

    for name in [&name_a, &name_b] {
        await_marker(rmux, name, MARKER).await;
    }

    kill_all(rmux, &[&name_a, &name_b]).await;
    shutdown_daemon(&socket).await;
}

/// Regression test for "it opens a shell but launches nothing in it": session resolution used to
/// be a process-local map, so a second CLI/MCP invocation saw an empty map and rejected a live
/// session as unknown. Resolution now goes to the shared daemon, so a runtime that never observed
/// the spawn must still resolve, list, drive, and capture the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_resolve_a_session_spawned_by_another_runtime() {
    const MARKER: &str = "S3-CROSS-RUNTIME";

    let dir = tempfile::tempdir().expect("tempdir");
    let socket = socket_in(&dir);

    let spawner = ShellRuntime::with_socket_path(socket.clone());
    let (session_id, name) = spawn_bash(&spawner).await;

    let observer = ShellRuntime::with_socket_path(socket.clone());

    assert_eq!(
        observer.resolve(&session_id).await.expect("observer resolve"),
        Some(name.clone()),
        "a runtime that never spawned {session_id} must still resolve it through the daemon"
    );

    let listed = observer.list().await.expect("observer list");
    let entry = listed
        .iter()
        .find(|info| info.session_id == session_id)
        .unwrap_or_else(|| panic!("session {session_id} missing from the observer's list: {listed:?}"));
    assert!(entry.alive, "a daemon-listed session is alive by construction");
    assert_eq!(entry.name, name, "the observer reports the spawner's rmux name");

    let delivered = observer
        .broadcast(std::slice::from_ref(&session_id), &format!("echo {MARKER}"), true)
        .await
        .expect("observer broadcast");
    assert_eq!(delivered, 1, "the observer drives the session it did not spawn");

    let rmux = observer.rmux().await.expect("observer rmux handle");
    await_marker(rmux, &name, MARKER).await;

    kill_all(rmux, &[&name]).await;
    await_gone(rmux, &name).await;
    assert_eq!(
        spawner.resolve(&session_id).await.expect("spawner resolve after kill"),
        None,
        "the spawning runtime sees the observer's kill — neither side caches its own view"
    );

    shutdown_daemon(&socket).await;
}
