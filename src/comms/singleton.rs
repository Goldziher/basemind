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

use super::protocol::{CommsRequest, CommsResponse, PROTO_VER, StatusReport};

/// Subdirectory under the user data dir holding the comms socket + store.
const COMMS_SUBDIR: &str = "comms";
/// The Unix socket file name within [`COMMS_SUBDIR`]. Unused on Windows, where the endpoint is
/// a named pipe resolved by [`comms_socket_path`] rather than a file under `comms_dir`.
#[cfg(not(windows))]
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
/// How long to wait for a previous / incompatible daemon to release the socket after we ask it to
/// stop, before giving up and surfacing a clear error.
const TAKEOVER_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

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
    /// A previous / incompatible daemon held the socket and would not stop, so we could not take
    /// over. Surfaced instead of silently talking to an incompatible daemon (which is how the
    /// pre-0.10 version-skew bug manifested as an opaque "connection closed").
    #[error(
        "a previous basemind comms daemon (v{version}, pid {pid}) is still running and did not \
         stop; run `basemind comms stop` or terminate pid {pid}, then retry"
    )]
    StalePredecessor {
        /// The stale daemon's build version.
        version: String,
        /// The stale daemon's process id.
        pid: u32,
    },
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
    Ok(CommsPaths { comms_dir, socket_path })
}

/// The socket path for a resolved comms dir. On Windows, a named-pipe path (see the TODO in
/// `frontend_uds`); on Unix, `<comms_dir>/comms.sock`.
pub fn comms_socket_path(comms_dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::hash::{Hash, Hasher};
        // Per-user named pipe, isolated by comms_dir. The username keeps it user-scoped on shared
        // hosts; the comms_dir hash mirrors the per-dir Unix socket so distinct BASEMIND_COMMS_DIR
        // values (test tempdirs, sandboxes) resolve to distinct pipes instead of colliding on one
        // per-user singleton. Production leaves BASEMIND_COMMS_DIR unset, so comms_dir is the single
        // constant ProjectDirs path and the daemon stays one stable per-user broker. DefaultHasher
        // has a fixed (non-random) seed, so the daemon and client processes derive the same name
        // from the same comms_dir. Without this, parallel comms dirs cross-contaminate (see #110).
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_string());
        let mut hasher = std::hash::DefaultHasher::new();
        comms_dir.hash(&mut hasher);
        let dir_hash = hasher.finish();
        PathBuf::from(format!(r"\\.\pipe\basemind-comms-{user}-{dir_hash:016x}"))
    }
    #[cfg(not(windows))]
    {
        comms_dir.join(SOCKET_FILE)
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), SingletonError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|source| SingletonError::Io {
        path: path.to_path_buf(),
        source,
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
            let _ = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(OWNER_ONLY_FILE));
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
                std::os::unix::net::UnixListener::bind(socket_path).map_err(|source| SingletonError::Io {
                    path: socket_path.to_path_buf(),
                    source,
                })?;
            std_listener
                .set_nonblocking(true)
                .map_err(|source| SingletonError::Io {
                    path: socket_path.to_path_buf(),
                    source,
                })?;
            let _ = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(OWNER_ONLY_FILE));
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

/// Bind the singleton named-pipe server on Windows, reclaiming a stale name only after a probe
/// confirms no live daemon answers. Returns the first pipe instance (creating it with
/// `first_pipe_instance(true)` IS the lock: a second `create` with that flag fails while the
/// first instance lives).
///
/// `probe` is invoked on the existing pipe path to decide live-vs-stale, mirroring the Unix
/// contract. Must be called inside a tokio runtime — `ServerOptions::create` registers the pipe
/// handle with the I/O reactor (like the Unix `from_std`).
#[cfg(windows)]
pub fn bind_listener(
    socket_path: &Path,
    probe: impl Fn(&Path) -> bool,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, SingletonError> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let pipe_name = socket_path.as_os_str();
    let io_err = |source: std::io::Error| SingletonError::Io {
        path: socket_path.to_path_buf(),
        source,
    };

    // First attempt: claiming the first instance wins the lock outright.
    match ServerOptions::new().first_pipe_instance(true).create(pipe_name) {
        Ok(server) => Ok(server),
        // ERROR_ACCESS_DENIED: another daemon already holds the first instance.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            if probe(socket_path) {
                return Err(SingletonError::AlreadyRunning(socket_path.to_path_buf()));
            }
            // The holder is dead but the pipe lingers in the brief window before its handles
            // drop; retry the create once. (No explicit unlink: named pipes vanish with their
            // owner, so there is no stale file to remove.)
            ServerOptions::new()
                .first_pipe_instance(true)
                .create(pipe_name)
                .map_err(io_err)
        }
        Err(source) => Err(io_err(source)),
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

