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
//! | the same version | use it | use it; no two releases differ this way, and `Hello` reports the mismatch |
//! | older than this binary | ask it to stop | ask it to stop |
//!
//! A daemon of the same or a newer release is therefore never asked to stop. "Ask it to stop" sends
//! `Stop` and then follows the daemon's answer:
//!
//! | Answer to `Stop` | Result |
//! |---|---|
//! | accepted | wait for it to exit, then spawn this binary's daemon |
//! | refused (`daemon_busy`, or any other error) | never signalled: use it when the protocol matches, else fail with `BusyPredecessor` |
//! | none, but it still answers `Ping` | never signalled: fail with `StalePredecessor` |
//! | none, and `Ping` goes unanswered too | wait the exit window as for an accepted `Stop` |
//!
//! A daemon whose socket has gone quiet has the whole exit window to leave by itself, because it is
//! usually draining for someone else: another client's `Stop`, or its own idle exit. If its process
//! is still there afterwards it is asked once more, `Stop` and then `Ping`; if it is silent to both
//! it gets SIGTERM then SIGKILL — but only while `daemon.lock` in this generation reads held and
//! `daemon.pid`, read at that moment, names the pid it reported. Otherwise the call fails with
//! `StalePredecessor`. `basemind comms stop` and `comms stop --all` reclaim a daemon through the
//! same steps ([`reclaim_holder`]), identifying it by the lock and `daemon.pid` alone.
//!
//! Silent means asked and unanswered. A `Ping` that could not be sent ([`Liveness::Unknown`])
//! establishes nothing: the daemon is not signalled and none is spawned beside it.
//!
//! A daemon that does not answer `Status` at all is not classified: if it answers `Ping` it is
//! used, and the handshake reports what is wrong with it; if it is silent, one is spawned, and the
//! spawned daemon defers to whoever holds `daemon.lock`. With `BASEMIND_NO_AUTOSPAWN` set, nothing
//! is ever asked to stop, because nothing could replace it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use directories::ProjectDirs;

use super::protocol::{CommsRequest, CommsResponse, PROTO_VER, StatusReport};

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

/// How long to wait for a spawned daemon to become reachable before giving up. A spawn made while
/// a predecessor drains (its socket already gone) waits out that teardown for the lock
/// ([`HOLDER_DRAIN_WAIT`](super::daemon::HOLDER_DRAIN_WAIT)), then starts; full-feature binaries can
/// take materially longer to page in when every core and memory channel is busy.
const SPAWN_READY_TIMEOUT: Duration = super::daemon::HOLDER_DRAIN_WAIT.saturating_add(Duration::from_secs(15));
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
    /// The endpoint could not be asked whether a daemon is there: the exchange never started.
    /// Nothing was spawned or signalled on a guess.
    #[error(
        "could not ask the basemind comms daemon at {socket} whether it is running, so none was \
         started or stopped; retry"
    )]
    EndpointUnknown {
        /// The endpoint that could not be asked.
        socket: PathBuf,
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

/// Environment opt-out: when set true ([`parse_bool`](crate::config::engine::parse_bool)), basemind
/// connects to a daemon that is already running but never starts one itself.
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
    forbids_autospawn(std::env::var_os(NO_AUTOSPAWN_ENV).as_deref())
}

