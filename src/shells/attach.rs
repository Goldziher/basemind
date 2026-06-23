//! Embedded visual-attach entry point.
//!
//! basemind has no external `rmux` binary, so the visual launcher cannot shell
//! out to `rmux attach`. Instead, the per-OS launchers in [`super::launcher`]
//! build a command that re-execs basemind itself with the hidden
//! [`INTERNAL_ATTACH_FLAG`] (`--__internal-attach`), which this module intercepts
//! at the very top of `main` (before clap parses) and routes to the blocking rmux
//! attach driver.
//!
//! Re-exec shape:
//! `basemind --__internal-attach <session-name> --socket <abs-path> --size <COLS>x<ROWS>`
//!
//! The driver connects to the daemon socket over the blocking rmux client,
//! begins an attach to the named session, and hands the upgraded stream to
//! `rmux_client::attach_terminal_with_initial_bytes`, which owns the terminal
//! (raw termios + SIGWINCH) until a clean detach / EOF.
//!
//! The whole module is gated on `feature = "shells"`.

use std::ffi::OsString;

use anyhow::{Context, Result, bail};

/// Hidden flag that marks a basemind re-exec as the visual-attach driver.
///
/// Mirrors `rmux_client::INTERNAL_DAEMON_FLAG` for the daemon re-exec. The leading
/// `--__internal` prefix keeps it out of the documented CLI surface (clap never
/// sees it — [`intercept_from_env`] consumes it first).
pub const INTERNAL_ATTACH_FLAG: &str = "--__internal-attach";

/// `--socket` flag introducing the absolute daemon socket path.
const SOCKET_FLAG: &str = "--socket";
/// `--size` flag introducing the initial terminal geometry (`COLSxROWS`).
const SIZE_FLAG: &str = "--size";
/// Fallback terminal columns when `--size` is missing or malformed.
const FALLBACK_COLS: u16 = 200;
/// Fallback terminal rows when `--size` is missing or malformed.
const FALLBACK_ROWS: u16 = 50;

/// Parsed arguments of a `--__internal-attach` re-exec.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachArgs {
    /// rmux session name to attach to.
    session_name: String,
    /// Absolute, traversal-free daemon socket path.
    socket_path: std::path::PathBuf,
    /// Initial terminal width (falls back to [`FALLBACK_COLS`] on a malformed size).
    cols: u16,
    /// Initial terminal height (falls back to [`FALLBACK_ROWS`] on a malformed size).
    rows: u16,
}

/// Inspect the process arguments and, when basemind was re-execed as the visual
/// attach driver, run the attach and return its result.
///
/// The launcher starts an attach by re-execing basemind with the hidden
/// [`INTERNAL_ATTACH_FLAG`] (`--__internal-attach`) as the first real argument,
/// followed by `<session-name> --socket <abs-path> --size <COLS>x<ROWS>`. When
/// that flag is present this returns `Some(run_internal_attach(...))`; otherwise
/// it returns `None` and the caller proceeds (to the daemon intercept, then
/// normal CLI parsing).
///
/// Called at the very top of `main`, before clap parses, so the attach process
/// never sees basemind's CLI surface.
#[must_use]
pub fn intercept_from_env() -> Option<Result<()>> {
    let mut args = std::env::args_os();
    let _argv0 = args.next();
    match args.next() {
        Some(first) if first == INTERNAL_ATTACH_FLAG => Some(run_internal_attach(args)),
        _ => None,
    }
}

/// Parse the post-flag argument list, validate it, and drive the attach.
fn run_internal_attach<I>(args: I) -> Result<()>
where
    I: IntoIterator<Item = OsString>,
{
    let parsed = parse_attach_args(args)?;
    drive_attach(&parsed)
}

