//! Session operations over the rmux SDK.
//!
//! Thin, typed wrappers around the verified rmux-sdk 0.6.1 surface:
//! [`rmux_sdk::EnsureSession`] to create a detached headless session,
//! [`rmux_sdk::Session::pane`] / [`rmux_sdk::Pane`] to drive stdin + capture
//! output, and [`rmux_sdk::Rmux::list_sessions`] / [`rmux_sdk::Session::kill`]
//! for lifecycle. Errors are surfaced as [`anyhow::Error`] so callers (the MCP
//! helpers) can map them to MCP errors at the boundary.

use anyhow::{Context, Result};
use rmux_sdk::{EnsureSession, Rmux, Session, SessionName, TerminalSizeSpec};

/// Default terminal geometry for a headless session. Wide enough that typical
/// command output is not wrapped, tall enough to hold a screenful for snapshot
/// capture. Headless sessions have no attached client driving a resize, so this
/// is the geometry the pane keeps for its whole life.
pub(crate) const DEFAULT_COLS: u16 = 200;
/// See [`DEFAULT_COLS`].
pub(crate) const DEFAULT_ROWS: u16 = 50;

/// How a shell session's program is specified.
///
/// `Shell` runs the string through the login shell (rmux's `ProcessCommandSpec::Shell`);
/// `Argv` execs the argument vector directly with no shell interpretation.
#[derive(Debug, Clone)]
pub enum ShellCommand {
    /// Run `command` via the login shell, e.g. `bash -lc '<command>'`.
    Shell(String),
    /// Exec this argument vector directly with no shell interpretation; the first
    /// element is the program and the rest are its arguments.
    Argv(Vec<String>),
}

/// Inputs for spawning one detached headless shell session.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// The rmux session name to create (already minted + sanitized by the caller).
    pub name: SessionName,
    /// The program to run in the session's initial pane.
    pub command: ShellCommand,
    /// Optional working directory for the spawned process.
    pub working_directory: Option<String>,
    /// Environment overrides as `"KEY=VALUE"` strings.
    pub environment: Vec<String>,
}

/// Create a detached headless session per `spec` and return the live handle.
///
/// The session is created with `detached(true)` so no client is attached — it
/// runs purely under the daemon. The pane geometry is fixed at
/// `DEFAULT_COLS`×`DEFAULT_ROWS`.
pub async fn spawn_session(rmux: &Rmux, spec: SpawnSpec) -> Result<Session> {
    let mut ensure = EnsureSession::named(spec.name)
        .detached(true)
        .size(TerminalSizeSpec::new(DEFAULT_COLS, DEFAULT_ROWS));

    ensure = match spec.command {
        ShellCommand::Shell(command) => ensure.shell(command),
        ShellCommand::Argv(argv) => ensure.argv(argv),
    };

    if let Some(cwd) = spec.working_directory {
        ensure = ensure.working_directory(cwd);
    }
    if !spec.environment.is_empty() {
        ensure = ensure.environment(spec.environment);
    }

    ensure
        .ensure(rmux)
        .await
        .context("create detached rmux session")
}

/// Send `text` to the session's primary pane.
///
/// When `enter` is true a trailing newline is appended so the shell executes the
/// line. Targets the first pane of the first window (`pane(0, 0)`).
pub async fn send_text(session: &Session, text: &str, enter: bool) -> Result<()> {
    let pane = session.pane(0, 0);
    let payload = if enter {
        format!("{text}\n")
    } else {
        text.to_string()
    };
    pane.send_text(payload)
        .await
        .context("send text to rmux pane")
}

/// Capture the currently-visible text of the session's primary pane.
///
/// Returns the rendered screen as a single newline-joined string. When `lines`
/// is supplied, only the last `lines` non-leading-blank rows are returned (the
/// most recent output), so callers can ask for a tail rather than the whole
/// screen. Trailing all-blank rows are always trimmed.
pub async fn capture(session: &Session, lines: Option<usize>) -> Result<String> {
    let pane = session.pane(0, 0);
    let snapshot = pane
        .snapshot()
        .await
        .context("snapshot rmux pane for capture")?;
    let mut rows: Vec<String> = snapshot.visible_lines();

    // Drop trailing blank rows — a 50-row pane running a short command is mostly
    // empty, and returning 45 blank lines is pure noise.
    while rows.last().is_some_and(|row| row.trim().is_empty()) {
        rows.pop();
    }

    if let Some(tail) = lines {
        let start = rows.len().saturating_sub(tail);
        rows.drain(..start);
    }

    Ok(rows.join("\n"))
}

/// List the names of all sessions currently known to the daemon.
pub async fn list_sessions(rmux: &Rmux) -> Result<Vec<SessionName>> {
    rmux.list_sessions().await.context("list rmux sessions")
}

/// Kill `session`. Returns `true` when a session existed and was terminated,
/// `false` when it was already gone.
pub async fn kill_session(session: &Session) -> Result<bool> {
    session.kill().await.context("kill rmux session")
}
