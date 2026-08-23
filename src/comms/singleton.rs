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

use super::protocol::{CommsOut, CommsRequest, CommsResponse, PROTO_VER, StatusReport};

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

/// Longest bindable Unix-socket path, from `sockaddr_un.sun_path` minus its NUL terminator: 108
/// bytes on Linux, 104 on macOS and the BSDs. A path over this can never be bound by anyone, so
/// it is worth failing on before spawning rather than after a timeout.
#[cfg(unix)]
const SUN_PATH_MAX: usize = if cfg!(target_os = "linux") { 107 } else { 103 };

/// How long to wait for a spawned daemon to become reachable before giving up. Full-feature
/// binaries can take materially longer to page in when every core and memory channel is busy.
const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(30);
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
    /// The socket path exceeds what `sockaddr_un` can hold, so no daemon can ever bind it.
    /// Reported before spawning: waiting out the readiness timeout would blame a hang for what is
    /// really a path-length limit, which is how this first surfaced.
    #[cfg(unix)]
    #[error(
        "the comms socket path is {len} bytes, past the {limit}-byte limit a Unix socket address \
         can hold, so no daemon can bind it: {path}. Point BASEMIND_COMMS_DIR at a shorter \
         directory."
    )]
    SocketPathTooLong {
        /// The over-long socket path.
        path: PathBuf,
        /// Its length in bytes.
        len: usize,
        /// The platform's limit.
        limit: usize,
    },
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
    /// The machine already runs at (or above) the live-daemon ceiling, so spawning another is
    /// refused rather than risking a runaway pile-up. Surfaced instead of silently forking an
    /// N+1-th daemon — the leak class this guards against exhausted the process table.
    #[error(
        "refusing to spawn another basemind comms daemon: {count} are already live (ceiling {max}); \
         run `basemind comms doctor` to inspect and `basemind comms stop --all` to reclaim, or raise \
         BASEMIND_MAX_DAEMONS"
    )]
    TooManyDaemons {
        /// Live daemons counted in the machine registry.
        count: usize,
        /// The effective ceiling.
        max: usize,
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

/// The socket path for a resolved comms dir. On Windows, a named-pipe path; on Unix,
/// `<comms_dir>/comms.sock`.
pub fn comms_socket_path(comms_dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::hash::{Hash, Hasher};
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
            if probe(socket_path) {
                return Err(SingletonError::AlreadyRunning(socket_path.to_path_buf()));
            }
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

    match ServerOptions::new().first_pipe_instance(true).create(pipe_name) {
        Ok(server) => Ok(server),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            if probe(socket_path) {
                return Err(SingletonError::AlreadyRunning(socket_path.to_path_buf()));
            }
            ServerOptions::new()
                .first_pipe_instance(true)
                .create(pipe_name)
                .map_err(io_err)
        }
        Err(source) => Err(io_err(source)),
    }
}

