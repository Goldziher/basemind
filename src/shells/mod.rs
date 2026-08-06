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

pub mod attach;
pub mod daemon;
pub mod launcher;
pub mod session;

/// Run any internal re-exec mode (visual attach, then embedded daemon) before clap parses.
///
/// Returns `Some(result)` if this process was an internal re-exec (it then exits),
/// else `None` and the caller proceeds with normal CLI parsing. The attach intercept
/// runs first because both re-execs are mutually exclusive (each carries a distinct
/// hidden first-argument flag) and the attach flag is the cheaper check.
#[must_use]
pub fn intercept_internal_reexec() -> Option<anyhow::Result<()>> {
    if let Some(result) = attach::intercept_from_env() {
        return Some(result);
    }
    daemon::intercept_from_env()
}

use std::path::PathBuf;
use std::process::id as process_id;
use std::sync::atomic::{AtomicU64, Ordering};

use ahash::AHashMap;
use anyhow::{Context, Result};
use rmux_sdk::{Rmux, RmuxBuilder, SessionName};
use tokio::sync::OnceCell;

use self::session::{ShellCommand, SpawnSpec};
use crate::daemon_lock::{DaemonKind, count_live_daemons_of, max_live_daemons};

const EXISTING_DAEMON_CONNECT_ATTEMPTS: usize = 4;
const EXISTING_DAEMON_CONNECT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

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

/// One entry in a [`ShellRuntime::list`] snapshot of the daemon's live sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSessionInfo {
    /// The basemind-minted [`SessionId`] this runtime handed to the client.
    pub session_id: SessionId,
    /// The underlying rmux session name.
    pub name: SessionName,
    /// Always `true`: dead sessions are absent from the daemon-backed snapshot.
    pub alive: bool,
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
/// caches the [`Rmux`] handle. Session resolution always queries the shared rmux
/// daemon, so separate CLI and MCP processes observe the same live sessions.
pub struct ShellRuntime {
    /// Unix socket the embedded daemon binds. Owned by basemind; defaults to a
    /// per-user private path under the data dir, overridable for test isolation.
    socket_path: PathBuf,
    /// Lazily-initialized rmux handle. The first call to [`Self::rmux`] runs
    /// `connect_or_start`, which spawns the embedded daemon if absent.
    rmux: OnceCell<Rmux>,
}

/// Environment override for the embedded-daemon socket path. When set to a
/// non-empty value, [`ShellRuntime::new`] binds the daemon there instead of the
/// default per-user temp path. Used by integration tests to sandbox each `serve`
/// instance on its own socket; not part of the documented public config.
pub const SHELLS_SOCKET_ENV: &str = "BASEMIND_SHELLS_SOCKET";