/// Whether a raw [`NO_AUTOSPAWN_ENV`] value forbids spawning. The binary refuses a non-boolean
/// value when it installs its engine; one that reaches here anyway (a library caller that never
/// installed it) is warned about and honoured as the opt-out the operator meant to set. A value
/// that is not UTF-8 is not a boolean either, so it forbids spawning too.
fn forbids_autospawn(value: Option<&OsStr>) -> bool {
    let value = value.map(OsStr::to_string_lossy);
    crate::config::engine::bool_env(NO_AUTOSPAWN_ENV, value.as_deref()).unwrap_or_else(|message| {
        crate::config::trust::warn_once(message);
        Some(true)
    }) == Some(true)
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

/// The comms directory itself, which holds one subdirectory per generation: the [`COMMS_DIR_ENV`]
/// override when set, else `comms/` under the per-user data directory. Creates nothing.
pub fn comms_base_dir() -> Result<PathBuf, SingletonError> {
    match comms_dir_override() {
        Some(dir) => Ok(dir),
        None => {
            let dirs = ProjectDirs::from("", "", "basemind").ok_or(SingletonError::NoDataDir)?;
            Ok(dirs.data_dir().join(COMMS_SUBDIR))
        }
    }
}

/// The [`COMMS_DIR_ENV`] override, when set.
fn comms_dir_override() -> Option<PathBuf> {
    std::env::var_os(COMMS_DIR_ENV)
        .filter(|dir| dir.to_str().is_none_or(|dir| !dir.trim().is_empty()))
        .map(PathBuf::from)
}

/// Resolve the per-user comms paths: this generation's directory under
/// `directories::ProjectDirs::from("", "", "basemind")`, or under the [`COMMS_DIR_ENV`] override
/// when set. Creates the directories as a side effect (see [`create_generation_dir`]).
pub fn resolve_paths() -> Result<CommsPaths, SingletonError> {
    let comms_base = comms_base_dir()?;
    let comms_dir = create_generation_dir(&comms_base, comms_dir_override().is_some())?;
    let socket_path = comms_socket_path(&comms_dir);
    Ok(CommsPaths { comms_dir, socket_path })
}

/// Create this generation's directory under `comms_base`, owner-only on Unix. An `overridden`
/// base that already exists is the user's directory and keeps its mode; one this call creates, and
/// the default base, are basemind's.
fn create_generation_dir(comms_base: &Path, overridden: bool) -> Result<PathBuf, SingletonError> {
    let owns_base = !overridden || !comms_base.exists();
    let comms_dir = generation_dir(comms_base);
    std::fs::create_dir_all(&comms_dir).map_err(|source| SingletonError::Io {
        path: comms_dir.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        if owns_base {
            set_mode(comms_base, OWNER_ONLY_DIR)?;
        }
        set_mode(&comms_dir, OWNER_ONLY_DIR)?;
    }
    #[cfg(not(unix))]
    let _ = owns_base;
    Ok(comms_dir)
}

/// The socket path for a generation directory (see [`generation_dir`]). On Unix,
/// `<comms_dir>/comms.sock`. On Windows, the named pipe [`pipe_name`] gives the directory, resolved
/// first so that every spelling of one directory, which is one `daemon.lock`, is one pipe.
pub fn comms_socket_path(comms_dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_string());
        let resolved = std::fs::canonicalize(comms_dir)
            .or_else(|_| std::path::absolute(comms_dir))
            .unwrap_or_else(|_| comms_dir.to_path_buf());
        PathBuf::from(pipe_name(&user, &resolved.to_string_lossy()))
    }
    #[cfg(not(windows))]
    {
        comms_dir.join(SOCKET_FILE)
    }
}

/// Hex digits of the hash carried in a pipe name: 64 bits, enough to keep one user's comms
/// directories apart.
#[cfg(any(windows, test))]
const PIPE_HASH_HEX_LEN: usize = 16;
/// Characters of the account name shown in a pipe name. The whole name is in the hash.
#[cfg(any(windows, test))]
const PIPE_USER_LEN: usize = 32;

