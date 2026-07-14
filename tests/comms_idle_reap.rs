//! Idle self-reap: a daemon nobody is using must exit; a daemon somebody IS using must not.
//!
//! The daemon is machine-wide and auto-spawned on demand (`singleton::spawn_detached_daemon`), so
//! exiting when genuinely idle is safe: the next client that needs one respawns it. Without the
//! reap, every session (and every `cargo test --features comms` run, which spawns a daemon against
//! a throwaway `BASEMIND_COMMS_DIR`) leaves a resident process behind forever.
//!
//! The two tests here are a matched pair and BOTH are load-bearing:
//!
//! * `idle_daemon_self_terminates_and_a_fresh_one_respawns` pins the reap itself, plus the two
//!   things that make the reap safe rather than merely convenient — the socket is unlinked on the
//!   way out, and the auto-spawn path still brings up a working daemon on the same comms dir
//!   afterwards (i.e. the exit released the store's locks cleanly instead of orphaning them).
//! * `daemon_with_a_connected_client_does_not_reap` is what stops the reap from being "fixed" by
//!   exiting eagerly. A connected client is the daemon's proxy for *work in flight*: a forwarded
//!   git-history build on a large repo runs for a minute-plus with ZERO socket traffic, and a
//!   daemon that mistook that silence for idleness would kill the build. The client here is
//!   deliberately silent after its handshake, which is exactly that shape.

#![cfg(all(feature = "comms", unix))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use basemind::comms::client::CommsClient;
use basemind::comms::daemon::{IDLE_REAP_AFTER_ENV, IDLE_REAP_CHECK_EVERY_ENV};
use basemind::comms::ids::AgentId;
use basemind::comms::singleton::{CommsPaths, comms_socket_path, probe_alive};

const BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// Idle window the daemons under test are configured with. Short enough to keep the suite fast,
/// long enough that a merely-slow boot is not mistaken for an idle reap.
const TEST_REAP_AFTER: Duration = Duration::from_secs(2);
/// How often the daemons under test re-check idleness.
const TEST_REAP_CHECK_EVERY: Duration = Duration::from_secs(1);
/// Ceiling on how long we wait for a reap that the daemon owes us. Generously over
/// `TEST_REAP_AFTER + TEST_REAP_CHECK_EVERY` so a loaded CI box cannot flake it.
const REAP_DEADLINE: Duration = Duration::from_secs(30);

/// A spawned daemon process, always reaped on drop so a failing assertion cannot leak one.
struct Daemon {
    child: Child,
    socket: PathBuf,
}