/// Parse `<session-name> --socket <abs-path> --size <COLS>x<ROWS>` into [`AttachArgs`].
///
/// The session name is the first positional argument; `--socket` is required and
/// validated as absolute + traversal-free via
/// [`crate::shells::daemon::validate_socket_path`]; the session name is validated
/// through `rmux_sdk::SessionName::new`. `--size` is parsed defensively — a
/// missing or malformed size falls back to [`FALLBACK_COLS`]x[`FALLBACK_ROWS`]
/// rather than failing or panicking.
fn parse_attach_args<I>(args: I) -> Result<AttachArgs>
where
    I: IntoIterator<Item = OsString>,
{
    let mut session_name: Option<String> = None;
    let mut socket_path: Option<std::path::PathBuf> = None;
    let mut cols = FALLBACK_COLS;
    let mut rows = FALLBACK_ROWS;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == SOCKET_FLAG {
            let value = iter
                .next()
                .context("--socket requires an absolute daemon socket path")?;
            socket_path = Some(std::path::PathBuf::from(value));
        } else if arg == SIZE_FLAG {
            // Defensive: a malformed size never aborts the attach — it falls back to
            // the default geometry. The terminal resizes to the real size via SIGWINCH
            // inside `attach_terminal_with_initial_bytes` regardless.
            if let Some(value) = iter.next()
                && let Some((c, r)) = parse_size(&value.to_string_lossy())
            {
                cols = c;
                rows = r;
            }
        } else if !arg.as_encoded_bytes().starts_with(b"--") && session_name.is_none() {
            session_name = Some(arg.to_string_lossy().into_owned());
        }
    }

    let session_name =
        session_name.context("the visual attach requires a session name argument")?;
    let socket_path = socket_path.context("the visual attach requires a --socket path")?;

    crate::shells::daemon::validate_socket_path(&socket_path)?;
    // Validate the session name through the same constructor the spawn path uses, so a
    // malformed name fails here rather than at the daemon boundary.
    rmux_sdk::SessionName::new(session_name.clone())
        .map_err(|e| anyhow::anyhow!("invalid rmux session name {session_name:?}: {e}"))?;

    Ok(AttachArgs {
        session_name,
        socket_path,
        cols,
        rows,
    })
}

/// Parse a `COLSxROWS` geometry token into `(u16, u16)`. Returns `None` for any
/// malformed input (no `x`, empty halves, non-numeric, overflow).
fn parse_size(raw: &str) -> Option<(u16, u16)> {
    let (cols, rows) = raw.split_once('x')?;
    let cols: u16 = cols.parse().ok()?;
    let rows: u16 = rows.parse().ok()?;
    Some((cols, rows))
}

/// Connect to the daemon, begin the attach, and hand the upgraded stream to the
/// blocking terminal driver.
///
/// Uses the NON-resize-geometry attach variant
/// (`attach_terminal_with_initial_bytes`): SIGWINCH + raw termios are managed
/// inside the driver, and skipping the explicit resize handshake avoids the
/// daemon-closes-stream hazard. The driver blocks until a clean detach / EOF,
/// which we map to `Ok(())`. The pre-parsed `cols`/`rows` are advisory — they
/// seed the initial geometry but the driver immediately re-syncs to the real TTY
/// size, so they are not forwarded into the resize handshake here.
#[cfg(unix)]
fn drive_attach(parsed: &AttachArgs) -> Result<()> {
    let _ = (parsed.cols, parsed.rows);
    let name = rmux_sdk::SessionName::new(parsed.session_name.clone())
        .map_err(|e| anyhow::anyhow!("invalid rmux session name: {e}"))?;
    let connection = rmux_client::connect(&parsed.socket_path).with_context(|| {
        format!(
            "connect to embedded rmux daemon at {:?}",
            parsed.socket_path
        )
    })?;
    match connection
        .begin_attach(name)
        .context("begin attach to rmux session")?
    {
        rmux_client::AttachTransition::Upgraded(upgrade) => {
            let (stream, initial) = upgrade.into_parts();
            // Blocks (owning the terminal: raw termios + SIGWINCH) until detach / EOF,
            // which is the clean-exit path mapped to `Ok(())`.
            rmux_client::attach_terminal_with_initial_bytes(stream, initial)
                .context("attach terminal to rmux session")?;
            Ok(())
        }
        rmux_client::AttachTransition::Rejected(_) => {
            bail!(
                "attach rejected by the daemon: no such session {:?} (it may have exited)",
                parsed.session_name
            )
        }
    }
}