impl ShellRuntime {
    /// Construct a runtime that binds the embedded daemon at the default
    /// basemind-owned socket path (`<data_dir>/shells/rmux.sock` under the
    /// per-user data dir), unless [`SHELLS_SOCKET_ENV`] overrides it.
    #[must_use]
    pub fn new() -> Self {
        let socket = std::env::var_os(SHELLS_SOCKET_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| match daemon::validate_socket_path(path) {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        path = %path.display(),
                        "BASEMIND_SHELLS_SOCKET rejected; falling back to the default socket path"
                    );
                    false
                }
            })
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
        }
    }

    /// Borrow the Unix socket path this runtime's embedded daemon binds.
    ///
    /// Exposed so the MCP spawn path can build the visual attach command (which
    /// re-execs basemind with `--socket <this path>`) against the same socket the
    /// daemon is bound to.
    #[must_use]
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    /// Resolve the rmux handle, connecting to (or starting) the embedded daemon
    /// on first call and caching it thereafter.
    ///
    /// The SDK's daemon binary is pointed at basemind's own executable once at
    /// startup (`daemon::intercept_from_env`, single-threaded), so
    /// `connect_or_start` re-execs basemind (not a missing `rmux`) as the daemon.
    /// Four connect-only attempts absorb a daemon's startup window. Only after
    /// those fail does this enforce the Shells daemon ceiling immediately before
    /// the spawning `connect_or_start` path. The endpoint is the explicit
    /// basemind-owned socket, which bypasses the SDK's `Default`-endpoint allowlist.
    pub async fn rmux(&self) -> Result<&Rmux> {
        self.rmux
            .get_or_try_init(|| async {
                let mut last_connect_error = None;
                for attempt in 1..=EXISTING_DAEMON_CONNECT_ATTEMPTS {
                    match self.rmux_builder().connect().await {
                        Ok(rmux) => return Ok(rmux),
                        Err(error) => last_connect_error = Some(error),
                    }
                    if attempt < EXISTING_DAEMON_CONNECT_ATTEMPTS {
                        tokio::time::sleep(EXISTING_DAEMON_CONNECT_RETRY_DELAY).await;
                    }
                }

                let connect_error =
                    last_connect_error.context("embedded rmux connection attempts produced no result")?;
                let live_daemons = tokio::task::spawn_blocking(|| count_live_daemons_of(DaemonKind::Shells))
                    .await
                    .context("join embedded rmux daemon registry count task")?;
                enforce_shell_daemon_ceiling(live_daemons, max_live_daemons())?;

                self.rmux_builder().connect_or_start().await.with_context(|| {
                    format!(
                        "connect to (or start) embedded rmux daemon after connect-only attempts failed: {connect_error}"
                    )
                })
            })
            .await
    }

    /// Build the rmux SDK builder pointed at this runtime's basemind-owned
    /// endpoint.
    ///
    /// On Unix the endpoint is a filesystem socket path; on Windows it is a
    /// named-pipe path (`\\.\pipe\basemind-shells-<user>`). The rmux SDK refuses a
    /// `UnixSocket` endpoint on Windows builds (and vice versa), so the endpoint
    /// kind MUST match the target OS — hence the `cfg` split rather than always
    /// calling `unix_socket`.
    fn rmux_builder(&self) -> RmuxBuilder {
        #[cfg(windows)]
        {
            RmuxBuilder::new().windows_pipe(self.socket_path.to_string_lossy().into_owned())
        }
        #[cfg(not(windows))]
        {
            RmuxBuilder::new().unix_socket(self.socket_path.clone())
        }
    }

    /// Mint the next deterministic [`SessionId`] for this runtime's process.
    ///
    /// Exposed so the MCP layer can mint ONE id up front and thread it through
    /// both the comms coupling (which keys the session-scoped room) and
    /// [`Self::spawn`] (which addresses the rmux session). Keeping a single id
    /// means the room the child auto-joins and the id the client addresses are
    /// provably the same value, not two counters that happen to stay in step.
    #[must_use]
    pub fn mint_session_id(&self) -> SessionId {
        next_session_id()
    }

    /// Spawn a detached headless session under the pre-minted [`SessionId`].
    /// Returns the id (echoed back) and the byte-identical rmux session name.
    ///
    /// The id is minted by the caller (via [`Self::mint_session_id`]) rather than
    /// here, so the comms-coupling env built before the spawn and the rmux session
    /// addressed after it share one identifier.
    pub async fn spawn(
        &self,
        session_id: SessionId,
        command: ShellCommand,
        working_directory: Option<String>,
        environment: Vec<String>,
        cols: u16,
        rows: u16,
    ) -> Result<(SessionId, SessionName)> {
        let rmux = self.rmux().await?;
        let name = SessionName::new(session_id.as_str()).map_err(|e| anyhow::anyhow!("mint rmux session name: {e}"))?;
        let spec = SpawnSpec {
            name: name.clone(),
            command,
            working_directory,
            environment,
            cols,
            rows,
        };
        let _session = session::spawn_session(rmux, spec).await?;
        Ok((session_id, name))
    }

    /// Resolve `id` against the shared daemon's live session set.
    ///
    /// Daemon connection and listing failures remain errors; only a successful
    /// listing that lacks the byte-identical session name returns `Ok(None)`.
    pub async fn resolve(&self, id: &SessionId) -> Result<Option<SessionName>> {
        let rmux = self.rmux().await?;
        let live = session::list_sessions(rmux).await?;
        Ok(live.into_iter().find(|name| name.as_str() == id.as_str()))
    }

    /// Broadcast `text` to the primary pane of every session in `ids` at once.
    ///
    /// Each id is resolved against one snapshot of the daemon's live sessions;
    /// an unknown id is an error (the broadcast is not attempted). When `enter`
    /// is true a trailing newline is appended so each shell executes the line.
    /// Returns the number of panes that accepted the input.
    pub async fn broadcast(&self, ids: &[SessionId], text: &str, enter: bool) -> Result<usize> {
        let rmux = self.rmux().await?;
        let live = session::list_sessions(rmux).await?;
        let live_by_id: AHashMap<&str, &SessionName> = live.iter().map(|name| (name.as_str(), name)).collect();
        let names = ids
            .iter()
            .map(|id| {
                live_by_id
                    .get(id.as_str())
                    .copied()
                    .cloned()
                    .with_context(|| format!("unknown session_id {id}"))
            })
            .collect::<Result<Vec<_>>>()?;
        session::broadcast(rmux, &names, text, enter).await
    }

    /// Snapshot every session currently reported by the shared daemon.
    pub async fn list(&self) -> Result<Vec<ShellSessionInfo>> {
        let rmux = self.rmux().await?;
        let mut live = session::list_sessions(rmux).await?;
        live.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        Ok(live
            .into_iter()
            .map(|name| ShellSessionInfo {
                session_id: SessionId::new(name.as_str()),
                name,
                alive: true,
            })
            .collect())
    }
}

impl Default for ShellRuntime {
    fn default() -> Self {
        Self::new()
    }
}

fn enforce_shell_daemon_ceiling(live_daemons: usize, maximum_daemons: usize) -> Result<()> {
    if live_daemons < maximum_daemons {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to start an embedded rmux daemon: {live_daemons} live shells daemons meets the limit of \
         {maximum_daemons}; override with BASEMIND_MAX_DAEMONS"
    )
}

