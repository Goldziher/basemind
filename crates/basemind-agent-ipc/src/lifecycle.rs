//! Daemon bring-up: ensure a daemon is listening (spawning a detached one if not), and the detached
//! spawn primitive itself. Mirrors basemind's comms `singleton::ensure_daemon_with` /
//! `spawn_detached_daemon`, scoped to the per-workspace agent socket.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use basemind::daemon_lock::{
    DaemonKind, DaemonLock, DaemonLockOutcome, MAX_LIVE_DAEMONS_ENV, count_live_daemons_of, max_live_daemons,
};

use crate::error::IpcError;
use crate::socket::probe_alive;

/// How long to wait for a freshly spawned daemon to start answering before giving up.
const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll cadence while waiting for a spawning daemon to become ready.
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Replaces the socket extension to form one lock directory per workspace daemon.
const AGENT_DAEMON_LOCK_EXTENSION: &str = "daemon";

// `setsid` detaches the child into its own session so the daemon outlives the spawning shell;
// declared directly (as comms does) to avoid a libc dependency. ~keep
#[cfg(unix)]
unsafe extern "C" {
    fn setsid() -> i32;
}

/// Ensure an agent daemon is listening on `socket_path`, spawning one via `spawn` if not and the
/// machine-wide Agent-family daemon ceiling has capacity. The production entry point; `spawn` is
/// typically a closure that [`spawn_detached`]s the current binary in `--daemon` mode.
pub async fn ensure_daemon<F>(socket_path: &Path, spawn: F) -> Result<(), IpcError>
where
    F: FnOnce() -> io::Result<()>,
{
    ensure_daemon_with(socket_path, probe_alive, spawn).await
}

/// [`ensure_daemon`] with the liveness probe injected, so tests can drive the spawn/wait sequence
/// without a real socket: if `is_alive` already reports a daemon, return; otherwise enforce the
/// Agent-family daemon ceiling, `spawn` one, and poll `is_alive` until it answers or
/// [`SPAWN_READY_TIMEOUT`] elapses.
pub async fn ensure_daemon_with<P, F>(socket_path: &Path, is_alive: P, spawn: F) -> Result<(), IpcError>
where
    P: Fn(&Path) -> bool,
    F: FnOnce() -> io::Result<()>,
{
    ensure_daemon_with_ceiling(
        socket_path,
        is_alive,
        spawn,
        || count_live_daemons_of(DaemonKind::Agent),
        max_live_daemons(),
    )
    .await
}

async fn ensure_daemon_with_ceiling<P, F, C>(
    socket_path: &Path,
    is_alive: P,
    spawn: F,
    count_live: C,
    ceiling: usize,
) -> Result<(), IpcError>
where
    P: Fn(&Path) -> bool,
    F: FnOnce() -> io::Result<()>,
    C: FnOnce() -> usize + Send + 'static,
{
    if is_alive(socket_path) {
        return Ok(());
    }
    let live = tokio::task::spawn_blocking(count_live).await.map_err(|error| {
        IpcError::Io(io::Error::other(format!(
            "count live agent daemons before spawn: {error}"
        )))
    })?;
    if live >= ceiling {
        return Err(IpcError::Io(io::Error::other(format!(
            "agent daemon ceiling reached ({live}/{ceiling}); refusing to spawn another daemon; set \
             {MAX_LIVE_DAEMONS_ENV} to raise the limit"
        ))));
    }
    spawn()?;
    let deadline = Instant::now() + SPAWN_READY_TIMEOUT;
    loop {
        if is_alive(socket_path) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(IpcError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "agent daemon did not start listening within the readiness timeout",
            )));
        }
        tokio::time::sleep(SPAWN_POLL_INTERVAL).await;
    }
}