/// Non-unix stub: the visual attach is unix-only (the rmux attach path is gated on
/// unix). On other platforms the re-exec should never be produced, but if it is,
/// fail loudly rather than silently succeeding.
#[cfg(not(unix))]
fn drive_attach(_parsed: &AttachArgs) -> Result<()> {
    bail!("the embedded visual attach is only supported on unix")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_valid_attach_args_with_size() {
        let parsed = parse_attach_args(args(&[
            "bmsh-1-2",
            "--socket",
            "/tmp/basemind/shells/rmux.sock",
            "--size",
            "120x40",
        ]))
        .expect("valid args parse");
        assert_eq!(parsed.session_name, "bmsh-1-2");
        assert_eq!(
            parsed.socket_path,
            std::path::PathBuf::from("/tmp/basemind/shells/rmux.sock")
        );
        assert_eq!(parsed.cols, 120);
        assert_eq!(parsed.rows, 40);
    }

    #[test]
    fn falls_back_to_default_size_when_size_is_malformed() {
        let parsed = parse_attach_args(args(&[
            "bmsh-1-2",
            "--socket",
            "/tmp/rmux.sock",
            "--size",
            "not-a-size",
        ]))
        .expect("malformed size must not fail the parse");
        assert_eq!(parsed.cols, FALLBACK_COLS);
        assert_eq!(parsed.rows, FALLBACK_ROWS);
    }

    #[test]
    fn falls_back_to_default_size_when_size_flag_is_absent() {
        let parsed = parse_attach_args(args(&["bmsh-1-2", "--socket", "/tmp/rmux.sock"]))
            .expect("missing size is fine");
        assert_eq!(parsed.cols, FALLBACK_COLS);
        assert_eq!(parsed.rows, FALLBACK_ROWS);
    }

    #[test]
    fn rejects_missing_socket() {
        let err = parse_attach_args(args(&["bmsh-1-2", "--size", "80x24"]))
            .expect_err("missing --socket must be rejected");
        assert!(err.to_string().contains("--socket"), "{err}");
    }

    #[test]
    fn rejects_missing_session_name() {
        let err = parse_attach_args(args(&["--socket", "/tmp/rmux.sock"]))
            .expect_err("missing session name must be rejected");
        assert!(err.to_string().contains("session name"), "{err}");
    }

    #[test]
    fn rejects_relative_socket_path() {
        let err = parse_attach_args(args(&["bmsh-1-2", "--socket", "relative/evil.sock"]))
            .expect_err("relative socket must be rejected");
        assert!(err.to_string().contains("must be absolute"), "{err}");
    }

    #[test]
    fn rejects_socket_path_with_parent_traversal() {
        let err = parse_attach_args(args(&["bmsh-1-2", "--socket", "/var/run/../../evil.sock"]))
            .expect_err("`..` socket must be rejected");
        assert!(err.to_string().contains("must not contain `..`"), "{err}");
    }

    #[test]
    fn rejects_empty_session_name() {
        // `--socket` consumes its own value; an empty positional name is rejected by
        // `SessionName::new` (EmptySessionName). Here we drive it via the missing-name
        // path: only flags present, no positional, so the name is absent.
        let err = parse_attach_args(args(&["", "--socket", "/tmp/rmux.sock"]))
            .expect_err("empty session name must be rejected by SessionName::new");
        assert!(err.to_string().contains("session name"), "{err}");
    }

    #[test]
    fn parse_size_handles_valid_and_invalid() {
        assert_eq!(parse_size("200x50"), Some((200, 50)));
        assert_eq!(parse_size("1x1"), Some((1, 1)));
        assert_eq!(parse_size("200"), None);
        assert_eq!(parse_size("x50"), None);
        assert_eq!(parse_size("200x"), None);
        assert_eq!(parse_size("axb"), None);
        assert_eq!(parse_size("999999x10"), None); // u16 overflow
    }
}
