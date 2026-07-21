//! Daemon bring-up: ensure a daemon is listening (spawning a detached one if not), and the detached
//! spawn primitive itself. Mirrors basemind's comms `singleton::ensure_daemon_with` /
//! `spawn_detached_daemon`, scoped to the per-workspace agent socket.

use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::error::IpcError;
use crate::socket::probe_alive;

/// How long to wait for a freshly spawned daemon to start answering before giving up.
const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll cadence while waiting for a spawning daemon to become ready.
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(50);

// `setsid` detaches the child into its own session so the daemon outlives the spawning shell;
// declared directly (as comms does) to avoid a libc dependency. ~keep
#[cfg(unix)]
unsafe extern "C" {
    fn setsid() -> i32;
}

/// Ensure an agent daemon is listening on `socket_path`, spawning one via `spawn` if not. The
/// production entry point; `spawn` is typically a closure that [`spawn_detached`]s the current
/// binary in `--daemon` mode.
pub async fn ensure_daemon<F>(socket_path: &Path, spawn: F) -> Result<(), IpcError>
where
    F: FnOnce() -> io::Result<()>,
{
    ensure_daemon_with(socket_path, probe_alive, spawn).await
}

/// [`ensure_daemon`] with the liveness probe injected, so tests can drive the spawn/wait sequence
/// without a real socket: if `is_alive` already reports a daemon, return; otherwise `spawn` one and
/// poll `is_alive` until it answers or [`SPAWN_READY_TIMEOUT`] elapses.
pub async fn ensure_daemon_with<P, F>(socket_path: &Path, is_alive: P, spawn: F) -> Result<(), IpcError>
where
    P: Fn(&Path) -> bool,
    F: FnOnce() -> io::Result<()>,
{
    if is_alive(socket_path) {
        return Ok(());
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        // The probe reports dead until `spawn` runs, then alive — proving the spawn-then-wait path. ~keep
        let spawned = AtomicUsize::new(0);
        ensure_daemon_with(
            &PathBuf::from("/unused"),
            |_| spawned.load(Ordering::SeqCst) > 0,
            || {
                spawned.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect("becomes ready after spawn");
        assert_eq!(spawned.load(Ordering::SeqCst), 1, "spawned exactly once");
    }

    #[tokio::test]
    async fn ensure_propagates_a_spawn_error() {
        let error = ensure_daemon_with(
            &PathBuf::from("/unused"),
            |_| false,
            || Err(io::Error::new(io::ErrorKind::PermissionDenied, "cannot spawn")),
        )
        .await
        .expect_err("spawn failure propagates");
        assert!(
            matches!(error, IpcError::Io(_)),
            "spawn error surfaces as Io: {error:?}"
        );
    }
}