/// Derive the dedicated ownership directory for the daemon listening on `socket_path`.
///
/// Replacing `.sock` with `.daemon` keeps the Unix socket short while ensuring every workspace
/// daemon has a distinct flock and pidfile directory.
#[must_use]
pub fn agent_daemon_lock_dir(socket_path: &Path) -> PathBuf {
    socket_path.with_extension(AGENT_DAEMON_LOCK_EXTENSION)
}

/// Try to acquire process-lifetime ownership for the agent daemon at `socket_path` and record its
/// executable `version` in the machine registry.
///
/// The caller must hold an [`DaemonLockOutcome::Acquired`] lock for the full daemon lifetime and
/// exit successfully on [`DaemonLockOutcome::AlreadyHeld`].
pub fn acquire_agent_daemon_lock(socket_path: &Path, version: &str) -> io::Result<DaemonLockOutcome> {
    let daemon_dir = prepare_agent_daemon_dir(socket_path)?;
    DaemonLock::acquire_kind(DaemonKind::Agent, &daemon_dir, version)
}

fn prepare_agent_daemon_dir(socket_path: &Path) -> io::Result<PathBuf> {
    let daemon_dir = agent_daemon_lock_dir(socket_path);
    std::fs::create_dir_all(&daemon_dir)?;
    Ok(daemon_dir)
}

#[cfg(test)]
fn acquire_agent_daemon_lock_at(
    socket_path: &Path,
    version: &str,
    machine_dir: &Path,
) -> io::Result<DaemonLockOutcome> {
    let daemon_dir = prepare_agent_daemon_dir(socket_path)?;
    DaemonLock::acquire_at(DaemonKind::Agent, &daemon_dir, version, machine_dir)
}