/// Subdirectory under the per-user data dir that holds the shells daemon socket.
/// Unix only — on Windows the endpoint is a named pipe with no parent dir.
#[cfg(not(windows))]
const SHELLS_SUBDIR: &str = "shells";
/// The Unix socket file name within [`SHELLS_SUBDIR`]. Unix only.
#[cfg(not(windows))]
const SHELLS_SOCKET_FILE: &str = "rmux.sock";
/// Owner-only directory mode for the private shells data dir (mirrors comms).
#[cfg(unix)]
const OWNER_ONLY_DIR: u32 = 0o700;

/// Default basemind-owned socket path under the per-user PRIVATE data dir:
/// `<data_dir>/shells/rmux.sock` (Unix) or a per-user named pipe
/// `\\.\pipe\basemind-shells-<user>` (Windows).
///
/// On Unix this mirrors the comms daemon's socket placement
/// (`directories::ProjectDirs`, an owner-only `0o700` parent dir) so the control
/// socket never lands in world-writable `/tmp`, where another local user could
/// pre-create it. It falls back to a per-user-namespaced path under the system
/// temp dir only when `ProjectDirs` cannot resolve a data dir (no `HOME` etc.).
///
/// On Windows there is no filesystem socket: rmux binds a named pipe, whose name
/// is the path verbatim. The pipe is scoped to the user via the sanitized
/// username (mirrors `comms_socket_path`'s Windows branch), and Windows named
/// pipes carry their own ACLs rather than a parent-dir mode, so no directory is
/// created or tightened.
fn default_socket_path() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\basemind-shells-{}", user_namespace()))
    }
    #[cfg(not(windows))]
    {
        if let Some(path) = project_dirs_socket_path() {
            return path;
        }
        let mut dir = std::env::temp_dir();
        dir.push(format!("basemind-shells-{}.sock", user_namespace()));
        dir
    }
}

/// Resolve the private `<data_dir>/shells/rmux.sock` path, creating the parent
/// dir with owner-only mode first. `None` when `ProjectDirs` cannot resolve a
/// data dir (so the caller falls back to the temp dir). Unix only — Windows uses
/// a named pipe with no parent dir.
#[cfg(not(windows))]
fn project_dirs_socket_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "basemind")?;
    let shells_dir = dirs.data_dir().join(SHELLS_SUBDIR);
    if std::fs::create_dir_all(&shells_dir).is_err() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&shells_dir, std::fs::Permissions::from_mode(OWNER_ONLY_DIR));
    }
    Some(shells_dir.join(SHELLS_SOCKET_FILE))
}

/// Best-effort per-user namespace token for the temp-dir fallback socket path.
/// Reads `USER` (unix) / `USERNAME` (windows); falls back to the pid when neither
/// is set so the path is still unique-enough rather than panicking.
///
/// The token is sanitized to `[A-Za-z0-9_-]` (every other byte replaced with
/// `_`) so a hostile `USER` value can never inject a newline / `;` / `$` / quote
/// / NUL into the socket path. Belt-and-suspenders with the private-dir placement
/// in [`default_socket_path`].
fn user_namespace() -> String {
    let raw = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| process_id().to_string());
    sanitize_namespace(&raw)
}

/// Replace every byte outside `[A-Za-z0-9_-]` with `_`. Keeps the token a safe
/// path component. An all-invalid input collapses to underscores, which is still
/// a valid (if uninformative) path component.
fn sanitize_namespace(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_daemon_ceiling_allows_capacity_below_the_limit() {
        assert!(enforce_shell_daemon_ceiling(7, 8).is_ok());
    }

    #[test]
    fn shell_daemon_ceiling_rejects_capacity_at_the_limit() {
        let error = enforce_shell_daemon_ceiling(8, 8).expect_err("the ceiling must reject another daemon");
        assert!(error.to_string().contains("8 live shells daemons"), "{error}");
        assert!(error.to_string().contains("BASEMIND_MAX_DAEMONS"), "{error}");
    }

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
        assert_eq!(name.as_str(), id.as_str());
    }

    #[test]
    fn sanitize_namespace_strips_shell_metacharacters() {
        assert_eq!(sanitize_namespace("alice"), "alice");
        assert_eq!(sanitize_namespace("a-b_c"), "a-b_c");
        assert_eq!(sanitize_namespace("a;b\nc"), "a_b_c");
        assert_eq!(sanitize_namespace("evil$(whoami)"), "evil__whoami_");
        assert_eq!(sanitize_namespace("x\0y\"z"), "x_y_z");
    }

    #[test]
    fn mint_session_id_yields_distinct_ids() {
        let runtime = ShellRuntime::with_socket_path(PathBuf::from("/tmp/unused.sock"));
        let a = runtime.mint_session_id();
        let b = runtime.mint_session_id();
        assert_ne!(a, b, "each mint yields a fresh id");
    }
}
