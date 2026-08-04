//! Single-daemon invariant, end to end against the real `basemind` binary.
//!
//! Proves the safeguards that killed the process/memory leak class: a redundant daemon on one comms
//! dir converges (exits 0) instead of running alongside; a spawn past the machine ceiling is refused
//! loudly; `comms doctor` / `comms stop --all` inspect and reclaim the registry; and a daemon nobody
//! ever connects to self-terminates within the bootstrap window rather than lingering.
//!
//! Every test isolates `BASEMIND_DATA_HOME` + `BASEMIND_COMMS_DIR` to its own tempdir and passes them
//! via `Command::env` (never the process env), so the tests are parallel-safe and never touch the
//! user's real machine daemon. Each spawned daemon is reaped on drop.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use basemind::comms::singleton::{comms_socket_path, probe_alive};

const BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// A real `basemind comms daemon` spawned into an isolated cache, stopped (best-effort) and killed on
/// drop so a panicking or early-returning test never leaks a detached daemon.
struct ReapingDaemon {
    child: Child,
    comms_dir: PathBuf,
    data_home: PathBuf,
}

impl ReapingDaemon {
    /// Spawn a daemon bound to `comms_dir` under `data_home` with `extra_env`, and wait until it
    /// answers a ping (or panic after a generous deadline).
    fn spawn(comms_dir: &Path, data_home: &Path, extra_env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(BIN);
        command
            .args(["comms", "daemon"])
            .env("BASEMIND_COMMS_DIR", comms_dir)
            .env("BASEMIND_DATA_HOME", data_home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let child = command.spawn().expect("spawn comms daemon");
        let daemon = Self {
            child,
            comms_dir: comms_dir.to_path_buf(),
            data_home: data_home.to_path_buf(),
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if daemon.is_alive() {
                return daemon;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("comms daemon did not become ready");
    }

    fn is_alive(&self) -> bool {
        probe_alive(&comms_socket_path(&self.comms_dir))
    }
}

impl Drop for ReapingDaemon {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .args(["comms", "stop"])
            .env("BASEMIND_COMMS_DIR", &self.comms_dir)
            .env("BASEMIND_DATA_HOME", &self.data_home)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
        if self.child.try_wait().ok().flatten().is_none() {
            std::thread::sleep(Duration::from_millis(200));
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// Poll a child for exit up to `timeout`, returning its status or `None` if still running.
fn wait_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Run a `basemind` subcommand to completion under an isolated cache, returning (status, stdout).
fn run_bin(comms_dir: &Path, data_home: &Path, extra_env: &[(&str, &str)], args: &[&str]) -> (ExitStatus, String) {
    let mut command = Command::new(BIN);
    command
        .args(args)
        .env("BASEMIND_COMMS_DIR", comms_dir)
        .env("BASEMIND_DATA_HOME", data_home);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let output = command.output().expect("run basemind");
    (output.status, String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Wait until the machine registry (via `comms doctor --json`) reports `want` live daemons, or fail.
fn await_doctor_count(comms_dir: &Path, data_home: &Path, want: u64, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    while Instant::now() < deadline {
        let (status, out) = run_bin(comms_dir, data_home, &[], &["comms", "doctor", "--json"]);
        assert!(status.success(), "comms doctor failed: {out}");
        last = out.clone();
        if out.contains(&format!("\"count\":{want}")) {
            return out;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("comms doctor never reported count {want}; last: {last}");
}

#[test]
fn two_daemons_on_one_comms_dir_converge_to_one() {
    let home = tempfile::tempdir().expect("data home");
    let comms = home.path().join("comms");
    std::fs::create_dir_all(&comms).expect("comms dir");

    let first = ReapingDaemon::spawn(&comms, home.path(), &[]);
    assert!(first.is_alive(), "the first daemon answers");

    // A second daemon on the SAME comms dir must lose the lock and exit 0 — never run alongside.
    let mut second = Command::new(BIN)
        .args(["comms", "daemon"])
        .env("BASEMIND_COMMS_DIR", &comms)
        .env("BASEMIND_DATA_HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn second daemon");
    let status = wait_timeout(&mut second, Duration::from_secs(10)).expect("the redundant daemon exits promptly");
    assert!(
        status.success(),
        "the redundant daemon converges with exit 0, got {status:?}"
    );
    assert!(
        first.is_alive(),
        "the original daemon still owns the comms dir after the redundant one exits"
    );

    // Exactly one daemon is registered on the machine.
    await_doctor_count(&comms, home.path(), 1, Duration::from_secs(5));
}

#[test]
fn spawning_past_the_ceiling_is_refused() {
    let home = tempfile::tempdir().expect("data home");
    let comms_one = home.path().join("comms-1");
    let comms_two = home.path().join("comms-2");
    std::fs::create_dir_all(&comms_one).expect("comms-1");
    std::fs::create_dir_all(&comms_two).expect("comms-2");

    // One live daemon under this data home, ceiling of 1.
    let _first = ReapingDaemon::spawn(&comms_one, home.path(), &[("BASEMIND_MAX_DAEMONS", "1")]);
    await_doctor_count(&comms_one, home.path(), 1, Duration::from_secs(5));

    // `comms start` in a DIFFERENT comms dir under the same data home must be refused, not spawn an
    // N+1-th daemon.
    let (status, _out) = run_bin(
        &comms_two,
        home.path(),
        &[("BASEMIND_MAX_DAEMONS", "1")],
        &["comms", "start"],
    );
    assert!(!status.success(), "spawning past the ceiling must fail");

    // Still only one daemon is registered, and doctor echoes the effective ceiling (passed here so
    // its `max_live_daemons()` reads the same override).
    let (status, doctor) = run_bin(
        &comms_one,
        home.path(),
        &[("BASEMIND_MAX_DAEMONS", "1")],
        &["comms", "doctor", "--json"],
    );
    assert!(status.success(), "comms doctor failed: {doctor}");
    assert!(doctor.contains("\"count\":1"), "exactly one daemon remains: {doctor}");
    assert!(doctor.contains("\"ceiling\":1"), "doctor echoes the ceiling: {doctor}");
}

#[test]
fn doctor_lists_and_stop_all_reclaims() {
    let home = tempfile::tempdir().expect("data home");
    let comms = home.path().join("comms");
    std::fs::create_dir_all(&comms).expect("comms dir");

    let _daemon = ReapingDaemon::spawn(&comms, home.path(), &[]);
    let doctor = await_doctor_count(&comms, home.path(), 1, Duration::from_secs(5));
    assert!(doctor.contains("\"count\":1"), "one live daemon listed: {doctor}");

    let (status, out) = run_bin(&comms, home.path(), &[], &["comms", "stop", "--all"]);
    assert!(status.success(), "stop --all succeeds: {out}");

    await_doctor_count(&comms, home.path(), 0, Duration::from_secs(10));
}

#[test]
fn a_daemon_nobody_connects_to_self_terminates() {
    let home = tempfile::tempdir().expect("data home");
    let comms = home.path().join("comms");
    std::fs::create_dir_all(&comms).expect("comms dir");

    // Bootstrap window of 2s, idle window huge: the only thing that can reap it is the bootstrap
    // path, and only because no client ever connects.
    let mut child = Command::new(BIN)
        .args(["comms", "daemon"])
        .env("BASEMIND_COMMS_DIR", &comms)
        .env("BASEMIND_DATA_HOME", home.path())
        .env("BASEMIND_COMMS_BOOTSTRAP_SECS", "2")
        .env("BASEMIND_COMMS_IDLE_REAP_SECS", "600")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn abandoned daemon");

    // Never connect. It must self-terminate on the bootstrap timeout, well before the idle window.
    let status = wait_timeout(&mut child, Duration::from_secs(15));
    match status {
        Some(status) => assert!(status.success(), "the abandoned daemon exits cleanly, got {status:?}"),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("an abandoned daemon must self-terminate within the bootstrap window");
        }
    }
}
