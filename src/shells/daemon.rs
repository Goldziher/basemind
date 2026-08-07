//! Embedded rmux daemon entry point.
//!
//! basemind ships its own rmux daemon rather than depending on an external
//! `rmux` binary. The rmux SDK's `connect_or_start` spawns a daemon by
//! re-executing a binary with the hidden flag
//! [`rmux_client::INTERNAL_DAEMON_FLAG`] (`--__internal-daemon`) followed by the
//! socket path and any config flags. By pointing the SDK at our own executable
//! (`current_exe()`, set via [`point_sdk_daemon_at`] from [`intercept_from_env`]
//! at `main` startup) and intercepting that flag at the very top of `main`,
//! `basemind --__internal-daemon <socket> [config…]` BECOMES the daemon.
//!
//! [`run_internal_daemon`] mirrors rmux's own `run_hidden_daemon`
//! (`/tmp/rmux` reference clone, `src/main.rs`): parse the socket path, build a
//! [`rmux_server::DaemonConfig`] with a generated internal config and no web
//! frontend, then bind + wait on a dedicated tokio runtime. The config disables
//! rmux's default exit-on-empty behavior so basemind's own idle reaper controls
//! daemon lifetime. Trailing config flags from the SDK are intentionally ignored.

use std::ffi::OsString;
#[cfg(not(windows))]
use std::path::Component;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::daemon_lock::{DaemonKind, DaemonLock, DaemonLockOutcome};

const IDLE_REAP_AFTER: Duration = Duration::from_secs(10 * 60);
const IDLE_REAP_CHECK_EVERY: Duration = Duration::from_secs(5);
const IDLE_REAP_AFTER_ENV: &str = "BASEMIND_SHELLS_IDLE_REAP_SECS";
const IDLE_REAP_CHECK_EVERY_ENV: &str = "BASEMIND_SHELLS_IDLE_CHECK_SECS";
const DAEMON_DIR_EXTENSION: &str = "daemon";
const RMUX_CONFIG_FILE_NAME: &str = "rmux.conf";
const RMUX_CONFIG: &str = "set-option -g exit-empty off\nset-option -g remain-on-exit on\n";
#[cfg(any(windows, test))]
const SHELLS_LOCKS_SUBDIR: &str = "shells";

#[derive(Debug, Default)]
struct IdleReapState {
    empty_since: Option<Instant>,
}

impl IdleReapState {
    fn observe(&mut self, sessions_empty: bool, now: Instant, idle_after: Duration) -> bool {
        if !sessions_empty {
            self.empty_since = None;
            return false;
        }

        let empty_since = self.empty_since.get_or_insert(now);
        now.duration_since(*empty_since) >= idle_after
    }

    fn observe_unknown(&mut self, now: Instant, idle_after: Duration) -> bool {
        self.observe(true, now, idle_after)
    }
}

/// Inspect the process arguments and, when basemind was re-execed as the
/// embedded rmux daemon, run the daemon and return its result.
///
/// The rmux SDK starts a daemon by re-execing the daemon binary with the hidden
/// [`rmux_client::INTERNAL_DAEMON_FLAG`] (`--__internal-daemon`) as the first
/// real argument, followed by the socket path and any config flags. When that
/// flag is present this returns `Some(run_internal_daemon(rest))`; otherwise it
/// returns `None` and the caller proceeds with normal CLI parsing.
///
/// Called at the very top of `main`, before clap parses, so the daemon process
/// never sees basemind's CLI surface.
#[must_use]
pub fn intercept_from_env() -> Option<Result<()>> {
    let mut args = std::env::args_os();
    let _argv0 = args.next();
    match args.next() {
        Some(first) if first == rmux_client::INTERNAL_DAEMON_FLAG => Some(run_internal_daemon(args)),
        _ => {
            point_sdk_daemon_at_self();
            None
        }
    }
}

/// Set the rmux SDK's daemon-binary discovery env var to `current_exe()`, so
/// `connect_or_start` re-execs basemind as the embedded daemon. Best-effort: if
/// the executable path can't be resolved the variable is left unset and the
/// first `shell_spawn` surfaces a clear "could not start daemon" error instead.
fn point_sdk_daemon_at_self() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // SAFETY: called only from `intercept_from_env` at the very top of `main`,
    unsafe { point_sdk_daemon_at(&exe) }
}

