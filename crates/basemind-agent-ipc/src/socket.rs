//! Per-workspace daemon socket path derivation and singleton binding.
//!
//! One agent daemon hosts one repo, so the socket is keyed by workspace — it sits alongside that
//! repo's session log under the machine-global cache. Binding the socket *is* the singleton lock
//! (mirroring basemind's comms `singleton::bind_listener`): a second bind on a live socket is
//! rejected, while a stale socket left by a crashed daemon is reclaimed only after a liveness probe
//! confirms nobody is listening.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::net::UnixListener;

use crate::error::IpcError;

/// Subdirectory under the machine-global cache holding agent daemon sockets.
const AGENT_SUBDIR: &str = "agent";
/// How many leading hex chars of the workspace key name the socket. The full 64-char key would push
/// the absolute socket path past the platform `sockaddr_un` limit (~104 bytes on macOS) even under a
/// normal home; a 16-hex (64-bit) prefix keeps it short while staying collision-free for the handful
/// of repos a user runs. It is a prefix of the session store's key, so the two stay associable.
const KEY_PREFIX_LEN: usize = 16;
/// Owner-only directory mode for the socket directory.
#[cfg(unix)]
const OWNER_ONLY_DIR: u32 = 0o700;
/// Owner-only file mode for the socket itself.
#[cfg(unix)]
const OWNER_ONLY_FILE: u32 = 0o600;
/// Maximum time a successor waits for a predecessor to finish socket cleanup.
const SOCKET_LOCK_WAIT: Duration = Duration::from_secs(15);
/// Poll cadence while a predecessor holds the socket cleanup lock without a live listener.
const SOCKET_LOCK_POLL: Duration = Duration::from_millis(25);
/// Number of connect attempts before an existing daemon socket is declared stale.
const PROBE_ATTEMPTS: usize = 4;
/// Backoff between liveness attempts so a briefly busy daemon is not reclaimed.
const PROBE_RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// Removes a daemon socket on drop only when the path still names the inode captured at creation.
/// This prevents a shutting-down daemon from unlinking a replacement socket bound by its successor.
#[derive(Debug)]
pub struct SocketCleanupGuard {
    _lock: std::fs::File,
    ownership: SocketOwnership,
}

/// Cloneable identity for checking that a daemon still owns its published socket path.
#[derive(Clone, Debug)]
pub struct SocketOwnership {
    path: PathBuf,
    device: u64,
    inode: u64,
    change_time_seconds: i64,
    change_time_nanoseconds: i64,
}

impl SocketOwnership {
    /// Return `true` while the path still names the socket inode captured at bind time.
    #[must_use]
    pub fn is_current(&self) -> bool {
        use std::os::unix::fs::MetadataExt;

        std::fs::metadata(&self.path)
            .map(|metadata| {
                metadata.dev() == self.device
                    && metadata.ino() == self.inode
                    && metadata.ctime() == self.change_time_seconds
                    && metadata.ctime_nsec() == self.change_time_nanoseconds
            })
            .unwrap_or(false)
    }
}

impl SocketCleanupGuard {
    /// Capture ownership of the socket currently at `path` for best-effort cleanup on drop.
    pub fn new(path: &Path) -> Result<Self, IpcError> {
        use std::os::unix::fs::MetadataExt;

        let lock = try_acquire_socket_lock(path)?;
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            _lock: lock,
            ownership: SocketOwnership {
                path: path.to_path_buf(),
                device: metadata.dev(),
                inode: metadata.ino(),
                change_time_seconds: metadata.ctime(),
                change_time_nanoseconds: metadata.ctime_nsec(),
            },
        })
    }

    /// Capture a cloneable token for watchdog checks without transferring cleanup ownership.
    #[must_use]
    pub fn ownership(&self) -> SocketOwnership {
        self.ownership.clone()
    }
}

impl Drop for SocketCleanupGuard {
    fn drop(&mut self) {
        if self.ownership.is_current()
            && let Err(error) = std::fs::remove_file(&self.ownership.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, socket = %self.ownership.path.display(), "agent ipc: socket cleanup failed");
        }
    }
}

/// The per-workspace daemon socket path: `cache_root()/agent/<workspace_key_prefix>.sock`.
///
/// The name is a prefix of the same `workspace_key` the session store uses, so the socket is
/// associable with `agent/sessions/<workspace_key>/` and honors `BASEMIND_DATA_HOME` — kept short to
/// stay within the platform Unix-socket path limit.
pub fn agent_socket_path(root: &Path) -> PathBuf {
    let key = basemind::store_layout::workspace_key(root);
    let short = &key[..key.len().min(KEY_PREFIX_LEN)];
    basemind::store_layout::cache_root()
        .join(AGENT_SUBDIR)
        .join(format!("{short}.sock"))
}