/// Spawn `command` as a detached background process — null stdio and its own session (Unix) — then
/// return immediately without waiting. The caller builds `command` with the daemon argv (typically
/// the current binary in `--daemon` mode); this only applies the detachment.
pub fn spawn_detached(mut command: Command) -> io::Result<()> {
    use std::process::Stdio;

    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the pre_exec hook runs in the forked child before exec; `setsid` takes no
        // arguments, touches no shared parent state, and only moves the child into a new session so
        // it survives the parent shell exiting. ~keep
        unsafe {
            command.pre_exec(|| {
                if setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    // Spawn and drop the handle: the daemon is detached and must outlive this process. ~keep
    command.spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn ensure_returns_immediately_when_a_daemon_is_already_alive() {
        let spawned = AtomicUsize::new(0);
        ensure_daemon_with(
            &PathBuf::from("/unused"),
            |_| true,
            || {
                spawned.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect("already-alive is Ok");
        assert_eq!(spawned.load(Ordering::SeqCst), 0, "no spawn when already alive");
    }

    #[tokio::test]
    async fn ensure_spawns_then_waits_until_the_daemon_answers() {
        const AVAILABLE_CEILING: usize = 1;

        // The probe reports dead until `spawn` runs, then alive — proving the spawn-then-wait path. ~keep
        let spawned = AtomicUsize::new(0);
        ensure_daemon_with_ceiling(
            &PathBuf::from("/unused"),
            |_| spawned.load(Ordering::SeqCst) > 0,
            || {
                spawned.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || 0,
            AVAILABLE_CEILING,
        )
        .await
        .expect("becomes ready after spawn");
        assert_eq!(spawned.load(Ordering::SeqCst), 1, "spawned exactly once");
    }

    #[tokio::test]
    async fn ensure_propagates_a_spawn_error() {
        const AVAILABLE_CEILING: usize = 1;

        let error = ensure_daemon_with_ceiling(
            &PathBuf::from("/unused"),
            |_| false,
            || Err(io::Error::new(io::ErrorKind::PermissionDenied, "cannot spawn")),
            || 0,
            AVAILABLE_CEILING,
        )
        .await
        .expect_err("spawn failure propagates");
        assert!(
            matches!(error, IpcError::Io(_)),
            "spawn error surfaces as Io: {error:?}"
        );
    }

    #[tokio::test]
    async fn ensure_refuses_to_spawn_when_the_agent_daemon_ceiling_is_reached() {
        const TEST_CEILING: usize = 8;

        let spawned = AtomicUsize::new(0);
        let error = ensure_daemon_with_ceiling(
            &PathBuf::from("/unused"),
            |_| false,
            || {
                spawned.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || TEST_CEILING,
            TEST_CEILING,
        )
        .await
        .expect_err("the ceiling must reject another spawn");

        assert_eq!(spawned.load(Ordering::SeqCst), 0, "spawn was not attempted");
        let expected = format!("agent daemon ceiling reached ({TEST_CEILING}/{TEST_CEILING})");
        assert!(
            error.to_string().contains(&expected),
            "error names the exhausted agent ceiling: {error}"
        );
        assert!(
            error.to_string().contains("BASEMIND_MAX_DAEMONS"),
            "error names the operator override: {error}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ensure_counts_live_daemons_off_the_async_runtime_thread() {
        const AVAILABLE_CEILING: usize = 1;

        let caller_thread = std::thread::current().id();
        let counted_off_runtime = Arc::new(AtomicBool::new(false));
        let counted = Arc::clone(&counted_off_runtime);
        let spawned = AtomicUsize::new(0);
        ensure_daemon_with_ceiling(
            &PathBuf::from("/unused"),
            |_| spawned.load(Ordering::SeqCst) > 0,
            || {
                spawned.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            move || {
                counted.store(std::thread::current().id() != caller_thread, Ordering::SeqCst);
                0
            },
            AVAILABLE_CEILING,
        )
        .await
        .expect("count and spawn succeed");

        assert!(
            counted_off_runtime.load(Ordering::SeqCst),
            "the registry count must not block the async runtime thread"
        );
    }

    #[tokio::test]
    async fn ensure_adds_context_when_the_daemon_count_task_fails() {
        const AVAILABLE_CEILING: usize = 1;

        let spawned = AtomicUsize::new(0);
        let error = ensure_daemon_with_ceiling(
            &PathBuf::from("/unused"),
            |_| false,
            || {
                spawned.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || panic!("count task failed"),
            AVAILABLE_CEILING,
        )
        .await
        .expect_err("a failed count task must surface an IPC error");

        assert_eq!(spawned.load(Ordering::SeqCst), 0, "spawn was not attempted");
        assert!(
            error.to_string().contains("count live agent daemons before spawn"),
            "error identifies the failed registry operation: {error}"
        );
    }

    #[test]
    fn lock_directory_is_dedicated_to_one_agent_socket() {
        let socket = PathBuf::from("/cache/agent/0123456789abcdef.sock");
        assert_eq!(
            agent_daemon_lock_dir(&socket),
            PathBuf::from("/cache/agent/0123456789abcdef.daemon")
        );
    }

    #[test]
    fn daemon_lock_converges_a_second_owner_before_socket_bind() {
        let temp = tempfile::tempdir().expect("temp dir");
        let socket = temp.path().join("agent/workspace.sock");
        let machine_dir = temp.path().join("machine-daemons");
        let first = acquire_agent_daemon_lock_at(&socket, "test-version", &machine_dir).expect("first acquisition");
        let _held = match first {
            DaemonLockOutcome::Acquired(lock) => lock,
            DaemonLockOutcome::AlreadyHeld(_) => panic!("first owner must acquire the lock"),
        };

        let second = acquire_agent_daemon_lock_at(&socket, "test-version", &machine_dir).expect("second acquisition");
        match second {
            DaemonLockOutcome::AlreadyHeld(Some(record)) => {
                assert_eq!(record.kind, DaemonKind::Agent);
                assert_eq!(record.dir, agent_daemon_lock_dir(&socket));
            }
            outcome => panic!("second owner must converge, got {outcome:?}"),
        }
    }
}
