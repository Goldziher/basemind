//! Per-workspace daemon socket path derivation and singleton binding.
//!
//! One agent daemon hosts one repo, so the socket is keyed by workspace — it sits alongside that
//! repo's session log under the machine-global cache. Binding the socket *is* the singleton lock
//! (mirroring basemind's comms `singleton::bind_listener`): a second bind on a live socket is
//! rejected, while a stale socket left by a crashed daemon is reclaimed only after a liveness probe
//! confirms nobody is listening.

use std::path::{Path, PathBuf};

use tokio::net::UnixListener;

use crate::error::IpcError;

/// Subdirectory under the machine-global cache holding agent daemon sockets.
const AGENT_SUBDIR: &str = "agent";
/// Subdirectory (under `AGENT_SUBDIR`) that holds the per-workspace daemon sockets.
const DAEMON_SUBDIR: &str = "daemon";
/// Owner-only directory mode for the socket directory.
#[cfg(unix)]
const OWNER_ONLY_DIR: u32 = 0o700;
/// Owner-only file mode for the socket itself.
#[cfg(unix)]
const OWNER_ONLY_FILE: u32 = 0o600;

/// The per-workspace daemon socket path: `cache_root()/agent/daemon/<workspace_key>.sock`.
///
/// Reuses the same `workspace_key` as the session store, so the daemon socket lives next to
/// `agent/sessions/<workspace_key>/` and honors `BASEMIND_DATA_HOME`.
pub fn agent_socket_path(root: &Path) -> PathBuf {
    basemind::store_layout::cache_root()
        .join(AGENT_SUBDIR)
        .join(DAEMON_SUBDIR)
        .join(format!("{}.sock", basemind::store_layout::workspace_key(root)))
}

/// Best-effort synchronous liveness probe: a live daemon's listener accepts the connect (even while
/// busy, via the socket backlog); a stale socket file left by a dead daemon refuses it.
#[cfg(unix)]
pub fn probe_alive(socket_path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

/// Bind the daemon listener at `socket_path`, reclaiming a stale socket only after `probe` confirms
/// no live daemon answers. The bind itself is the singleton lock; a live socket yields
/// [`IpcError::AlreadyRunning`]. Must be called inside a tokio runtime (the listener registers with
/// the IO reactor).
///
/// `probe` is injected so tests can drive the live-vs-stale decision deterministically; production
/// callers pass [`probe_alive`].
#[cfg(unix)]
pub fn bind_listener(socket_path: &Path, probe: impl Fn(&Path) -> bool) -> Result<UnixListener, IpcError> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(OWNER_ONLY_DIR));
    }

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

#[cfg(all(test, unix))]
mod tests {
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

        let other_repo = tempfile::tempdir().expect("other repo");
        assert_ne!(first, agent_socket_path(other_repo.path()), "distinct repos differ");
        drop(guard);
    }

    #[tokio::test]
    async fn bind_creates_the_socket_and_a_second_live_bind_is_rejected() {
        let dir = tempfile::tempdir().expect("dir");
        let socket = dir.path().join("agent.sock");

        let _listener = bind_listener(&socket, probe_alive).expect("first bind");
        assert!(socket.exists(), "the socket file was created");
        assert!(probe_alive(&socket), "the live listener answers the probe");

        // The bind is the singleton lock: a second bind on the live socket is refused. ~keep
        match bind_listener(&socket, probe_alive) {
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
        let _listener = bind_listener(&socket, |_| false).expect("reclaim stale socket");
        assert!(probe_alive(&socket), "the reclaimed listener now answers");
    }
}