/// Point the rmux SDK's daemon-binary discovery at `binary`, so `connect_or_start`
/// re-execs `binary --__internal-daemon …` as the daemon.
///
/// Centralizes the single `set_var` basemind performs for the shells feature.
/// Production calls it via [`intercept_from_env`] at `main` startup; integration
/// tests (which never run basemind's `main`) call it to point the SDK at the
/// separately built `basemind` binary.
///
/// # Safety
/// `std::env::set_var` is not thread-safe under the 2024 edition. The caller must
/// ensure no other thread is concurrently reading or writing the environment —
/// call this once, before any rmux interaction and before the multi-threaded
/// runtime is doing other work.
pub unsafe fn point_sdk_daemon_at(binary: &std::path::Path) {
    unsafe {
        std::env::set_var(rmux_sdk::bootstrap::discovery::SDK_DAEMON_BINARY_ENV, binary);
    }
}

/// Run basemind as the embedded rmux daemon and block until shutdown.
///
/// `args` are the arguments that followed [`rmux_client::INTERNAL_DAEMON_FLAG`]
/// on the command line: the first non-`--` argument is the Unix socket path the
/// daemon must bind, and any subsequent `--…` flags are config selectors the SDK
/// forwards. basemind ignores those trailing flags and always runs with its own
/// generated config and no web frontend.
///
/// This builds its own multi-thread tokio runtime (the daemon owns the process
/// at this point — `main` has not yet parsed clap and never will), then polls
/// rmux's live session set. After it remains empty for ten minutes, the server
/// shuts down cleanly. Tests and operators can override that window with
/// `BASEMIND_SHELLS_IDLE_REAP_SECS` and the five-second polling cadence with
/// `BASEMIND_SHELLS_IDLE_CHECK_SECS`; both accept positive whole seconds. A
/// per-socket daemon lock is acquired before the socket bind and held through
/// idle shutdown so concurrent re-execs converge without disturbing the owner.
pub fn run_internal_daemon<I>(args: I) -> Result<()>
where
    I: IntoIterator<Item = OsString>,
{
    let socket_path = parse_socket_path(args).context("the embedded rmux daemon requires a socket path argument")?;
    validate_socket_path(&socket_path)?;
    let daemon_dir = daemon_lock_dir(&socket_path);
    std::fs::create_dir_all(&daemon_dir)
        .with_context(|| format!("create embedded rmux daemon lock directory {}", daemon_dir.display()))?;
    let _daemon_lock = match DaemonLock::acquire_kind(DaemonKind::Shells, &daemon_dir, env!("CARGO_PKG_VERSION"))
        .with_context(|| format!("acquire embedded rmux daemon lock in {}", daemon_dir.display()))?
    {
        DaemonLockOutcome::Acquired(lock) => lock,
        DaemonLockOutcome::AlreadyHeld(record) => {
            tracing::info!(
                holder_pid = record.as_ref().map(|holder| holder.pid),
                daemon_dir = %daemon_dir.display(),
                "embedded rmux daemon already running"
            );
            return Ok(());
        }
    };

    let config_path = daemon_dir.join(RMUX_CONFIG_FILE_NAME);
    std::fs::write(&config_path, RMUX_CONFIG)
        .with_context(|| format!("write embedded rmux daemon config {}", config_path.display()))?;
    let config = rmux_server::DaemonConfig::new(socket_path.clone()).with_config_files(vec![config_path], false, None);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for embedded rmux daemon")?;

    runtime.block_on(async move {
        let server = rmux_server::ServerDaemon::new(config)
            .bind()
            .await
            .context("bind embedded rmux daemon socket")?;

        let rmux = daemon_rmux(&socket_path);
        wait_until_sessions_idle(
            &rmux,
            &socket_path,
            duration_from_env(IDLE_REAP_AFTER_ENV, IDLE_REAP_AFTER),
            duration_from_env(IDLE_REAP_CHECK_EVERY_ENV, IDLE_REAP_CHECK_EVERY),
        )
        .await;

        server.shutdown().await.context("shut down idle embedded rmux daemon")?;
        Ok::<(), anyhow::Error>(())
    })
}

#[cfg(not(windows))]
fn daemon_lock_dir(socket_path: &Path) -> PathBuf {
    socket_path.with_extension(DAEMON_DIR_EXTENSION)
}

#[cfg(windows)]
fn daemon_lock_dir(socket_path: &Path) -> PathBuf {
    windows_daemon_lock_dir(&crate::store_layout::cache_root(), socket_path)
}

#[cfg(any(windows, test))]
fn windows_daemon_lock_dir(cache_root: &Path, socket_path: &Path) -> PathBuf {
    let socket_hash = crate::hashing::hex(&crate::hashing::hash_bytes(socket_path.as_os_str().as_encoded_bytes()));
    cache_root
        .join(SHELLS_LOCKS_SUBDIR)
        .join(format!("{socket_hash}.{DAEMON_DIR_EXTENSION}"))
}

