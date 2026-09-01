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
    /// No daemon answered and [`NO_AUTOSPAWN_ENV`] forbids starting one. The operator asked for
    /// connect-only behaviour; saying so is the whole point, so this is an error rather than a
    /// silent no-op that would surface later as an unexplained connection failure.
    #[error(
        "no basemind comms daemon is running at {socket} and {env} is set, so one will not be \
         auto-spawned; start it from your own supervisor (see the systemd unit in \
         docs/systemd/), or run `basemind comms start`, which ignores {env}"
    )]
    AutospawnDisabled {
        /// The endpoint nothing answered on.
        socket: PathBuf,
        /// The variable that suppressed the spawn, named so the message is actionable.
        env: &'static str,
    },
}

/// Environment opt-out: when set to a truthy value, basemind connects to a daemon that is already
/// running but never starts one itself.
///
/// This exists because `setsid` cannot solve the containment problem. A detached child changes
/// session and process group; on Linux it stays in the **spawning process's cgroup**, so a daemon
/// auto-spawned from an interactive shell is outside whatever `MemoryMax` the operator configured,
/// no matter what this code does. The only way to guarantee the daemon runs inside a
/// resource-controlled unit is for that unit to be the thing that starts it — and that requires
/// being able to switch the auto-spawn off. See `docs/systemd/basemind-comms.service`.
pub const NO_AUTOSPAWN_ENV: &str = "BASEMIND_NO_AUTOSPAWN";

/// Whether [`NO_AUTOSPAWN_ENV`] forbids spawning a daemon from this process.
pub fn autospawn_disabled() -> bool {
    std::env::var(NO_AUTOSPAWN_ENV).is_ok_and(|value| {
        let value = value.trim();
        value.eq_ignore_ascii_case("1") || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
    })
}

/// Whether a caller is allowed to spawn a daemon when none answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnPolicy {
    /// Spawn on demand unless [`NO_AUTOSPAWN_ENV`] says otherwise. Every implicit caller.
    Auto,
    /// Spawn regardless: the operator typed a command whose entire purpose is to start the daemon,
    /// so honouring the auto-spawn opt-out there would make `comms start` a no-op.
    Explicit,
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
///
/// This is the single choke point every implicit bring-up passes through — [`ensure_daemon`] and
/// `CommsClient::reconnect`, which does not go via [`ensure_daemon`] — so it is where the
/// [`NO_AUTOSPAWN_ENV`] opt-out is enforced.
pub async fn ensure_daemon_with(
    paths: &CommsPaths,
    is_alive: impl Fn(&Path) -> bool,
    spawn: impl FnOnce(&CommsPaths) -> std::io::Result<()>,
) -> Result<(), SingletonError> {
    ensure_daemon_with_policy(paths, SpawnPolicy::Auto, is_alive, spawn).await
}

/// [`ensure_daemon_with`] with an explicit [`SpawnPolicy`].
pub async fn ensure_daemon_with_policy(
    paths: &CommsPaths,
    policy: SpawnPolicy,
    is_alive: impl Fn(&Path) -> bool,
    spawn: impl FnOnce(&CommsPaths) -> std::io::Result<()>,
) -> Result<(), SingletonError> {
    // Ahead of the opt-out on purpose: connect-only means *do not start one*, not *do not use one*.
    if is_alive(&paths.socket_path) {
        return Ok(());
    }
    if policy == SpawnPolicy::Auto && autospawn_disabled() {
        return Err(SingletonError::AutospawnDisabled {
            socket: paths.socket_path.clone(),
            env: NO_AUTOSPAWN_ENV,
        });
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
    ensure_daemon_with_spawn_policy(paths, SpawnPolicy::Auto).await
}

/// [`ensure_daemon`] for `basemind comms start`: the operator asked for a daemon in so many words,
/// so [`NO_AUTOSPAWN_ENV`] does not apply. It is what makes the opt-out usable — a supervisor unit
/// that sets the variable globally still needs one command that actually starts the thing.
pub async fn ensure_daemon_explicit(paths: &CommsPaths) -> Result<(), SingletonError> {
    ensure_daemon_with_spawn_policy(paths, SpawnPolicy::Explicit).await
}

async fn ensure_daemon_with_spawn_policy(paths: &CommsPaths, policy: SpawnPolicy) -> Result<(), SingletonError> {
    if let Some(report) = daemon_status(&paths.socket_path) {
        let ours = env!("CARGO_PKG_VERSION");
        let compatible = report.proto_ver == PROTO_VER && !version_is_older(&report.version, ours);
        if compatible {
            return Ok(());
        }
        // Never stop a daemon we are then forbidden to replace. Under connect-only the takeover
        // would leave the operator with no daemon at all — strictly worse than an old one, whose
        // skew the handshake reports precisely.
        if policy == SpawnPolicy::Auto && autospawn_disabled() {
            tracing::warn!(
                daemon_version = %report.version,
                daemon_pid = report.pid,
                ours,
                env = NO_AUTOSPAWN_ENV,
                "comms: a previous/incompatible daemon holds the socket, but auto-spawn is disabled; using it as-is"
            );
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
    ensure_daemon_with_policy(paths, policy, probe_alive, spawn_detached_daemon).await
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

/// Serviceability probing + forced reclaim. See [`singleton_probe`](probe) for why these are not
/// the same question as [`probe_alive`].
#[path = "singleton_probe.rs"]
mod probe;
pub use probe::{DaemonProbe, StopOutcome, force_terminate, probe_serving, request_stop_classified};

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

/// Daemon binary resolution + detached spawn. See [`singleton_spawn`](spawn) for why re-exec'ing
/// an executable that is not `basemind` is refused outright rather than merely discouraged.
#[path = "singleton_spawn.rs"]
mod spawn;
pub use spawn::{DAEMON_BINARY_ENV, spawn_detached_daemon};

#[cfg(test)]
mod tests {
    use super::*;

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