/// Ensure a healthy, current daemon is running, taking over from a previous one on the way.
///
/// On load this reaps the kind of stale process that used to pile up: if the socket is held by a
/// daemon from an OLDER build (or one speaking a different protocol version), we ask it to stop and
/// spawn a fresh daemon in its place — converging the singleton on the newest binary. A daemon at
/// our version (or newer) is reused as-is, so concurrent same-version sessions still share one
/// broker. If the predecessor will not yield the socket, we error out clearly
/// ([`SingletonError::StalePredecessor`]) rather than silently talking to an incompatible daemon.
pub async fn ensure_daemon(paths: &CommsPaths) -> Result<(), SingletonError> {
    if let Some(report) = daemon_status(&paths.socket_path) {
        let ours = env!("CARGO_PKG_VERSION");
        let compatible = report.proto_ver == PROTO_VER && !version_is_older(&report.version, ours);
        if compatible {
            return Ok(()); // healthy current (or newer) daemon — reuse it
        }
        tracing::warn!(
            daemon_version = %report.version,
            daemon_pid = report.pid,
            ours,
            "comms: a previous/incompatible daemon holds the socket; taking over"
        );
        request_stop(&paths.socket_path);
        let deadline = std::time::Instant::now() + TAKEOVER_DRAIN_TIMEOUT;
        while std::time::Instant::now() < deadline {
            if !probe_alive(&paths.socket_path) {
                break;
            }
            tokio::time::sleep(SPAWN_POLL_INTERVAL).await;
        }
        if probe_alive(&paths.socket_path) {
            return Err(SingletonError::StalePredecessor {
                version: report.version,
                pid: report.pid,
            });
        }
    }
    ensure_daemon_with(paths, probe_alive, spawn_detached_daemon).await
}

/// True when `daemon`'s `MAJOR.MINOR.PATCH` is strictly older than `ours`. Pre-release suffixes
/// (`-rc.N`) are ignored — close enough for the "is this a previous build?" takeover decision.
fn version_is_older(daemon: &str, ours: &str) -> bool {
    fn triple(v: &str) -> (u64, u64, u64) {
        let core = v.split('-').next().unwrap_or(v);
        let mut it = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
        (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
    }
    triple(daemon) < triple(ours)
}

/// One-shot `Status` request against a live daemon — returns its [`StatusReport`] (pid / version /
/// proto) or `None` if nothing answers. Synchronous, mirroring [`probe_alive`]'s framing.
fn daemon_status(socket_path: &Path) -> Option<StatusReport> {
    match roundtrip(socket_path, &CommsRequest::Status)? {
        CommsResponse::Status(report) => Some(report),
        _ => None,
    }
}

/// Best-effort `Stop` request asking a daemon to drain and exit. Errors are ignored — the caller
/// polls the socket to confirm the daemon actually went away.
fn request_stop(socket_path: &Path) {
    let _ = roundtrip(socket_path, &CommsRequest::Stop);
}

/// Connect to the daemon endpoint, send one length-delimited msgpack request, and decode the one
/// framed [`CommsResponse`]. `None` on any transport/codec failure. Bounds the response to 64 KiB.
fn roundtrip(socket_path: &Path, req: &CommsRequest) -> Option<CommsResponse> {
    use std::io::{Read, Write};
    let mut stream = open_endpoint(socket_path)?;
    let body = rmp_serde::to_vec_named(req).ok()?;
    let len = u32::try_from(body.len()).ok()?;
    stream.write_all(&len.to_be_bytes()).ok()?;
    stream.write_all(&body).ok()?;
    let mut prefix = [0u8; 4];
    stream.read_exact(&mut prefix).ok()?;
    let rlen = u32::from_be_bytes(prefix) as usize;
    if rlen > 64 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; rlen];
    stream.read_exact(&mut buf).ok()?;
    rmp_serde::from_slice::<CommsResponse>(&buf).ok()
}

/// Open the platform endpoint (Unix socket / Windows named pipe) with short read/write timeouts.
#[cfg(unix)]
fn open_endpoint(socket_path: &Path) -> Option<impl std::io::Read + std::io::Write> {
    let stream = std::os::unix::net::UnixStream::connect(socket_path).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(800)));
    Some(stream)
}

#[cfg(windows)]
fn open_endpoint(socket_path: &Path) -> Option<impl std::io::Read + std::io::Write> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(socket_path)
        .ok()
}

/// How many times [`probe_alive`] pings before declaring a daemon dead, and the backoff between
/// attempts. A live-but-busy daemon can miss a single ping (its accept loop is mid-request); a
/// false "dead" verdict in [`bind_listener`] would unlink its socket and orphan it. Retrying a
/// few times makes reclaim conservative, which is the dominant guard against daemon pile-up.
const PROBE_ATTEMPTS: u32 = 4;
const PROBE_RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// Probe whether a daemon is alive at `socket_path` by connecting and pinging it. Synchronous
/// (uses a blocking connect with a short timeout) so it can be used as the `probe` in
/// [`bind_listener`] too. Retries a few times before giving up so a momentarily-busy daemon is
/// not falsely reclaimed.
#[cfg(any(unix, windows))]
pub fn probe_alive(socket_path: &Path) -> bool {
    for attempt in 0..PROBE_ATTEMPTS {
        if probe_once(socket_path) {
            return true;
        }
        if attempt + 1 < PROBE_ATTEMPTS {
            std::thread::sleep(PROBE_RETRY_BACKOFF);
        }
    }
    false
}

