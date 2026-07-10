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
//! [`rmux_server::DaemonConfig`] with config-file loading disabled and no web
//! frontend, then bind + wait on a dedicated tokio runtime. The trailing config
//! flags the SDK passes are intentionally ignored — basemind always runs the
//! daemon with `ConfigFileSelection::Disabled` and no web port.

use std::ffi::OsString;
#[cfg(not(windows))]
use std::path::Component;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

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
/// forwards. basemind ignores those trailing flags and always runs with config
/// loading disabled and no web frontend.
///
/// This builds its own multi-thread tokio runtime (the daemon owns the process
/// at this point — `main` has not yet parsed clap and never will) and blocks on
/// `bind().await` then `wait().await`.
pub fn run_internal_daemon<I>(args: I) -> Result<()>
where
    I: IntoIterator<Item = OsString>,
{
    let socket_path = parse_socket_path(args).context("the embedded rmux daemon requires a socket path argument")?;
    validate_socket_path(&socket_path)?;

    let config = rmux_server::DaemonConfig::new(socket_path);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for embedded rmux daemon")?;

    runtime.block_on(async move {
        let server = rmux_server::ServerDaemon::new(config)
            .bind()
            .await
            .context("bind embedded rmux daemon socket")?;
        server.wait().await.context("embedded rmux daemon wait loop")?;
        Ok::<(), anyhow::Error>(())
    })
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
