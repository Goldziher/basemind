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
//!
//! ## Protocol generations
//!
//! Everything a daemon owns — socket (or named pipe), `daemon.lock`, `daemon.pid`, `store.fjall/`,
//! `daemon.log`, `http.addr` — lives in a per-generation subdirectory of the comms directory, named
//! by [`RENDEZVOUS_GENERATION`]. A binary only ever looks inside its own generation, so two installed
//! releases of different generations each run their own daemon and cannot see, stop or signal the
//! other's. The machine registry is scoped the same way (see [`crate::daemon_lock`]), so
//! `comms stop --all` and `comms doctor` reach this generation's daemons only. Releases up to 0.33
//! predate this and use the comms directory itself; that unscoped layout is their generation, and
//! this binary never opens it.
//!
//! ## Takeover within a generation
//!
//! One generation holds exactly one daemon. A client that finds one running reads its `Status`
//! and decides ([`ensure_daemon`]):
//!
//! | Running daemon is | same protocol | different protocol |
//! |---|---|---|
//! | newer than this binary | use it | fail with `NewerDaemon`; it is not asked to stop |
//! | the same version | use it | ask it to stop |
//! | older than this binary | ask it to stop | ask it to stop |
//!
//! "Ask it to stop" sends `Stop` and then follows the daemon's answer:
//!
//! | Answer to `Stop` | Result |
//! |---|---|
//! | accepted | wait for it to exit, then spawn this binary's daemon |
//! | refused (`daemon_busy`, or any other error) | never signalled: use it when the protocol matches, else fail with `BusyPredecessor` |
//! | none, but it still answers `Ping` | never signalled: fail with `StalePredecessor` |
//! | none, and `Ping` goes unanswered too | SIGTERM then SIGKILL, only when `daemon.pid` in this generation names the pid it reported; then spawn |
//!
//! A daemon that does not answer `Status` at all is not classified: if it answers `Ping` it is
//! used, and the handshake reports what is wrong with it; if it does not, one is spawned, and the
//! spawned daemon defers to whoever holds `daemon.lock`. With `BASEMIND_NO_AUTOSPAWN` set, nothing
//! is ever asked to stop, because nothing could replace it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use directories::ProjectDirs;

use super::protocol::{CommsOut, CommsRequest, CommsResponse, PROTO_VER, StatusReport};

/// Subdirectory under the user data dir holding the comms generations.
const COMMS_SUBDIR: &str = "comms";

pub use crate::daemon_lock::RENDEZVOUS_GENERATION;

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
/// Resolved per-user comms paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommsPaths {
    /// This generation's directory (`<data_dir>/comms/g<N>/`) holding the store, lock and socket.
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
    /// A newer daemon holds this generation and speaks a protocol this binary does not. It is
    /// never stopped from here: the newer install owns the daemon, and replacing it with an older
    /// one would sever its sessions only for them to take it back.
    #[error(
        "a newer basemind comms daemon (v{version}, pid {pid}, protocol {daemon_proto}) is running \
         and this basemind (v{ours}, protocol {our_proto}) cannot talk to it; update this basemind, \
         or stop that daemon with `basemind comms stop` once its sessions are done"
    )]
    NewerDaemon {
        /// The running daemon's build version.
        version: String,
        /// The running daemon's process id.
        pid: u32,
        /// The protocol version the daemon reported.
        daemon_proto: u32,
        /// This binary's version.
        ours: String,
        /// This binary's protocol version.
        our_proto: u32,
    },
    /// A daemon this binary cannot use answered `Stop` with a refusal. It is left running — the
    /// refusal protects the sessions attached to it — and the caller is told why.
    #[error(
        "a basemind comms daemon from another build (v{version}, pid {pid}) refused to stop \
         [{code}]: {message}. It was left running; close the sessions using it (it exits once \
         idle) or run `basemind comms stop`, then retry"
    )]
    BusyPredecessor {
        /// The running daemon's build version.
        version: String,
        /// The running daemon's process id.
        pid: u32,
        /// The daemon's stable refusal token (`daemon_busy`, …).
        code: String,
        /// The daemon's rendered refusal.
        message: String,
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

/// Environment override for the comms directory. When set, it replaces the per-user
/// `directories::ProjectDirs` location. Intended for tests, CI, and users who want the broker's
/// socket + store in a custom (e.g. sandboxed) location.
///
/// The override names the comms directory, not a generation: [`RENDEZVOUS_GENERATION`] applies inside
/// it exactly as it does inside the default location, so two binaries of different generations
/// pointed at one override still do not meet.
pub const COMMS_DIR_ENV: &str = "BASEMIND_COMMS_DIR";

/// This binary's generation directory inside a comms directory: `<comms_base>/g<N>`. The one place
/// the generation enters a path — the socket, lock, pidfile, store and log are all derived from the
/// directory this returns, and on Windows the pipe name is a hash of it.
pub fn generation_dir(comms_base: &Path) -> PathBuf {
    comms_base.join(format!("g{RENDEZVOUS_GENERATION}"))
}

/// Resolve the per-user comms paths: this generation's directory under
/// `directories::ProjectDirs::from("", "", "basemind")`, or under the [`COMMS_DIR_ENV`] override
/// when set. Creates the directories (mode 0700 on Unix) as a side effect.
pub fn resolve_paths() -> Result<CommsPaths, SingletonError> {
    let comms_base = match std::env::var_os(COMMS_DIR_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let dirs = ProjectDirs::from("", "", "basemind").ok_or(SingletonError::NoDataDir)?;
            dirs.data_dir().join(COMMS_SUBDIR)
        }
    };
    let comms_dir = generation_dir(&comms_base);
    std::fs::create_dir_all(&comms_dir).map_err(|source| SingletonError::Io {
        path: comms_dir.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        set_mode(&comms_base, OWNER_ONLY_DIR)?;
        set_mode(&comms_dir, OWNER_ONLY_DIR)?;
    }

    let socket_path = comms_socket_path(&comms_dir);
    Ok(CommsPaths { comms_dir, socket_path })
}

/// The socket path for a generation directory (see [`generation_dir`]). On Unix,
/// `<comms_dir>/comms.sock`. On Windows, the named pipe [`pipe_name`] gives the directory.
pub fn comms_socket_path(comms_dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_string());
        PathBuf::from(pipe_name(&user, comms_dir))
    }
    #[cfg(not(windows))]
    {
        comms_dir.join(SOCKET_FILE)
    }
}

