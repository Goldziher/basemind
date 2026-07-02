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

use std::time::{Duration, Instant};

use basemind::shells::ShellRuntime;
use basemind::shells::session::{self, ShellCommand};
use tempfile::TempDir;

/// Build a runtime sandboxed to its own socket under `dir`, with the SDK's
/// daemon-binary discovery pointed at the built `basemind` executable (which
/// carries the `--__internal-daemon` intercept the test-harness binary lacks).
fn runtime_in(dir: &TempDir) -> ShellRuntime {
    let daemon = std::path::PathBuf::from(env!("CARGO_BIN_EXE_basemind"));
    // SAFETY: `set_var` is not thread-safe under the 2024 edition. This runs once
    // at the very start of the test, before any rmux interaction, and the test
    // performs no other concurrent environment access.
    unsafe { basemind::shells::daemon::point_sdk_daemon_at(&daemon) }
    let socket = dir.path().join("shells.sock");
    ShellRuntime::with_socket_path(socket)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_capture_kill_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = runtime_in(&dir);

    // Keep-alive session: the embedded daemon self-terminates once it has no
    // sessions left, so hold one long-lived idle session open for the whole test
    // and let the post-kill `list_sessions` exchange survive killing the session
    // under test.
    let (keepalive_id, keepalive_name) = runtime
        .spawn(
            runtime.mint_session_id(),
            ShellCommand::Shell("sleep 60".to_string()),
            None,
            Vec::new(),
            200,
            50,
        )
        .await
        .expect("spawn keepalive session");

    // Spawn a session that prints a sentinel, then idles long enough to capture
    // it before the pane process exits.
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

    // The session must be addressable by the minted id.
    assert_eq!(
        runtime.resolve(&session_id).await.as_ref(),
        Some(&name),
        "minted session_id should resolve to the rmux session name"
    );

    // Poll capture until the sentinel shows up (bounded), so the test is not
    // flaky against shell/daemon startup latency.
    let rmux = runtime.rmux().await.expect("rmux handle");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let live = rmux.session(name.clone()).await.expect("open live session");
        let captured = session::capture(&live, None).await.expect("capture");
        if captured.contains("basemind-hi") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for sentinel; last capture was {captured:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // The session should be listed while alive.
    let listed = session::list_sessions(rmux).await.expect("list sessions");
    assert!(
        listed.iter().any(|n| n == &name),
        "live session {name:?} should appear in list_sessions: {listed:?}"
    );

    // Kill it and confirm it disappears from the listing.
    let live = rmux.session(name.clone()).await.expect("open for kill");
    let killed = session::kill_session(&live).await.expect("kill");
    assert!(killed, "killing a live session returns true");
    runtime.forget(&session_id).await;

    // Poll the listing until the session is gone (kill is async on the daemon).
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let listed = session::list_sessions(rmux).await.expect("list after kill");
        if !listed.iter().any(|n| n == &name) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session {name:?} still listed after kill: {listed:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        runtime.resolve(&session_id).await.is_none(),
        "forgotten session_id should no longer resolve"
    );

    // Tear down the keep-alive session; this is the last one, so the daemon may
    // self-terminate afterward — that is fine, the assertions are done.
    let keepalive = rmux.session(keepalive_name).await.expect("open keepalive for kill");
    let _ = session::kill_session(&keepalive).await;
    runtime.forget(&keepalive_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broadcast_reaches_every_session_and_list_reports_alive() {
    const MARKER: &str = "S2-BROADCAST";

    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = runtime_in(&dir);

    // Two long-lived shells reading stdin: bare `bash` stays alive waiting on
    // stdin, so a broadcast line is read, executed, and the marker rendered.
    let (id_a, name_a) = runtime
        .spawn(
            runtime.mint_session_id(),
            ShellCommand::Argv(vec!["bash".to_string()]),
            None,
            Vec::new(),
            200,
            50,
        )
        .await
        .expect("spawn session A");
    let (id_b, name_b) = runtime
        .spawn(
            runtime.mint_session_id(),
            ShellCommand::Argv(vec!["bash".to_string()]),
            None,
            Vec::new(),
            200,
            50,
        )
        .await
        .expect("spawn session B");
    assert_ne!(name_a, name_b, "sessions get distinct names");

    let rmux = runtime.rmux().await.expect("rmux handle");

    // Both sessions should appear alive in the runtime listing.
    let listed = runtime.list().await.expect("list sessions");
    assert_eq!(listed.len(), 2, "both spawned sessions are listed: {listed:?}");
    for id in [&id_a, &id_b] {
        let entry = listed
            .iter()
            .find(|info| &info.session_id == id)
            .unwrap_or_else(|| panic!("session {id} missing from list: {listed:?}"));
        assert!(entry.alive, "freshly spawned session {id} should be alive");
    }

    // Broadcast a command that echoes a unique marker into both shells at once.
    let delivered = runtime
        .broadcast(&[id_a.clone(), id_b.clone()], &format!("echo {MARKER}"), true)
        .await
        .expect("broadcast to both sessions");
    assert_eq!(delivered, 2, "broadcast delivered to both panes");

    // Poll each pane until the marker is rendered (bounded, to absorb latency).
    for name in [&name_a, &name_b] {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let live = rmux.session(name.clone()).await.expect("open live session");
            let captured = session::capture(&live, None).await.expect("capture");
            if captured.contains(MARKER) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {MARKER} in {name:?}; last capture was {captured:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // Tear both sessions down.
    for (id, name) in [(&id_a, &name_a), (&id_b, &name_b)] {
        let live = rmux.session(name.clone()).await.expect("open for kill");
        let _ = session::kill_session(&live).await;
        runtime.forget(id).await;
    }
}