fn daemon_rmux(socket_path: &Path) -> rmux_sdk::Rmux {
    #[cfg(windows)]
    let builder = rmux_sdk::Rmux::builder().windows_pipe(socket_path.to_string_lossy().into_owned());
    #[cfg(not(windows))]
    let builder = rmux_sdk::Rmux::builder().unix_socket(socket_path.to_path_buf());

    builder.build()
}

async fn wait_until_sessions_idle(
    rmux: &rmux_sdk::Rmux,
    socket_path: &Path,
    idle_after: Duration,
    check_every: Duration,
) {
    let mut state = IdleReapState::default();

    loop {
        tokio::time::sleep(check_every).await;
        let sessions = match super::session::list_session_liveness_strict(rmux).await {
            Ok(sessions) => sessions,
            Err(error) => {
                if endpoint_is_absent(socket_path).await {
                    tracing::info!("embedded rmux daemon endpoint removed; shutdown already started");
                    return;
                }
                tracing::warn!(error = %error, "embedded rmux daemon session poll failed; retrying");
                if state.observe_unknown(Instant::now(), idle_after) {
                    tracing::info!(
                        idle_after_secs = idle_after.as_secs(),
                        "embedded rmux daemon liveness unavailable through idle window; shutting down"
                    );
                    return;
                }
                continue;
            }
        };

        let has_live_session = sessions.iter().any(|(_, alive)| *alive);
        if state.observe(!has_live_session, Instant::now(), idle_after) {
            tracing::info!(
                idle_after_secs = idle_after.as_secs(),
                retained_sessions = sessions.len(),
                "embedded rmux daemon idle; shutting down"
            );
            return;
        }
    }
}

async fn endpoint_is_absent(socket_path: &Path) -> bool {
    let socket_path = socket_path.to_path_buf();
    match tokio::task::spawn_blocking(move || rmux_client::connect_or_absent(&socket_path)).await {
        Ok(Ok(rmux_client::ConnectResult::Absent)) => true,
        Ok(Ok(rmux_client::ConnectResult::Connected(_))) => false,
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "embedded rmux daemon endpoint probe failed");
            false
        }
        Err(error) => {
            tracing::warn!(error = %error, "embedded rmux daemon endpoint probe task failed");
            false
        }
    }
}

fn duration_from_env(name: &str, fallback: Duration) -> Duration {
    duration_from_env_value(std::env::var_os(name), fallback)
}

fn duration_from_env_value(value: Option<OsString>, fallback: Duration) -> Duration {
    value
        .and_then(|value| value.to_str().and_then(|raw| raw.parse::<u64>().ok()))
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(fallback)
}