/// Hex digits of the directory hash carried in a pipe name: 64 bits, enough to keep one user's
/// comms directories apart.
#[cfg(any(windows, test))]
const PIPE_HASH_HEX_LEN: usize = 16;

/// The named pipe of the daemon that owns `comms_dir`:
/// `\\.\pipe\basemind-comms-<user>-<hash of the directory path>`. The path is the generation
/// directory, so the generation is in the hash and two generations in one comms directory get two
/// pipes. BLAKE3 rather than the standard library's hasher, whose output is not specified across
/// toolchains: every binary of a generation must compute the same name. Releases up to 0.33 named
/// their pipe from another hash of the unscoped directory; this binary never computes that name.
#[cfg(any(windows, test))]
fn pipe_name(user: &str, comms_dir: &Path) -> String {
    let digest = blake3::hash(comms_dir.as_os_str().as_encoded_bytes()).to_hex();
    format!(r"\\.\pipe\basemind-comms-{user}-{}", &digest[..PIPE_HASH_HEX_LEN])
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

/// Ensure a daemon this binary can use is running in its generation, replacing one from another
/// build only when that daemon agrees to go. The rules are the decision table in the module doc; in
/// short, a daemon that answers is never signalled, and a newer one is never even asked to stop.
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
    let ours = takeover::Ours {
        version: env!("CARGO_PKG_VERSION"),
        proto_ver: PROTO_VER,
    };
    if takeover::settle_existing(paths, policy, ours, takeover::Timings::PRODUCTION).await? == takeover::Existing::Use {
        return Ok(());
    }
    // No compatible daemon answered — we are about to spawn one. Enforce the machine ceiling for the
    // comms family first so a runaway (each isolated comms dir spawning its own detached daemon) is
    // refused loudly, not silently piled up. Counting per kind keeps another family's daemons from
    // consuming the comms budget. The count covers every generation's daemons, read from the
    // registry without contacting them, and prunes this generation's dead holders as a side effect.
    let live = crate::daemon_lock::count_live_daemons_of(crate::daemon_lock::DaemonKind::Comms);
    let max = crate::daemon_lock::max_live_daemons();
    if live >= max {
        return Err(SingletonError::TooManyDaemons { count: live, max });
    }
    ensure_daemon_with_policy(
        paths,
        policy,
        |socket| off_worker(|| probe_alive(socket)),
        spawn_detached_daemon,
    )
    .await
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

/// What to do about a daemon that is already running in this generation.
#[path = "singleton_takeover.rs"]
mod takeover;

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
        CommsOut::Response(resp) | CommsOut::Reply { response: resp, .. } => Some(resp),
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

/// Run blocking `work` without parking an async worker: on a multi-thread runtime the worker is
/// handed off via `block_in_place`; on a current-thread runtime (or none) it just runs inline, since
/// `block_in_place` panics there. For the synchronous probes ([`probe_alive`], [`daemon_status`]) that
/// async bring-up paths call — a wedged daemon makes them burn up to ~2 s of std socket timeouts.
pub(crate) fn off_worker<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
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
pub use spawn::{DAEMON_BINARY_ENV, daemon_log_path, spawn_detached_daemon, trim_daemon_log};

#[cfg(test)]
#[path = "singleton_tests.rs"]
mod tests;