/// Reject a socket path the kernel could never bind, before anything tries.
///
/// A `sockaddr_un` holds a fixed-size `sun_path`, so an over-long path fails at `bind` inside the
/// spawned daemon — where the caller cannot see it. The caller then waits out the full readiness
/// timeout and reports a hang, sending whoever hit it looking for a stuck process instead of a
/// directory name. Checking up front turns that into one accurate sentence.
///
/// A no-op on Windows, whose named pipes have no equivalent ceiling.
#[cfg(unix)]
fn check_socket_path_len(socket_path: &Path) -> Result<(), SingletonError> {
    let len = socket_path.as_os_str().len();
    if len > SUN_PATH_MAX {
        return Err(SingletonError::SocketPathTooLong {
            path: socket_path.to_path_buf(),
            len,
            limit: SUN_PATH_MAX,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_socket_path_len(_socket_path: &Path) -> Result<(), SingletonError> {
    Ok(())
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
    check_socket_path_len(&paths.socket_path)?;
    spawn(paths).map_err(|source| SingletonError::Io {
        path: paths.socket_path.clone(),
        source,
    })?;
    let deadline = std::time::Instant::now() + SPAWN_READY_TIMEOUT;
    loop {
        if is_alive(&paths.socket_path) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            break;
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
            return Ok(());
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
    // No compatible daemon answered — we are about to spawn one. Enforce the machine ceiling for the
    // comms family first so a runaway (each isolated comms dir spawning its own detached daemon) is
    // refused loudly, not silently piled up. Counting per kind keeps another family's daemons from
    // consuming the comms budget. The count prunes dead holders as a side effect.
    let live = crate::daemon_lock::count_live_daemons_of(crate::daemon_lock::DaemonKind::Comms);
    let max = crate::daemon_lock::max_live_daemons();
    if live >= max {
        return Err(SingletonError::TooManyDaemons { count: live, max });
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
/// polls the socket to confirm the daemon actually went away. Public so `comms stop --all` can
/// signal each registered daemon by its socket without opening a full [`CommsClient`] (which could
/// respawn a daemon it meant to stop).
pub fn request_stop(socket_path: &Path) {
    let _ = roundtrip(socket_path, &CommsRequest::Stop);
}

/// Connect to the daemon endpoint, send one length-delimited msgpack request, and decode the one
/// framed reply. `None` on any transport/codec failure. Bounds the response to 64 KiB.
///
/// The daemon frames every reply as a [`CommsOut`] envelope (see `frontend_uds::send`), so we
/// decode `CommsOut` and unwrap its `Response`. Decoding a bare [`CommsResponse`] here silently
/// failed for every reply — which left [`daemon_status`] returning `None` and the version-gated
/// takeover in [`ensure_daemon`] never firing against a live daemon.
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
    match rmp_serde::from_slice::<CommsOut>(&buf).ok()? {
        CommsOut::Response(resp) => Some(resp),
        CommsOut::Notification(_) => None,
    }
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
    let mut prefix = [0u8; 4];
    stream.read_exact(&mut prefix).is_ok()
}

#[cfg(not(any(unix, windows)))]
pub fn probe_alive(_socket_path: &Path) -> bool {
    false
}

/// Env override naming the `basemind` binary to spawn the daemon from. Set it when the running
/// executable is not itself `basemind` — a test harness, or an embedding host.
pub const DAEMON_BINARY_ENV: &str = "BASEMIND_DAEMON_BINARY";

/// Resolve the executable to re-exec as `basemind comms daemon`.
///
/// This deliberately refuses to re-exec an executable that is not a `basemind` binary, because
/// blindly re-exec'ing `current_exe()` is a fork bomb rather than a mistake. Under `cargo test`,
/// `current_exe()` is the libtest harness: it reads the appended `comms daemon` as a *test-name
/// filter*, re-runs the whole suite, and every test in that re-run that reaches this function spawns
/// another generation. One stray call took this machine from ~800 to 8256 processes.
///
/// [`DAEMON_BINARY_ENV`] is the escape hatch for a legitimate non-`basemind` caller, and it is what
/// a test should set (to `env!("CARGO_BIN_EXE_basemind")`) when it genuinely wants a real daemon.
fn resolve_daemon_binary() -> std::io::Result<PathBuf> {
    if let Some(explicit) = std::env::var_os(DAEMON_BINARY_ENV) {
        return Ok(PathBuf::from(explicit));
    }
    daemon_binary_for(&std::env::current_exe()?)
}

/// The [`resolve_daemon_binary`] decision for a given running executable, split out so the refusal
/// can be tested without a harness whose own path decides the outcome.
fn daemon_binary_for(exe: &Path) -> std::io::Result<PathBuf> {
    let stem = exe.file_stem().and_then(std::ffi::OsStr::to_str).unwrap_or_default();
    if stem == "basemind" {
        return Ok(exe.to_path_buf());
    }
    // A cargo test harness lives at `target/<profile>/deps/<name>-<hash>`, with the real binary its
    // grandparent's `basemind`. Preferring that keeps a test that genuinely wants a daemon working
    // while still never re-exec'ing the harness itself.
    if let Some(sibling) = exe
        .parent()
        .and_then(std::path::Path::parent)
        .map(|dir| dir.join(if cfg!(windows) { "basemind.exe" } else { "basemind" }))
        .filter(|candidate| candidate.is_file())
    {
        return Ok(sibling);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "refusing to spawn a comms daemon by re-exec'ing {}: it is not a `basemind` binary. \
             Re-exec'ing a test harness makes it re-run its own suite and spawn again, without \
             bound. Set {DAEMON_BINARY_ENV} to a real `basemind` executable to spawn one from here.",
            exe.display()
        ),
    ))
}

/// Spawn `basemind comms daemon` detached so it outlives the spawning process. stdout/stderr
/// are redirected to null; the daemon's own tracing goes to its log sink.
pub fn spawn_detached_daemon(_paths: &CommsPaths) -> std::io::Result<()> {
    let exe = resolve_daemon_binary()?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("comms")
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` takes no arguments and only detaches the child into a new session.
        unsafe {
            command.pre_exec(|| {
                let _ = detach_session();
                Ok(())
            });
        }
    }
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

    /// The guard against the fork bomb. `current_exe()` under `cargo test` IS this harness binary,
    /// and re-exec'ing it with `comms daemon` appended makes libtest read those as a test-name filter
    /// and re-run the whole suite — which spawns again, without bound. So the resolver must refuse
    /// here rather than hand back a path.
    #[test]
    fn should_refuse_to_re_exec_a_binary_that_is_not_basemind() {
        let harness = std::path::Path::new("/nowhere/target/debug/deps/git_history_daemon-17c2bf1b");
        let error = daemon_binary_for(harness).expect_err("a test harness must never be re-exec'd");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let message = error.to_string();
        assert!(
            message.contains("not a `basemind` binary") && message.contains(DAEMON_BINARY_ENV),
            "the refusal must name the cause and the escape hatch; got: {message}"
        );
    }

    #[test]
    fn should_re_exec_a_real_basemind_binary() {
        let real = std::path::Path::new("/usr/local/bin/basemind");
        assert_eq!(daemon_binary_for(real).expect("a basemind binary resolves"), real);
    }

    /// A harness DOES get a daemon when the real binary is sitting where cargo puts it — the guard
    /// redirects the spawn, it does not disable it. What it must never return is the harness itself.
    #[test]
    fn should_redirect_a_harness_to_the_sibling_basemind_instead_of_itself() {
        let target = tempfile::tempdir().expect("tempdir");
        let deps = target.path().join("deps");
        std::fs::create_dir_all(&deps).expect("create deps dir");
        let real = target
            .path()
            .join(if cfg!(windows) { "basemind.exe" } else { "basemind" });
        std::fs::write(&real, b"").expect("write the sibling binary");
        let harness = deps.join("comms_smoke-deadbeef");

        assert_eq!(
            daemon_binary_for(&harness).expect("the sibling resolves"),
            real,
            "the harness must be redirected to the real binary, never re-exec'd"
        );
    }

    /// The escape hatch for an embedding host whose executable is neither `basemind` nor sitting
    /// beside one.
    #[test]
    fn should_use_the_explicit_binary_override_when_set() {
        let previous = std::env::var_os(DAEMON_BINARY_ENV);
        let explicit = std::path::PathBuf::from("/opt/somewhere/basemind-wrapper");
        // SAFETY: single-threaded within this test; the previous value is restored before asserting.
        unsafe { std::env::set_var(DAEMON_BINARY_ENV, &explicit) };
        let resolved = resolve_daemon_binary();
        match previous {
            Some(value) => unsafe { std::env::set_var(DAEMON_BINARY_ENV, value) },
            None => unsafe { std::env::remove_var(DAEMON_BINARY_ENV) },
        }

        assert_eq!(resolved.expect("the override resolves"), explicit);
    }

    #[test]
    fn version_is_older_orders_releases_and_ignores_prerelease() {
        assert!(version_is_older("0.6.3", "0.10.0"));
        assert!(version_is_older("0.9.0", "0.10.0"));
        assert!(version_is_older("0.10.0", "0.10.1"));
        assert!(!version_is_older("0.10.0", "0.10.0"));
        assert!(!version_is_older("0.11.0", "0.10.0"));
        assert!(!version_is_older("1.0.0", "0.10.0"));
        assert!(!version_is_older("0.10.0-rc.1", "0.10.0"));
        assert!(version_is_older("0.9.0-rc.2", "0.10.0"));
    }

    #[cfg(unix)]
    #[test]
    fn bind_as_lock_admits_exactly_one_winner_in_a_race() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("race.sock");
        let winners = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        const N: usize = 16;
        let listeners = Arc::new(std::sync::Mutex::new(Vec::new()));

        for _ in 0..N {
            let socket = socket.clone();
            let winners = winners.clone();
            let listeners = listeners.clone();
            handles.push(std::thread::spawn(move || {
                let probe = |p: &std::path::Path| p.exists();
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

    #[cfg(unix)]
    #[test]
    fn daemon_status_decodes_the_commsout_response_envelope() {
        use super::super::protocol::{CommsOut, CommsResponse, StatusReport};
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("status.sock");
        let listener = UnixListener::bind(&socket).expect("bind");

        let want = StatusReport {
            pid: 4242,
            version: "0.22.4".to_string(),
            build_id: "deadbeef1234".to_string(),
            proto_ver: PROTO_VER,
            uptime_secs: 99,
            threads: 3,
            subscribers: 0,
        };
        let reply = want.clone();
        // A faithful daemon: read the framed request, reply with the SAME CommsOut::Response ~keep
        // envelope the real UDS front-end sends (`frontend_uds::send`), not a bare CommsResponse. ~keep
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut prefix = [0u8; 4];
            stream.read_exact(&mut prefix).expect("read len");
            let len = u32::from_be_bytes(prefix) as usize;
            let mut req = vec![0u8; len];
            stream.read_exact(&mut req).expect("read req");
            let body = rmp_serde::to_vec_named(&CommsOut::Response(CommsResponse::Status(reply))).expect("encode");
            let out_len = u32::try_from(body.len()).expect("len fits");
            stream.write_all(&out_len.to_be_bytes()).expect("write len");
            stream.write_all(&body).expect("write body");
        });

        let got = daemon_status(&socket).expect("daemon_status must decode the CommsOut envelope");
        server.join().expect("server thread");
        assert_eq!(got.pid, want.pid);
        assert_eq!(got.version, want.version);
        assert_eq!(got.proto_ver, want.proto_ver);
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

    #[test]
    fn spawn_ready_timeout_covers_a_cold_full_feature_start() {
        const MINIMUM_COLD_START_BUDGET: Duration = Duration::from_secs(30);

        assert!(
            SPAWN_READY_TIMEOUT >= MINIMUM_COLD_START_BUDGET,
            "the comms daemon needs at least the agent daemon's ten-second cold-start budget"
        );
    }

    /// A socket path past `sun_path` can never be bound, so the caller must be told that — not
    /// left to wait out the readiness timeout and report a hang. Asserting the error is NOT
    /// `SpawnTimeout` is the point: that is the misleading message this replaces.
    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_daemon_rejects_a_socket_path_past_the_platform_limit() {
        let comms_dir = PathBuf::from("/tmp").join("d".repeat(SUN_PATH_MAX));
        let paths = CommsPaths {
            socket_path: comms_socket_path(&comms_dir),
            comms_dir,
        };
        let spawned = std::cell::Cell::new(false);
        let res = ensure_daemon_with(
            &paths,
            |_| false,
            |_| {
                spawned.set(true);
                Ok(())
            },
        )
        .await;

        let err = res.expect_err("an unbindable path must fail");
        assert!(
            matches!(err, SingletonError::SocketPathTooLong { .. }),
            "must name the path-length limit, not blame a timeout: {err}"
        );
        assert!(
            err.to_string().contains("BASEMIND_COMMS_DIR"),
            "the message must say which knob to change: {err}"
        );
        assert!(!spawned.get(), "must not spawn a daemon that cannot possibly bind");
    }

    /// Pins the boundary rather than a comfortable middle: an off-by-one here would either reject
    /// paths the kernel accepts or let through the one length that fails at `bind`.
    #[cfg(unix)]
    #[test]
    fn the_guard_accepts_the_longest_bindable_path_and_rejects_one_byte_more() {
        let at_limit = PathBuf::from("/".to_string() + &"a".repeat(SUN_PATH_MAX - 1));
        assert_eq!(at_limit.as_os_str().len(), SUN_PATH_MAX);
        check_socket_path_len(&at_limit).expect("a path exactly at the limit is bindable");

        let over = PathBuf::from("/".to_string() + &"a".repeat(SUN_PATH_MAX));
        assert!(
            matches!(
                check_socket_path_len(&over),
                Err(SingletonError::SocketPathTooLong { .. })
            ),
            "one byte past the limit must be rejected"
        );
    }
}