/// The named pipe of the daemon that owns `comms_dir`:
/// `\\.\pipe\basemind-comms-<user>-<hash of the user and the directory's components>`. The path is
/// the generation directory, so the generation is in the hash and two generations in one comms
/// directory get two pipes. The hash covers the path's components, not its bytes: `C:\x\comms`,
/// `C:\x\comms\` and `C:/x/comms` are one directory and give one name, with or without the
/// `\\?\` prefix canonicalisation adds. BLAKE3 rather than the standard library's hasher, whose
/// output is not specified across toolchains: every binary of a generation must compute the same
/// name. The account name appears only as letters, digits, `.`, `_` and `-`, since `USERNAME` is
/// free text and a backslash would end the pipe's name. Releases up to 0.33 named their pipe from
/// another hash of the unscoped directory; this binary never computes that name.
#[cfg(any(windows, test))]
fn pipe_name(user: &str, comms_dir: &str) -> String {
    fn feed(hasher: &mut blake3::Hasher, part: &str) {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let unprefixed = comms_dir
        .strip_prefix(r"\\?\UNC\")
        .or_else(|| comms_dir.strip_prefix(r"\\?\"))
        .unwrap_or(comms_dir);
    let mut hasher = blake3::Hasher::new();
    feed(&mut hasher, user);
    for component in unprefixed
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
    {
        feed(&mut hasher, component);
    }
    let digest = hasher.finalize().to_hex();
    let shown: String = user
        .chars()
        .take(PIPE_USER_LEN)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(r"\\.\pipe\basemind-comms-{shown}-{}", &digest[..PIPE_HASH_HEX_LEN])
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
    let probe = |socket: &Path| {
        if is_alive(socket) {
            Liveness::Answers
        } else {
            Liveness::Silent
        }
    };
    ensure_daemon_probing(paths, policy, probe, spawn).await
}

/// [`ensure_daemon_with_policy`] with a probe that can say it does not know. A daemon is spawned
/// only when the probe found the endpoint silent: one that could not be asked may be serving, and
/// a second daemon would only exit on the lock the first holds.
async fn ensure_daemon_probing(
    paths: &CommsPaths,
    policy: SpawnPolicy,
    probe: impl Fn(&Path) -> Liveness,
    spawn: impl FnOnce(&CommsPaths) -> std::io::Result<()>,
) -> Result<(), SingletonError> {
    let is_alive = |socket: &Path| probe(socket) == Liveness::Answers;
    // Ahead of the opt-out on purpose: connect-only means *do not start one*, not *do not use one*.
    match probe(&paths.socket_path) {
        Liveness::Answers => return Ok(()),
        Liveness::Silent => {}
        Liveness::Unknown => {
            return Err(SingletonError::EndpointUnknown {
                socket: paths.socket_path.clone(),
            });
        }
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

/// [`ensure_daemon`] for a caller that retries and must not send an older daemon `Stop` on every
/// attempt. With `may_ask_stop` false the daemon is not asked: it is used if it speaks this
/// binary's protocol and reported as [`SingletonError::StalePredecessor`] otherwise; once it has
/// gone, this spawns as usual. Returns the outcome and whether this call sent a `Stop`, which is
/// what the caller paces on: most calls find nothing to ask.
pub async fn ensure_daemon_paced(paths: &CommsPaths, may_ask_stop: bool) -> (Result<(), SingletonError>, bool) {
    let takeover = if may_ask_stop { Takeover::Ask } else { Takeover::Wait };
    let mut stop_sent = false;
    let ensured = ensure_daemon_as(paths, SpawnPolicy::Auto, takeover, &mut stop_sent).await;
    (ensured, stop_sent)
}

async fn ensure_daemon_with_spawn_policy(paths: &CommsPaths, policy: SpawnPolicy) -> Result<(), SingletonError> {
    ensure_daemon_as(paths, policy, Takeover::Ask, &mut false).await
}

async fn ensure_daemon_as(
    paths: &CommsPaths,
    policy: SpawnPolicy,
    takeover: Takeover,
    stop_sent: &mut bool,
) -> Result<(), SingletonError> {
    let ours = takeover::Ours {
        version: env!("CARGO_PKG_VERSION"),
        proto_ver: PROTO_VER,
    };
    let settled =
        takeover::settle_existing(paths, policy, takeover, ours, takeover::Timings::PRODUCTION, stop_sent).await?;
    if settled == takeover::Existing::Use {
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
    ensure_daemon_probing(
        paths,
        policy,
        |socket| off_worker(|| probe_liveness(socket)),
        |paths: &CommsPaths| {
            let grants = match policy {
                SpawnPolicy::Auto => spawn::Grants::Withheld,
                SpawnPolicy::Explicit => spawn::Grants::Passed,
            };
            spawn::spawn_detached_daemon_with(paths, grants)
        },
    )
    .await
}

/// True when version `daemon` is strictly older than `ours` by semantic-version precedence: the
/// `MAJOR.MINOR.PATCH` core first, then the pre-release, so `0.34.0-rc.1` is older than `0.34.0`
/// and `rc.2` older than `rc.10`. Build metadata (`+…`) is ignored, and a part that is not a number
/// counts as zero.
fn version_is_older(daemon: &str, ours: &str) -> bool {
    use std::cmp::Ordering;

    fn parts(version: &str) -> ([u64; 3], Option<&str>) {
        let version = version.split('+').next().unwrap_or(version);
        let (core, pre) = match version.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (version, None),
        };
        let mut numbers = core.split('.').map(|part| part.parse::<u64>().unwrap_or(0));
        let core = std::array::from_fn(|_| numbers.next().unwrap_or(0));
        (core, pre)
    }
    /// Numeric identifiers order by value and below alphanumeric ones; a longer list of otherwise
    /// equal identifiers is the later one.
    fn compare_pre(a: &str, b: &str) -> Ordering {
        let (mut a, mut b) = (a.split('.'), b.split('.'));
        loop {
            let ordering = match (a.next(), b.next()) {
                (None, None) => return Ordering::Equal,
                (None, Some(_)) => return Ordering::Less,
                (Some(_), None) => return Ordering::Greater,
                (Some(x), Some(y)) => match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                },
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
    }
    let ((daemon_core, daemon_pre), (our_core, our_pre)) = (parts(daemon), parts(ours));
    let ordering = daemon_core.cmp(&our_core).then_with(|| match (daemon_pre, our_pre) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(a), Some(b)) => compare_pre(a, b),
    });
    ordering == Ordering::Less
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
use takeover::Takeover;
pub use takeover::{ReclaimRules, Reclaimed, reclaim_holder};

/// Serviceability probing + forced reclaim. See [`singleton_probe`](probe) for why these are not
/// the same question as [`probe_alive`].
#[path = "singleton_probe.rs"]
mod probe;
pub use probe::{
    DaemonProbe, StopOutcome, force_terminate, probe_serving, request_stop_classified, request_stop_within,
};

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
/// The daemon frames every reply as a [`CommsOut`](super::protocol::CommsOut) envelope (see
/// `frontend_uds::send`), so that is what is decoded and its `Response` unwrapped. Decoding a bare [`CommsResponse`] here silently
/// failed for every reply — which left [`daemon_status`] returning `None` and the version-gated
/// takeover in [`ensure_daemon`] never firing against a live daemon.
fn roundtrip(socket_path: &Path, req: &CommsRequest) -> Option<CommsResponse> {
    roundtrip_within(socket_path, req, ENDPOINT_TIMEOUT)
}

/// How long [`roundtrip`] waits on each read and write: enough for a daemon that is serving, short
/// enough that probing a wedged one does not stall bring-up.
const ENDPOINT_TIMEOUT: Duration = Duration::from_millis(800);

/// [`roundtrip`] with an explicit bound, for a request whose answer is worth waiting for.
fn roundtrip_within(socket_path: &Path, req: &CommsRequest, timeout: Duration) -> Option<CommsResponse> {
    let body = rmp_serde::to_vec_named(req).ok()?;
    endpoint::exchange(socket_path, body, timeout)
}

/// One-shot blocking exchanges with a daemon's endpoint. See [`singleton_endpoint`](endpoint) for
/// what bounds them on each platform.
#[path = "singleton_endpoint.rs"]
mod endpoint;
pub use endpoint::Liveness;
pub(crate) use endpoint::endpoint_accepts;
use endpoint::probe_once;

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
/// not falsely reclaimed. `false` also when the daemon could not be asked; a caller about to
/// signal or spawn uses [`probe_liveness`], which keeps that case apart.
pub fn probe_alive(socket_path: &Path) -> bool {
    probe_liveness(socket_path) == Liveness::Answers
}

/// [`probe_alive`] with the third answer kept: [`Liveness::Silent`] only when every attempt was
/// made and none was answered, [`Liveness::Unknown`] when any could not be made.
pub fn probe_liveness(socket_path: &Path) -> Liveness {
    let mut verdict = Liveness::Silent;
    for attempt in 0..PROBE_ATTEMPTS {
        match probe_once(socket_path) {
            Liveness::Answers => return Liveness::Answers,
            Liveness::Silent => {}
            Liveness::Unknown => verdict = Liveness::Unknown,
        }
        if attempt + 1 < PROBE_ATTEMPTS {
            std::thread::sleep(PROBE_RETRY_BACKOFF);
        }
    }
    verdict
}

/// Whether a machine daemon is up — and therefore holds this machine's git-history databases.
///
/// This is the ROUTING question for a process that does not already know the answer. `serve` knows it
/// by construction (it is the process that brings the daemon up); the
/// one-shot CLI does not, and must not guess. Guessing wrong is the whole failure: with a daemon up,
/// a local open can never win fjall's exclusive directory lock, so the CLI burns the retry ladder
/// (`GH_OPEN_RETRIES` × `GH_OPEN_BACKOFF`, ~1.3 s on every invocation, git or not) and then silently
/// live-walks — on the exact machine whose index is built, fresh, and hundreds of times faster.
///
/// Cheap by design, because it sits on the startup path of every CLI invocation:
///
/// * no endpoint on disk ⇒ no daemon, decided by one `stat` (Unix). This is the standalone case, and
///   it must stay free: [`probe_alive`] retries four times with
///   a 100 ms backoff before declaring a daemon dead — right for reclaiming a socket, a ~300 ms tax
///   here on a machine that simply has no daemon.
/// * otherwise one connect + ping. A live daemon answers on the first attempt; only an ORPHANED
///   socket (a crashed daemon) pays the full probe, and it is then correctly judged dead.
pub fn daemon_is_up() -> bool {
    let Ok(paths) = resolve_paths() else {
        return false;
    };
    #[cfg(unix)]
    if !paths.socket_path.exists() {
        return false;
    }
    // A socket file whose listener ACCEPTS a connection is a live daemon: an orphaned file (crashed
    // daemon) refuses. The ping round-trip of `probe_alive` is the wrong test here, because a busy
    // daemon (CPU-starved, mid-sync) can miss its 500 ms read timeout four times running; the CLI
    // then concludes "no daemon", loses the lock race for the index it could have proxied to, and
    // silently live-walks. Fall back to the full probe only when the connect itself fails.
    #[cfg(unix)]
    if std::os::unix::net::UnixStream::connect(&paths.socket_path).is_ok() {
        return true;
    }
    probe_alive(&paths.socket_path)
}

/// Daemon binary resolution + detached spawn. See [`singleton_spawn`](spawn) for why re-exec'ing
/// an executable that is not `basemind` is refused outright rather than merely discouraged.
#[path = "singleton_spawn.rs"]
mod spawn;
pub use spawn::{DAEMON_BINARY_ENV, Grants, daemon_log_path, spawn_detached_daemon, trim_daemon_log};

#[cfg(test)]
#[path = "singleton_tests.rs"]
mod tests;