/// One connect+ping attempt against the Unix socket. See [`probe_alive`] for the retrying wrapper.
#[cfg(unix)]
fn probe_once(socket_path: &Path) -> bool {
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

/// One connect+ping attempt against the Windows named pipe. See [`probe_alive`] for the retrying
/// wrapper. A `\\.\pipe\...` path opens as an ordinary [`std::fs::File`], so we open it blocking
/// and write the SAME framed `Ping` the Unix probe sends (u32-be length prefix + msgpack), then
/// read a 4-byte response prefix. Any successful read ⇒ alive. A transient busy/not-found at open
/// time ⇒ not alive (the caller treats that as "reclaimable").
#[cfg(windows)]
fn probe_once(socket_path: &Path) -> bool {
    use std::io::{Read, Write};

    let Ok(mut stream) = std::fs::OpenOptions::new().read(true).write(true).open(socket_path) else {
        return false;
    };

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

#[cfg(not(any(unix, windows)))]
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
    // Detach the daemon from the spawning console on Windows: a new process group + no console
    // window so a parent exit (or Ctrl-C in the parent's console) does not take the daemon down.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// `CreateProcess` flag: the child has no controlling console.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        /// `CreateProcess` flag: the child starts a new process group (Ctrl-C/Break isolation).
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        /// `CreateProcess` flag: do not allocate a console window for the child.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        // Stop the long-lived daemon from inheriting our standard handles. `CreateProcess` is
        // called with `bInheritHandles = TRUE` whenever stdio is redirected (it is, to NUL above),
        // and that inherits EVERY inheritable handle in this process — including the stdout/stderr
        // pipe a parent gave us when it captured our output (e.g. `Command::output()`, or `serve`'s
        // MCP stdio). The detached daemon would then hold the write end open forever, so the
        // capturing parent never sees EOF and blocks until the daemon dies — the Windows-only
        // `comms start` hang. Unix avoids this because `Stdio::null()` dup2's /dev/null over the
        // child fds and Rust sets CLOEXEC on its own pipes. Clearing the inherit bit on our std
        // handles first makes the detached spawn leak none of them.
        clear_std_handle_inheritance();
    }
    command.spawn()?;
    Ok(())
}

/// Clear `HANDLE_FLAG_INHERIT` on this process's standard input/output/error handles so a
/// subsequently spawned child does not inherit them. See the call site in [`spawn_detached_daemon`]
/// for why the detached daemon must not inherit a captured stdout/stderr pipe. Clearing the inherit
/// bit does not affect this process's own use of the handles — only whether children receive a
/// duplicate — so it is safe for the short-lived `comms start` CLI and for `serve` (whose later
/// child spawns pass their stdio explicitly rather than relying on inheritance).
#[cfg(windows)]
fn clear_std_handle_inheritance() {
    /// `GetStdHandle` selectors (Win32 `STD_*_HANDLE`, defined as negative `DWORD`s).
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    /// `SetHandleInformation` mask bit controlling handle inheritance.
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    /// `GetStdHandle` failure sentinel.
    const INVALID_HANDLE_VALUE: isize = -1;

    for selector in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: both calls take only primitive arguments and read no caller memory.
        // `GetStdHandle` returns the process's current standard handle (or null /
        // `INVALID_HANDLE_VALUE`, which we skip); `SetHandleInformation` only clears the inherit
        // bit on that handle. Neither touches our heap or thread state, so the calls are safe.
        unsafe {
            let handle = GetStdHandle(selector);
            if handle != 0 && handle != INVALID_HANDLE_VALUE {
                let _ = SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    /// Win32 `GetStdHandle` — returns the handle for a standard device (`STD_*_HANDLE`).
    fn GetStdHandle(nstdhandle: u32) -> isize;
    /// Win32 `SetHandleInformation` — sets the masked flag bits on a handle. Returns nonzero on
    /// success.
    fn SetHandleInformation(hobject: isize, dwmask: u32, dwflags: u32) -> i32;
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

    #[test]
    fn version_is_older_orders_releases_and_ignores_prerelease() {
        // A strictly older build is a takeover candidate (this is the 0.6.3-vs-0.10.0 squatter).
        assert!(version_is_older("0.6.3", "0.10.0"));
        assert!(version_is_older("0.9.0", "0.10.0"));
        assert!(version_is_older("0.10.0", "0.10.1"));
        // Same version or newer is reused, never replaced (no flapping between same-version peers).
        assert!(!version_is_older("0.10.0", "0.10.0"));
        assert!(!version_is_older("0.11.0", "0.10.0"));
        assert!(!version_is_older("1.0.0", "0.10.0"));
        // Pre-release suffixes are ignored for the ordering decision.
        assert!(!version_is_older("0.10.0-rc.1", "0.10.0"));
        assert!(version_is_older("0.9.0-rc.2", "0.10.0"));
    }

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
        assert!(!spawned.get(), "must not spawn when a daemon already answers");
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
