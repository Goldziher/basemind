//! Embedded rmux-backed headless agent shells.
//!
//! This feature lets an agent spawn a real headless shell session, drive its
//! stdin, capture its output, and kill it — all over the rmux SDK, with basemind
//! itself acting as the embedded rmux daemon (see [`daemon`]). No external `rmux`
//! binary is required: [`ShellRuntime`] points the SDK's daemon-binary discovery
//! at `current_exe()`, so `connect_or_start` re-execs basemind with the hidden
//! `--__internal-daemon` flag, which `main` intercepts and routes to
//! [`daemon::run_internal_daemon`].
//!
//! The whole module is gated on `feature = "shells"`.

pub mod daemon;
pub mod session;

use std::path::PathBuf;
use std::process::id as process_id;
use std::sync::atomic::{AtomicU64, Ordering};

use ahash::AHashMap;
use anyhow::{Context, Result};
use rmux_sdk::{Rmux, RmuxBuilder, SessionName};
use tokio::sync::{Mutex, OnceCell};

use self::session::{ShellCommand, SpawnSpec};

/// Stable, opaque identifier minted by basemind for one spawned shell session.
///
/// Deterministic by construction: a monotonic per-process counter combined with
/// the process id, formatted as `bmsh-<pid>-<counter>`. No randomness and no
/// argless wall-clock read, so the value is reproducible within a process and
/// distinct across processes. The string is also a valid (sanitization-stable)
/// rmux [`SessionName`] — it contains no `:` or `.`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SessionId(String);

impl SessionId {
    /// Reconstruct an identifier from a string previously handed to a client.
    ///
    /// Used by the MCP layer to turn a caller-supplied `session_id` back into the
    /// map key. The value is opaque to clients, so any string is accepted here;
    /// validity is decided by lookup in the runtime's session map.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Monotonic counter feeding [`SessionId`]. Combined with the pid so two
/// processes never collide even though each starts the counter at zero.
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Mint the next deterministic [`SessionId`] for this process.
fn next_session_id() -> SessionId {
    let n = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    SessionId(format!("bmsh-{}-{}", process_id(), n))
}

/// Runtime handle for the embedded-shells feature.
///
/// Lazily connects to (or starts) the embedded rmux daemon on first use and
/// caches the [`Rmux`] handle. Holds the basemind-owned `session_id ->
/// SessionName` map so MCP tools address sessions by the stable [`SessionId`]
/// basemind minted rather than the raw rmux name.
pub struct ShellRuntime {
    /// Unix socket the embedded daemon binds. Owned by basemind; defaults to a
    /// per-uid path under the system temp dir, overridable for test isolation.
    socket_path: PathBuf,
    /// Lazily-initialized rmux handle. The first call to [`Self::rmux`] runs
    /// `connect_or_start`, which spawns the embedded daemon if absent.
    rmux: OnceCell<Rmux>,
    /// `session_id -> rmux session name`. Guarded by an async mutex because the
    /// MCP tools that mutate it are async.
    sessions: Mutex<AHashMap<SessionId, SessionName>>,
}

/// Environment override for the embedded-daemon socket path. When set to a
/// non-empty value, [`ShellRuntime::new`] binds the daemon there instead of the
/// default per-user temp path. Used by integration tests to sandbox each `serve`
/// instance on its own socket; not part of the documented public config.
pub const SHELLS_SOCKET_ENV: &str = "BASEMIND_SHELLS_SOCKET";

impl ShellRuntime {
    /// Construct a runtime that binds the embedded daemon at the default
    /// basemind-owned socket path (`<temp_dir>/basemind-shells-<user>.sock`),
    /// unless [`SHELLS_SOCKET_ENV`] overrides it.
    #[must_use]
    pub fn new() -> Self {
        let socket = std::env::var_os(SHELLS_SOCKET_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(default_socket_path);
        Self::with_socket_path(socket)
    }

    /// Construct a runtime bound to an explicit socket path. Used by tests to
    /// sandbox each run in its own temp directory.
    #[must_use]
    pub fn with_socket_path(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            rmux: OnceCell::new(),
            sessions: Mutex::new(AHashMap::new()),
        }
    }

    /// Resolve the rmux handle, connecting to (or starting) the embedded daemon
    /// on first call and caching it thereafter.
    ///
    /// The SDK's daemon binary is pointed at basemind's own executable once at
    /// startup (`daemon::intercept_from_env`, single-threaded), so
    /// `connect_or_start` re-execs basemind (not a missing `rmux`) as the daemon.
    /// The endpoint is the explicit basemind-owned socket, which bypasses the
    /// SDK's `Default`-endpoint allowlist.
    pub async fn rmux(&self) -> Result<&Rmux> {
        self.rmux
            .get_or_try_init(|| async {
                RmuxBuilder::new()
                    .unix_socket(self.socket_path.clone())
                    .connect_or_start()
                    .await
                    .context("connect to (or start) embedded rmux daemon")
            })
            .await
    }

    /// Spawn a detached headless session and register it under a fresh
    /// [`SessionId`]. Returns the minted id and the rmux session name.
    pub async fn spawn(
        &self,
        command: ShellCommand,
        working_directory: Option<String>,
        environment: Vec<String>,
    ) -> Result<(SessionId, SessionName)> {
        let rmux = self.rmux().await?;
        let session_id = next_session_id();
        let name = SessionName::new(session_id.as_str())
            .map_err(|e| anyhow::anyhow!("mint rmux session name: {e}"))?;
        let spec = SpawnSpec {
            name: name.clone(),
            command,
            working_directory,
            environment,
        };
        let _session = session::spawn_session(rmux, spec).await?;
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), name.clone());
        Ok((session_id, name))
    }

    /// Resolve the rmux [`SessionName`] previously registered for `id`.
    pub async fn resolve(&self, id: &SessionId) -> Option<SessionName> {
        self.sessions.lock().await.get(id).cloned()
    }

    /// Forget the mapping for `id` (after a successful kill).
    pub async fn forget(&self, id: &SessionId) {
        self.sessions.lock().await.remove(id);
    }
}

impl Default for ShellRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// Default basemind-owned socket path: `<temp_dir>/basemind-shells-<user>.sock`.
///
/// Namespaced by the current user so two users on a shared host get distinct
/// daemons; under the system temp dir so it is writable without extra setup.
fn default_socket_path() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("basemind-shells-{}.sock", user_namespace()));
    dir
}

/// Best-effort per-user namespace token for the socket path. Reads `USER` (unix)
/// / `USERNAME` (windows); falls back to the pid when neither is set so the path
/// is still unique-enough rather than panicking.
fn user_namespace() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| process_id().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_monotonic_and_pid_scoped() {
        let a = next_session_id();
        let b = next_session_id();
        assert_ne!(a, b);
        let pid_prefix = format!("bmsh-{}-", process_id());
        assert!(a.as_str().starts_with(&pid_prefix));
        assert!(b.as_str().starts_with(&pid_prefix));
    }

    #[test]
    fn session_id_is_a_valid_rmux_name_unchanged() {
        let id = next_session_id();
        let name = SessionName::new(id.as_str()).expect("valid name");
        // The id contains no `:` or `.`, so sanitization is a no-op.
        assert_eq!(name.as_str(), id.as_str());
    }
}