/// Best-effort synchronous liveness probe: a live daemon's listener accepts the connect (even while
/// busy, via the socket backlog); a stale socket file left by a dead daemon refuses it.
#[cfg(unix)]
pub fn probe_alive(socket_path: &Path) -> bool {
    probe_with_retries(
        || std::os::unix::net::UnixStream::connect(socket_path).is_ok(),
        PROBE_RETRY_BACKOFF,
    )
}

#[cfg(unix)]
fn probe_with_retries(mut probe_once: impl FnMut() -> bool, retry_backoff: Duration) -> bool {
    for attempt in 0..PROBE_ATTEMPTS {
        if probe_once() {
            return true;
        }
        if attempt + 1 < PROBE_ATTEMPTS && !retry_backoff.is_zero() {
            std::thread::sleep(retry_backoff);
        }
    }
    false
}

/// Bind the daemon listener at `socket_path`, reclaiming a stale socket only after `probe` confirms
/// no live daemon answers. The bind itself is the singleton lock; a live socket yields
/// [`IpcError::AlreadyRunning`]. Must be called inside a tokio runtime (the listener registers with
/// the IO reactor).
///
/// `probe` is injected so tests can drive the live-vs-stale decision deterministically; production
/// callers pass [`probe_alive`].
#[cfg(unix)]
pub async fn bind_listener(socket_path: &Path, probe: impl Fn(&Path) -> bool) -> Result<UnixListener, IpcError> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(OWNER_ONLY_DIR));
    }
    let _bind_lock = acquire_bind_lock(socket_path, &probe).await?;

    let adopt = |listener: std::os::unix::net::UnixListener| -> Result<UnixListener, IpcError> {
        listener.set_nonblocking(true)?;
        let _ = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(OWNER_ONLY_FILE));
        Ok(UnixListener::from_std(listener)?)
    };

    match std::os::unix::net::UnixListener::bind(socket_path) {
        Ok(listener) => adopt(listener),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            if probe(socket_path) {
                return Err(IpcError::AlreadyRunning(socket_path.to_path_buf()));
            }
            // The socket is stale (nobody listening); reclaim it and rebind. ~keep
            std::fs::remove_file(socket_path)?;
            adopt(std::os::unix::net::UnixListener::bind(socket_path)?)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn open_socket_lock(socket_path: &Path) -> Result<std::fs::File, IpcError> {
    use std::os::unix::fs::PermissionsExt;

    let mut lock_name = socket_path.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    let _ = lock.set_permissions(std::fs::Permissions::from_mode(OWNER_ONLY_FILE));
    Ok(lock)
}

#[cfg(unix)]
fn try_acquire_socket_lock(socket_path: &Path) -> Result<std::fs::File, IpcError> {
    let lock = open_socket_lock(socket_path)?;
    match fs2::FileExt::try_lock_exclusive(&lock) {
        Ok(()) => Ok(lock),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            Err(IpcError::AlreadyRunning(socket_path.to_path_buf()))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
async fn acquire_bind_lock(socket_path: &Path, probe: &impl Fn(&Path) -> bool) -> Result<std::fs::File, IpcError> {
    let deadline = Instant::now() + SOCKET_LOCK_WAIT;
    loop {
        match try_acquire_socket_lock(socket_path) {
            Ok(lock) => return Ok(lock),
            Err(IpcError::AlreadyRunning(_)) if probe(socket_path) => {
                return Err(IpcError::AlreadyRunning(socket_path.to_path_buf()));
            }
            Err(IpcError::AlreadyRunning(_)) if Instant::now() < deadline => {
                tokio::time::sleep(SOCKET_LOCK_POLL).await;
            }
            Err(IpcError::AlreadyRunning(_)) => {
                return Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("timed out waiting to reclaim agent socket {}", socket_path.display()),
                )));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// `BASEMIND_DATA_HOME` is process-global; serialize the env-mutating tests on a mutex.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        &LOCK
    }

    #[test]
    fn socket_path_is_stable_per_workspace_and_honors_data_home() {
        let guard = env_lock().lock().unwrap_or_else(|poison| poison.into_inner());
        let data_home = tempfile::tempdir().expect("data home");
        // SAFETY (test-only): serialized by `env_lock`, so no other test observes a torn value. ~keep
        unsafe { std::env::set_var("BASEMIND_DATA_HOME", data_home.path()) };

        let repo = tempfile::tempdir().expect("repo");
        let first = agent_socket_path(repo.path());
        let second = agent_socket_path(repo.path());
        assert_eq!(first, second, "same repo derives the same socket path");
        assert!(
            first.starts_with(data_home.path()),
            "socket lives under BASEMIND_DATA_HOME: {}",
            first.display()
        );
        assert_eq!(first.extension().and_then(|ext| ext.to_str()), Some("sock"));
        // Guard the SUN_LEN ceiling (~104 bytes on macOS): even under a deep tempdir data home the
        // shortened key must keep the absolute socket path bindable. ~keep
        assert!(
            first.as_os_str().len() < 104,
            "socket path must stay within the platform limit: {} ({} bytes)",
            first.display(),
            first.as_os_str().len()
        );

        let other_repo = tempfile::tempdir().expect("other repo");
        assert_ne!(first, agent_socket_path(other_repo.path()), "distinct repos differ");
        drop(guard);
    }

    #[tokio::test]
    async fn bind_creates_the_socket_and_a_second_live_bind_is_rejected() {
        let dir = tempfile::tempdir().expect("dir");
        let socket = dir.path().join("agent.sock");

        let _listener = bind_listener(&socket, probe_alive).await.expect("first bind");
        assert!(socket.exists(), "the socket file was created");
        assert!(probe_alive(&socket), "the live listener answers the probe");

        // The bind is the singleton lock: a second bind on the live socket is refused. ~keep
        match bind_listener(&socket, probe_alive).await {
            Err(IpcError::AlreadyRunning(path)) => assert_eq!(path, socket),
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_stale_socket_is_reclaimed_when_the_probe_reports_dead() {
        let dir = tempfile::tempdir().expect("dir");
        let socket = dir.path().join("agent.sock");

        // Leave a stale socket file behind: a std listener dropped without unlinking its path. ~keep
        let stale = std::os::unix::net::UnixListener::bind(&socket).expect("stale bind");
        drop(stale);
        assert!(socket.exists(), "the stale socket file remains after drop");

        // With a probe that reports the socket dead, the bind reclaims and rebinds. ~keep
        let _listener = bind_listener(&socket, |_| false).await.expect("reclaim stale socket");
        assert!(probe_alive(&socket), "the reclaimed listener now answers");
    }

    #[tokio::test]
    async fn cleanup_guard_unlinks_the_socket_it_owns() {
        let dir = tempfile::tempdir().expect("dir");
        let socket = dir.path().join("agent.sock");
        let listener = bind_listener(&socket, probe_alive).await.expect("bind listener");
        let guard = SocketCleanupGuard::new(&socket).expect("create cleanup guard");

        drop(listener);
        drop(guard);

        assert!(!socket.exists(), "owned socket is removed during cleanup");
    }

    #[tokio::test]
    async fn cleanup_guard_preserves_a_replacement_socket() {
        let dir = tempfile::tempdir().expect("dir");
        let socket = dir.path().join("agent.sock");
        let listener = bind_listener(&socket, probe_alive).await.expect("bind listener");
        let guard = SocketCleanupGuard::new(&socket).expect("create cleanup guard");
        let mut ownership = guard.ownership();
        assert!(ownership.is_current(), "guard initially owns the published socket");

        drop(listener);
        std::fs::remove_file(&socket).expect("remove original socket");
        let replacement = std::os::unix::net::UnixListener::bind(&socket).expect("bind replacement");
        use std::os::unix::fs::MetadataExt;
        let replacement_metadata = std::fs::metadata(&socket).expect("replacement metadata");
        // Reproduce Linux immediately reusing the removed socket's inode on filesystems where the
        // allocator happens to choose a different one during this test run.
        ownership.device = replacement_metadata.dev();
        ownership.inode = replacement_metadata.ino();
        assert!(!ownership.is_current(), "replacement invalidates captured ownership");
        drop(guard);

        assert!(socket.exists(), "cleanup must not unlink a replacement socket");
        drop(replacement);
    }

    #[tokio::test]
    async fn cleanup_guard_serializes_rebind_until_cleanup_finishes() {
        let dir = tempfile::tempdir().expect("dir");
        let socket = dir.path().join("agent.sock");
        let listener = bind_listener(&socket, probe_alive).await.expect("bind listener");
        let guard = SocketCleanupGuard::new(&socket).expect("create cleanup guard");
        drop(listener);

        let rebind_socket = socket.clone();
        let mut rebind = tokio::spawn(async move { bind_listener(&rebind_socket, |_| false).await });
        tokio::time::sleep(SOCKET_LOCK_POLL * 2).await;
        assert!(!rebind.is_finished(), "rebind waits while cleanup owns the lock");
        drop(guard);
        let _replacement = tokio::time::timeout(Duration::from_secs(2), &mut rebind)
            .await
            .expect("rebind completes after cleanup")
            .expect("rebind task joins")
            .expect("rebind succeeds after cleanup releases lock");
    }

    #[test]
    fn liveness_probe_retries_transient_connect_failures() {
        let attempts = AtomicUsize::new(0);

        let alive = probe_with_retries(
            || attempts.fetch_add(1, Ordering::SeqCst) + 1 == PROBE_ATTEMPTS,
            Duration::ZERO,
        );

        assert!(alive, "the final permitted attempt can establish liveness");
        assert_eq!(attempts.load(Ordering::SeqCst), PROBE_ATTEMPTS);
    }

    #[test]
    fn liveness_probe_stops_after_the_first_success() {
        let attempts = AtomicUsize::new(0);

        let alive = probe_with_retries(
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                true
            },
            Duration::ZERO,
        );

        assert!(alive);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
