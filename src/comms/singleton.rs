//! Singleton-daemon machinery: per-user path resolution, bind-as-lock, and spawn-on-demand.
//!
//! The broker is a **per-user, repo-independent singleton**. Its socket + store live under the
//! user's data directory (`directories::ProjectDirs`), never inside any repo's `.basemind/`.
//!
//! ## Bind-as-lock
//!
//! Binding the Unix listener IS the singleton lock: the kernel guarantees only one process can
//! own a given socket path. [`bind_listener`] reclaims a stale socket only after probing it —
//! if a live daemon answers a ping, we back off; if nothing answers, the socket is an orphan
//! from a crashed daemon and we unlink + rebind. This probe-before-unlink keeps two daemons
//! from racing into a split brain.

use std::path::{Path, PathBuf};
use std::time::Duration;

use directories::ProjectDirs;

/// Subdirectory under the user data dir holding the comms socket + store.
const COMMS_SUBDIR: &str = "comms";
/// The Unix socket file name within [`COMMS_SUBDIR`].
const SOCKET_FILE: &str = "comms.sock";
/// Octal mode for the socket + comms dir: owner-only (rwx for the dir, rw for the socket).
#[cfg(unix)]
const OWNER_ONLY_DIR: u32 = 0o700;
#[cfg(unix)]
const OWNER_ONLY_FILE: u32 = 0o600;

/// How long to wait for a spawned daemon to become reachable before giving up.
const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval while waiting for a spawned daemon.
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Resolved per-user comms paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommsPaths {
    /// The `<data_dir>/comms/` directory holding the store and socket.
    pub comms_dir: PathBuf,
    /// The Unix socket path (Unix) — clients connect here.
    pub socket_path: PathBuf,
}

/// Errors from path resolution / daemon bring-up.
#[derive(Debug, thiserror::Error)]
pub enum SingletonError {
    /// The platform could not provide a per-user data directory.
    #[error("could not resolve a per-user data directory for basemind")]
    NoDataDir,
    /// An io failure with the offending path.
    #[error("io error on {path}: {source}")]
    Io {
        /// The path the operation targeted.
        path: PathBuf,
        /// The underlying io error.
        #[source]
        source: std::io::Error,
    },
    /// A live daemon already holds the socket.
    #[error("a comms daemon is already running at {0}")]
    AlreadyRunning(PathBuf),
    /// A spawned daemon did not become reachable in time.
    #[error("spawned comms daemon did not become ready within the timeout")]
    SpawnTimeout,
}

/// Environment override for the comms data directory. When set, it is used verbatim as the
/// `comms_dir` instead of the per-user `directories::ProjectDirs` location. Intended for tests,
/// CI, and users who want the broker's socket + store in a custom (e.g. sandboxed) location.
pub const COMMS_DIR_ENV: &str = "BASEMIND_COMMS_DIR";

/// Resolve the per-user comms paths via `directories::ProjectDirs::from("", "", "basemind")`,
/// or the [`COMMS_DIR_ENV`] override when set. Creates the dir (mode 0700 on Unix) as a side effect.
pub fn resolve_paths() -> Result<CommsPaths, SingletonError> {
    let comms_dir = match std::env::var_os(COMMS_DIR_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let dirs = ProjectDirs::from("", "", "basemind").ok_or(SingletonError::NoDataDir)?;
            dirs.data_dir().join(COMMS_SUBDIR)
        }
    };
    std::fs::create_dir_all(&comms_dir).map_err(|source| SingletonError::Io {
        path: comms_dir.clone(),
        source,
    })?;
    #[cfg(unix)]
    set_mode(&comms_dir, OWNER_ONLY_DIR)?;

    let socket_path = comms_socket_path(&comms_dir);
    Ok(CommsPaths {
        comms_dir,
        socket_path,
    })
}