/// Extract the socket path from the internal-daemon argument list.
///
/// Matches rmux's own parser: the first argument that does NOT start with `--`
/// is the socket path. Everything else (config-file selectors, web flags) is a
/// `--…` flag basemind deliberately drops. Returns `None` when no positional
/// socket path is present.
fn parse_socket_path<I>(args: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = OsString>,
{
    for arg in args {
        if !arg.as_encoded_bytes().starts_with(b"--") {
            return Some(PathBuf::from(arg));
        }
    }
    None
}

/// Reject a daemon socket path that is not an absolute, traversal-free path.
///
/// The path arrives as a process argument when basemind is re-execed as the
/// embedded daemon. Although the SDK only ever passes a basemind-owned absolute
/// path, validating defends against argument confusion (e.g. an external caller
/// invoking `basemind --__internal-daemon ../evil`): a relative path or one
/// containing a `..` component is refused so the daemon can only bind where it
/// was legitimately told to.
pub(crate) fn validate_socket_path(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        const PIPE_PREFIX: &str = r"\\.\pipe\";
        let display = path.to_string_lossy();
        if !display.starts_with(PIPE_PREFIX) {
            bail!("embedded rmux daemon named-pipe path must start with `{PIPE_PREFIX}`, got {display}");
        }
        let name = &display[PIPE_PREFIX.len()..];
        if name.is_empty() || name.contains('\\') || name.contains('/') {
            bail!("embedded rmux daemon named-pipe name is empty or contains a separator: {display}");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        if !path.is_absolute() {
            bail!(
                "embedded rmux daemon socket path must be absolute, got {}",
                path.display()
            );
        }
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            bail!(
                "embedded rmux daemon socket path must not contain `..`, got {}",
                path.display()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn daemon_lock_dir_is_dedicated_to_the_socket() {
        assert_eq!(
            daemon_lock_dir(Path::new("/tmp/basemind/shells/rmux.sock")),
            PathBuf::from("/tmp/basemind/shells/rmux.daemon")
        );
    }

    #[test]
    fn windows_daemon_lock_dir_hashes_the_named_pipe_under_the_cache_root() {
        let cache_root = Path::new(r"C:\Users\alice\AppData\Roaming\basemind");
        let socket_path = Path::new(r"\\.\pipe\basemind-shells-alice");

        assert_eq!(
            windows_daemon_lock_dir(cache_root, socket_path),
            cache_root
                .join("shells")
                .join("235756a7677c8cf483482227975012a1ff840203d5621bb94150a5b3f8bf57b0.daemon")
        );
    }

    #[test]
    fn shell_daemon_lock_allows_only_one_owner_per_socket() {
        let directory = tempfile::tempdir().expect("create daemon test directory");
        let machine_dir = directory.path().join("machine-daemons");
        #[cfg(not(windows))]
        let first_lock_dir = daemon_lock_dir(&directory.path().join("first.sock"));
        #[cfg(not(windows))]
        let second_lock_dir = daemon_lock_dir(&directory.path().join("second.sock"));
        #[cfg(windows)]
        let first_lock_dir = windows_daemon_lock_dir(directory.path(), Path::new(r"\\.\pipe\first"));
        #[cfg(windows)]
        let second_lock_dir = windows_daemon_lock_dir(directory.path(), Path::new(r"\\.\pipe\second"));
        std::fs::create_dir_all(&first_lock_dir).expect("create first lock directory");
        std::fs::create_dir_all(&second_lock_dir).expect("create second lock directory");

        let first = crate::daemon_lock::DaemonLock::acquire_at(
            crate::daemon_lock::DaemonKind::Shells,
            &first_lock_dir,
            env!("CARGO_PKG_VERSION"),
            &machine_dir,
        )
        .expect("acquire first shell daemon lock");
        assert!(matches!(&first, crate::daemon_lock::DaemonLockOutcome::Acquired(_)));

        let contender = crate::daemon_lock::DaemonLock::acquire_at(
            crate::daemon_lock::DaemonKind::Shells,
            &first_lock_dir,
            env!("CARGO_PKG_VERSION"),
            &machine_dir,
        )
        .expect("contend for first shell daemon lock");
        assert!(matches!(
            contender,
            crate::daemon_lock::DaemonLockOutcome::AlreadyHeld(_)
        ));

        let second = crate::daemon_lock::DaemonLock::acquire_at(
            crate::daemon_lock::DaemonKind::Shells,
            &second_lock_dir,
            env!("CARGO_PKG_VERSION"),
            &machine_dir,
        )
        .expect("acquire second shell daemon lock");
        assert!(matches!(second, crate::daemon_lock::DaemonLockOutcome::Acquired(_)));
    }

    #[test]
    fn empty_sessions_reap_only_after_the_idle_window() {
        let started = std::time::Instant::now();
        let idle_after = std::time::Duration::from_secs(10);
        let mut state = IdleReapState::default();

        assert!(!state.observe(true, started, idle_after));
        assert!(!state.observe(
            true,
            started + idle_after - std::time::Duration::from_nanos(1),
            idle_after
        ));
        assert!(state.observe(true, started + idle_after, idle_after));
    }

    #[test]
    fn live_session_resets_the_empty_window() {
        let started = std::time::Instant::now();
        let idle_after = std::time::Duration::from_secs(10);
        let mut state = IdleReapState::default();

        assert!(!state.observe(true, started, idle_after));
        assert!(!state.observe(false, started + idle_after, idle_after));
        assert!(!state.observe(true, started + idle_after, idle_after));
        assert!(!state.observe(
            true,
            started + idle_after * 2 - std::time::Duration::from_nanos(1),
            idle_after
        ));
        assert!(state.observe(true, started + idle_after * 2, idle_after));
    }

    #[test]
    fn repeated_unknown_liveness_cannot_pin_daemon() {
        let started = std::time::Instant::now();
        let idle_after = std::time::Duration::from_secs(10);
        let mut state = IdleReapState::default();

        assert!(!state.observe_unknown(started, idle_after));
        assert!(!state.observe_unknown(started + idle_after - std::time::Duration::from_nanos(1), idle_after));
        assert!(state.observe_unknown(started + idle_after, idle_after));
    }

    #[test]
    fn duration_override_accepts_positive_whole_seconds() {
        let fallback = std::time::Duration::from_secs(600);

        assert_eq!(
            duration_from_env_value(Some(OsString::from("2")), fallback),
            std::time::Duration::from_secs(2)
        );
        assert_eq!(duration_from_env_value(Some(OsString::from("0")), fallback), fallback);
        assert_eq!(
            duration_from_env_value(Some(OsString::from("invalid")), fallback),
            fallback
        );
        assert_eq!(duration_from_env_value(None, fallback), fallback);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn bound_daemon_reaps_after_live_session_set_stays_empty() {
        let directory = tempfile::tempdir().expect("create daemon test directory");
        let socket_path = directory.path().join("rmux.sock");
        let server = rmux_server::ServerDaemon::new(rmux_server::DaemonConfig::new(socket_path.clone()))
            .bind()
            .await
            .expect("bind test daemon");
        let rmux = daemon_rmux(&socket_path);

        tokio::time::timeout(
            Duration::from_secs(2),
            wait_until_sessions_idle(
                &rmux,
                &socket_path,
                Duration::from_millis(20),
                Duration::from_millis(10),
            ),
        )
        .await
        .expect("idle daemon should become reapable");
        server.shutdown().await.expect("shut down test daemon");

        assert!(!socket_path.exists(), "daemon shutdown should remove its socket");
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn monitor_finishes_when_last_hosted_session_removes_endpoint() {
        let directory = tempfile::tempdir().expect("create daemon test directory");
        let socket_path = directory.path().join("rmux.sock");
        let server = rmux_server::ServerDaemon::new(rmux_server::DaemonConfig::new(socket_path.clone()))
            .bind()
            .await
            .expect("bind test daemon");
        let rmux = daemon_rmux(&socket_path);
        let session = crate::shells::session::spawn_session(
            &rmux,
            crate::shells::session::SpawnSpec {
                name: rmux_sdk::SessionName::new("idle-reap-regression").expect("valid session name"),
                command: crate::shells::session::ShellCommand::Shell("sleep 60".to_owned()),
                working_directory: None,
                environment: Vec::new(),
                cols: crate::shells::session::DEFAULT_COLS,
                rows: crate::shells::session::DEFAULT_ROWS,
            },
        )
        .await
        .expect("spawn test session");
        assert!(session.kill().await.expect("kill test session"));

        let monitor_result = tokio::time::timeout(
            Duration::from_secs(2),
            wait_until_sessions_idle(
                &rmux,
                &socket_path,
                Duration::from_millis(20),
                Duration::from_millis(10),
            ),
        )
        .await;
        server.shutdown().await.expect("join stopped test daemon");

        assert!(
            monitor_result.is_ok(),
            "monitor should recognize rmux exit-empty endpoint removal"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn validate_socket_path_accepts_absolute_traversal_free_path() {
        assert!(validate_socket_path(Path::new("/tmp/basemind/shells/rmux.sock")).is_ok());
    }

    #[cfg(not(windows))]
    #[test]
    fn validate_socket_path_rejects_relative_path() {
        let err = validate_socket_path(Path::new("relative/evil.sock")).expect_err("relative path must be rejected");
        assert!(err.to_string().contains("must be absolute"), "{err}");
    }

    #[cfg(not(windows))]
    #[test]
    fn validate_socket_path_rejects_parent_dir_traversal() {
        let err =
            validate_socket_path(Path::new("/var/run/../../evil.sock")).expect_err("`..` component must be rejected");
        assert!(err.to_string().contains("must not contain `..`"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn validate_socket_path_accepts_named_pipe_path() {
        assert!(validate_socket_path(Path::new(r"\\.\pipe\basemind-shells-alice")).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn validate_socket_path_rejects_non_pipe_path() {
        let err = validate_socket_path(Path::new(r"C:\Windows\Temp\evil.sock"))
            .expect_err("a non-pipe path must be rejected on Windows");
        assert!(err.to_string().contains(r"\\.\pipe\"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn validate_socket_path_rejects_pipe_name_with_separator() {
        let err = validate_socket_path(Path::new(r"\\.\pipe\evil\..\escape"))
            .expect_err("a pipe name with a separator must be rejected");
        assert!(err.to_string().contains("separator"), "{err}");
    }

    #[test]
    fn parses_first_positional_as_socket_path() {
        let args = vec![OsString::from("/tmp/basemind-shells.sock")];
        assert_eq!(
            parse_socket_path(args),
            Some(PathBuf::from("/tmp/basemind-shells.sock"))
        );
    }

    #[test]
    fn skips_leading_config_flags_and_finds_socket() {
        let args = vec![OsString::from("/tmp/sock"), OsString::from("--config-quiet")];
        assert_eq!(parse_socket_path(args), Some(PathBuf::from("/tmp/sock")));
    }

    #[test]
    fn returns_none_when_only_flags_present() {
        let args = vec![OsString::from("--config-quiet")];
        assert_eq!(parse_socket_path(args), None);
    }
}