impl Daemon {
    /// Spawn `basemind comms daemon` against a private comms dir with a short idle window.
    fn start(comms_dir: &Path) -> Self {
        let child = Command::new(BIN)
            .args(["comms", "daemon"])
            .env("BASEMIND_COMMS_DIR", comms_dir)
            // Keep the registry snapshot + any index writes inside the tempdir: this test must
            // never touch the real XDG cache (a live session may be using it).
            .env("BASEMIND_DATA_HOME", comms_dir)
            .env(IDLE_REAP_AFTER_ENV, TEST_REAP_AFTER.as_secs().to_string())
            .env(IDLE_REAP_CHECK_EVERY_ENV, TEST_REAP_CHECK_EVERY.as_secs().to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn comms daemon");
        let daemon = Self {
            child,
            socket: comms_socket_path(comms_dir),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if probe_alive(&daemon.socket) {
                return daemon;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon never came up on {}", daemon.socket.display());
    }

    /// Wait up to `within` for the daemon to exit ON ITS OWN. `true` if it did.
    fn waited_for_self_exit(&mut self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            match self.child.try_wait().expect("try_wait") {
                Some(_) => return true,
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        false
    }

    /// True while the process is still running.
    fn is_running(&mut self) -> bool {
        self.child.try_wait().expect("try_wait").is_none()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Only ever kills the child WE spawned — never a broadly-matched `basemind` process.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn paths_for(comms_dir: &Path) -> CommsPaths {
    CommsPaths {
        comms_dir: comms_dir.to_path_buf(),
        socket_path: comms_socket_path(comms_dir),
    }
}

/// An idle daemon exits on its own, unlinks its socket, and leaves the comms dir in a state a
/// freshly auto-spawned daemon can open (locks released, nothing torn).
#[test]
fn idle_daemon_self_terminates_and_a_fresh_one_respawns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let comms_dir = dir.path().join("comms");
    std::fs::create_dir_all(&comms_dir).expect("create comms dir");

    let mut daemon = Daemon::start(&comms_dir);
    assert!(
        daemon.waited_for_self_exit(REAP_DEADLINE),
        "an idle daemon must self-terminate within its reap window, but it was still running \
         after {REAP_DEADLINE:?}"
    );
    assert!(
        !daemon.socket.exists(),
        "the reaped daemon must unlink its socket on the way out, or the next client dials a \
         dead endpoint instead of respawning"
    );

    // The auto-spawn path must still work after an idle exit, and the fresh daemon must be able to
    // open the very same store the reaped one owned — i.e. the exit released its locks cleanly.
    let mut respawned = Daemon::start(&comms_dir);
    assert!(
        respawned.is_running(),
        "a daemon must respawn against the comms dir an idle daemon just released"
    );
    let paths = paths_for(&comms_dir);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let mut client = CommsClient::connect(&paths, AgentId::parse("reap-probe").expect("agent id"), None, None)
            .await
            .expect("the respawned daemon must serve a fresh client");
        client.status().await.expect("respawned daemon answers Status");
    });
}

/// A daemon holding a connected client must NOT reap, even though that client is completely silent
/// past its handshake. Silence on the socket is not idleness: a forwarded scan or git-history build
/// is exactly this shape (one open link, no traffic, minutes of work).
#[test]
fn daemon_with_a_connected_client_does_not_reap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let comms_dir = dir.path().join("comms");
    std::fs::create_dir_all(&comms_dir).expect("create comms dir");

    let mut daemon = Daemon::start(&comms_dir);
    let paths = paths_for(&comms_dir);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");

    let client = runtime.block_on(async {
        CommsClient::connect(&paths, AgentId::parse("holder").expect("agent id"), None, None)
            .await
            .expect("connect to daemon")
    });

    // Sit silent for several multiples of the idle window. A daemon that reaped on socket silence
    // would be long gone by now.
    let quiet_for = TEST_REAP_AFTER * 4 + TEST_REAP_CHECK_EVERY * 2;
    std::thread::sleep(quiet_for);

    assert!(
        daemon.is_running(),
        "the daemon must not reap while a client holds a link, but it exited after {quiet_for:?} \
         of socket silence"
    );
    assert!(
        probe_alive(&daemon.socket),
        "a daemon with a connected client must still be serving its socket"
    );

    // Releasing the last client makes it genuinely idle — and then it MUST go. Without this the
    // test above would still pass on a daemon that simply never exits.
    drop(client);
    drop(runtime);
    assert!(
        daemon.waited_for_self_exit(REAP_DEADLINE),
        "once its last client disconnects the daemon is idle and must self-terminate"
    );
}

// NOTE: the third hazard — a LONG forwarded RPC (a scan / git-history build that runs for seconds
// to minutes inside a single request) — is deliberately NOT covered here. The only honest way to
// exercise it end-to-end is to scan a repo big enough to outlast the idle window, and a test that
// does that is both slow and a bad parallel citizen: it pegs the CPU while the neighbouring daemon
// suites are racing their own readiness deadlines, and it made `concurrency_smoke` flake.
//
// It is covered where it is cheap to cover instead: the link refcount that protects it is the same
// one `daemon_with_a_connected_client_does_not_reap` pins above (the guard is taken at accept and
// released when the link closes — how long a request takes in between cannot affect it), and the
// linkless variant, daemon-internal work with no client attached, is pinned directly by
// `comms::daemon::tests::work_in_flight_blocks_the_idle_reap_even_with_no_links`.