/// The socket path for a resolved comms dir. On Windows, a named-pipe path (see the TODO in
/// `frontend_uds`); on Unix, `<comms_dir>/comms.sock`.
pub fn comms_socket_path(comms_dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let _ = comms_dir;
        // Per-user named pipe. The username keeps it user-scoped on shared hosts.
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_string());
        PathBuf::from(format!(r"\\.\pipe\basemind-comms-{user}"))
    }
    #[cfg(not(windows))]
    {
        comms_dir.join(SOCKET_FILE)
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), SingletonError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|source| {
        SingletonError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

/// Bind the singleton Unix listener, reclaiming a stale socket only after a probe confirms no
/// live daemon answers. Returns the bound listener (the bind IS the lock).
///
/// `probe` is invoked on the existing socket path to decide live-vs-stale; it should attempt a
/// connect + ping and return `true` only when a daemon answered. Injected so tests can drive
/// the race deterministically.
#[cfg(unix)]
pub fn bind_listener(
    socket_path: &Path,
    probe: impl Fn(&Path) -> bool,
) -> Result<tokio::net::UnixListener, SingletonError> {
    use std::os::unix::fs::PermissionsExt;

    // First attempt: a clean bind wins the lock outright.
    match std::os::unix::net::UnixListener::bind(socket_path) {
        Ok(std_listener) => {
            std_listener
                .set_nonblocking(true)
                .map_err(|source| SingletonError::Io {
                    path: socket_path.to_path_buf(),
                    source,
                })?;
            let _ = std::fs::set_permissions(
                socket_path,
                std::fs::Permissions::from_mode(OWNER_ONLY_FILE),
            );
            tokio::net::UnixListener::from_std(std_listener).map_err(|source| SingletonError::Io {
                path: socket_path.to_path_buf(),
                source,
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // The path is occupied: a live daemon, or a stale socket from a crash. Probe.
            if probe(socket_path) {
                return Err(SingletonError::AlreadyRunning(socket_path.to_path_buf()));
            }
            // Stale: unlink and rebind once.
            std::fs::remove_file(socket_path).map_err(|source| SingletonError::Io {
                path: socket_path.to_path_buf(),
                source,
            })?;
            let std_listener =
                std::os::unix::net::UnixListener::bind(socket_path).map_err(|source| {
                    SingletonError::Io {
                        path: socket_path.to_path_buf(),
                        source,
                    }
                })?;
            std_listener
                .set_nonblocking(true)
                .map_err(|source| SingletonError::Io {
                    path: socket_path.to_path_buf(),
                    source,
                })?;
            let _ = std::fs::set_permissions(
                socket_path,
                std::fs::Permissions::from_mode(OWNER_ONLY_FILE),
            );
            tokio::net::UnixListener::from_std(std_listener).map_err(|source| SingletonError::Io {
                path: socket_path.to_path_buf(),
                source,
            })
        }
        Err(source) => Err(SingletonError::Io {
            path: socket_path.to_path_buf(),
            source,
        }),
    }
}

/// Ensure a daemon is running and reachable: probe-connect + ping; if that succeeds, return
/// without doing anything. Otherwise spawn `basemind comms daemon` detached and poll the
/// socket until it answers (or the timeout elapses).
///
/// `is_alive` probes the socket (connect + ping). `spawn` launches the detached daemon. Both
/// are injected so the unit tests can exercise the control flow without a real process; the
/// production wiring in [`ensure_daemon`] supplies the real implementations.
pub async fn ensure_daemon_with(
    paths: &CommsPaths,
    is_alive: impl Fn(&Path) -> bool,
    spawn: impl FnOnce(&CommsPaths) -> std::io::Result<()>,
) -> Result<(), SingletonError> {
    if is_alive(&paths.socket_path) {
        return Ok(());
    }
    spawn(paths).map_err(|source| SingletonError::Io {
        path: paths.socket_path.clone(),
        source,
    })?;
    // Poll until ready.
    let deadline = std::time::Instant::now() + SPAWN_READY_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if is_alive(&paths.socket_path) {
            return Ok(());
        }
        tokio::time::sleep(SPAWN_POLL_INTERVAL).await;
    }
    Err(SingletonError::SpawnTimeout)
}

/// Production [`ensure_daemon_with`]: probes via a real connect+ping and spawns
/// `basemind comms daemon` detached.
pub async fn ensure_daemon(paths: &CommsPaths) -> Result<(), SingletonError> {
    ensure_daemon_with(paths, probe_alive, spawn_detached_daemon).await
}

/// Probe whether a daemon is alive at `socket_path` by connecting and pinging it. Synchronous
/// (uses a blocking connect with a short timeout) so it can be used as the `probe` in
/// [`bind_listener`] too.
#[cfg(unix)]
pub fn probe_alive(socket_path: &Path) -> bool {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let Ok(mut stream) = UnixStream::connect(socket_path) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));

    // Frame a Ping request: u32-be length prefix + msgpack body (matches LengthDelimitedCodec).
    let body = match rmp_serde::to_vec_named(&super::protocol::CommsRequest::Ping) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let len = match u32::try_from(body.len()) {
        Ok(l) => l,
        Err(_) => return false,
    };
    if stream.write_all(&len.to_be_bytes()).is_err() || stream.write_all(&body).is_err() {
        return false;
    }
    // Read the response frame's length prefix; any well-formed reply means "alive".
    let mut prefix = [0u8; 4];
    stream.read_exact(&mut prefix).is_ok()
}

#[cfg(not(unix))]
pub fn probe_alive(_socket_path: &Path) -> bool {
    false
}

/// Spawn `basemind comms daemon` detached so it outlives the spawning process. stdout/stderr
/// are redirected to null; the daemon's own tracing goes to its log sink.
pub fn spawn_detached_daemon(_paths: &CommsPaths) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("comms")
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Detach from the parent's process group on Unix so a parent exit (or Ctrl-C) does not
    // take the daemon with it.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` takes no arguments and only detaches the child into a new session.
        // It is called in the child between fork and exec; touching no parent state makes it
        // safe in the `pre_exec` context (no allocation, no locks).
        unsafe {
            command.pre_exec(|| {
                // Best-effort: ignore the rare EPERM (already a group leader).
                let _ = detach_session();
                Ok(())
            });
        }
    }
    command.spawn()?;
    Ok(())
}

#[cfg(unix)]
fn detach_session() -> std::io::Result<()> {
    // SAFETY: `setsid()` takes no arguments, reads no caller memory, and creates a new session
    // with the calling process as leader. It can only fail with EPERM when the caller is
    // already a process-group leader, which we treat as benign.
    let rc = unsafe { setsid() };
    if rc == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
unsafe extern "C" {
    /// POSIX `setsid(2)`.
    fn setsid() -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn bind_as_lock_admits_exactly_one_winner_in_a_race() {
        // N threads race to bind the same socket; exactly one should win. `probe` always
        // reports "not alive" so a loser never reclaims — it must observe AddrInUse and fail
        // (the real daemon would back off; here we just count winners).
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("race.sock");
        let winners = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        const N: usize = 16;
        // Hold winning listeners so their sockets stay bound for the duration of the race.
        let listeners = Arc::new(std::sync::Mutex::new(Vec::new()));

        for _ in 0..N {
            let socket = socket.clone();
            let winners = winners.clone();
            let listeners = listeners.clone();
            handles.push(std::thread::spawn(move || {
                // A probe that always says "stale" would let losers reclaim and double-bind, so
                // we say "alive" once a socket file exists — modelling a live holder.
                let probe = |p: &std::path::Path| p.exists();
                // Each attempt needs its own tokio reactor to call `from_std`.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("rt");
                let result = rt.block_on(async { bind_listener(&socket, probe) });
                if let Ok(listener) = result {
                    winners.fetch_add(1, Ordering::SeqCst);
                    listeners.lock().expect("lock").push((listener, rt));
                }
            }));
        }
        for h in handles {
            h.join().expect("join");
        }
        assert_eq!(
            winners.load(Ordering::SeqCst),
            1,
            "exactly one binder may win the singleton lock"
        );
    }

    #[tokio::test]
    async fn ensure_daemon_noops_when_already_alive() {
        let paths = CommsPaths {
            comms_dir: PathBuf::from("/tmp/x"),
            socket_path: PathBuf::from("/tmp/x/comms.sock"),
        };
        let spawned = std::cell::Cell::new(false);
        let res = ensure_daemon_with(
            &paths,
            |_| true,
            |_| {
                spawned.set(true);
                Ok(())
            },
        )
        .await;
        assert!(res.is_ok());
        assert!(
            !spawned.get(),
            "must not spawn when a daemon already answers"
        );
    }

    #[tokio::test]
    async fn ensure_daemon_spawns_then_waits_for_ready() {
        let paths = CommsPaths {
            comms_dir: PathBuf::from("/tmp/x"),
            socket_path: PathBuf::from("/tmp/x/comms.sock"),
        };
        // Not alive until after spawn flips the flag.
        let alive = std::sync::atomic::AtomicBool::new(false);
        let res = ensure_daemon_with(
            &paths,
            |_| alive.load(std::sync::atomic::Ordering::SeqCst),
            |_| {
                alive.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )
        .await;
        assert!(res.is_ok(), "daemon became ready after spawn");
    }
}
