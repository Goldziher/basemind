//! Session operations over the rmux SDK.
//!
//! Thin, typed wrappers around the verified rmux-sdk 0.6.1 surface:
//! [`rmux_sdk::EnsureSession`] to create a detached headless session,
//! [`rmux_sdk::Session::pane`] / [`rmux_sdk::Pane`] to drive stdin + capture
//! output, and [`rmux_sdk::Rmux::list_sessions`] / [`rmux_sdk::Session::kill`]
//! for lifecycle. Errors are surfaced as [`anyhow::Error`] so callers (the MCP
//! helpers) can map them to MCP errors at the boundary.

use anyhow::{Context, Result};
use rmux_sdk::{EnsureSession, Input, Pane, PaneProcessState, Rmux, RmuxError, Session, SessionName, TerminalSizeSpec};

/// Fallback terminal geometry for a headless session. Wide enough that typical
/// command output is not wrapped, tall enough to hold a screenful for snapshot
/// capture. Headless sessions have no attached client driving a resize, so this
/// is the geometry the pane keeps for its whole life.
///
/// The configured `[shells].default_cols` normally supplies the width; this const
/// is the floor applied when a caller passes `0`, so a degenerate zero-sized PTY
/// can never reach the daemon. The config default mirrors this value.
pub(crate) const DEFAULT_COLS: u16 = 200;
/// See [`DEFAULT_COLS`]. The configured `[shells].default_rows` normally supplies
/// the height; this const is the floor applied when a caller passes `0`.
pub(crate) const DEFAULT_ROWS: u16 = 50;

/// Maximum number of non-blank output rows one capture call may return.
pub(crate) const MAX_CAPTURE_LINES: usize = 500;

const CAPTURE_SCAN_FACTOR: usize = 8;
const MAX_CAPTURE_SCAN_ROWS: usize = 1024;
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

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
    /// Terminal width in columns for the headless pane. Sourced from `[shells].default_cols` so an
    /// operator can widen the geometry; `DEFAULT_COLS` is the fallback the config default mirrors.
    pub cols: u16,
    /// Terminal height in rows for the headless pane. Sourced from `[shells].default_rows`;
    /// `DEFAULT_ROWS` is the fallback the config default mirrors.
    pub rows: u16,
}

/// Create a detached headless session per `spec` and return the live handle.
///
/// The session is created with `detached(true)` so no client is attached — it
/// runs purely under the daemon. The pane geometry is taken from `spec.cols` ×
/// `spec.rows` (the configured `[shells]` defaults), falling back to
/// `DEFAULT_COLS` × `DEFAULT_ROWS` at the config layer.
pub async fn spawn_session(rmux: &Rmux, spec: SpawnSpec) -> Result<Session> {
    let cols = if spec.cols == 0 { DEFAULT_COLS } else { spec.cols };
    let rows = if spec.rows == 0 { DEFAULT_ROWS } else { spec.rows };
    let mut ensure = EnsureSession::named(spec.name)
        .detached(true)
        .size(TerminalSizeSpec::new(cols, rows));

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

    ensure.ensure(rmux).await.context("create detached rmux session")
}

/// Send `text` to the session's primary pane.
///
/// When `enter` is true a trailing newline is appended so the shell executes the
/// line. Targets the first pane of the first window (`pane(0, 0)`).
pub async fn send_text(session: &Session, text: &str, enter: bool) -> Result<()> {
    let pane = session.pane(0, 0);
    let payload = if enter { format!("{text}\n") } else { text.to_string() };
    pane.send_text(payload).await.context("send text to rmux pane")
}

/// Capture the most recent rendered output from the session's primary pane.
///
/// Reads retained pane history so a completed one-line command remains visible
/// to a fresh CLI or MCP client. Leading/trailing blank rows and rmux's terminal
/// synthetic dead-pane row are omitted while interior spacing is preserved;
/// `lines` caps the returned non-blank rows at 500 and defaults to 50 rows.
pub async fn capture(session: &Session, lines: Option<usize>) -> Result<String> {
    if lines.is_some_and(|line_count| line_count > MAX_CAPTURE_LINES) {
        anyhow::bail!("capture lines exceeds maximum of {MAX_CAPTURE_LINES}");
    }
    let requested_lines = lines.unwrap_or(DEFAULT_ROWS as usize);
    if requested_lines == 0 {
        return Ok(String::new());
    }
    let scan_rows = requested_lines
        .saturating_mul(CAPTURE_SCAN_FACTOR)
        .saturating_add(DEFAULT_ROWS as usize + 1)
        .min(MAX_CAPTURE_SCAN_ROWS);
    let pane = session.pane(0, 0);
    let captured = pane
        .capture_pane()
        .start(-(scan_rows as i64))
        .await
        .context("capture retained rmux pane output")?;
    let output_start = captured.stdout.len().saturating_sub(MAX_CAPTURE_BYTES);
    let output = String::from_utf8_lossy(&captured.stdout[output_start..]);
    Ok(render_capture_rows(&output, requested_lines))
}

fn is_dead_pane_status(row: &str) -> bool {
    (row.starts_with("Pane is dead (status ") || row.starts_with("Pane is dead (signal ")) && row.ends_with(')')
}

fn render_capture_rows(output: &str, requested_lines: usize) -> String {
    let mut rows: Vec<&str> = output.lines().map(str::trim_end).collect();
    let first_nonblank = rows.iter().position(|row| !row.trim().is_empty()).unwrap_or(rows.len());
    rows.drain(..first_nonblank);
    while rows.last().is_some_and(|row| row.trim().is_empty()) {
        rows.pop();
    }
    if rows.last().is_some_and(|row| is_dead_pane_status(row)) {
        rows.pop();
        while rows.last().is_some_and(|row| row.trim().is_empty()) {
            rows.pop();
        }
    }

    let mut remaining = requested_lines;
    let start = rows
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, row)| {
            if row.trim().is_empty() {
                return None;
            }
            remaining = remaining.saturating_sub(1);
            (remaining == 0).then_some(index)
        })
        .unwrap_or(0);
    rows[start..].join("\n")
}

async fn inspect_session_liveness(rmux: &Rmux, names: &[SessionName]) -> Result<Vec<(SessionName, bool)>> {
    let panes = rmux.find_panes().all().await.context("inspect rmux pane liveness")?;
    Ok(names
        .iter()
        .cloned()
        .map(|name| {
            let alive = process_states_show_live(
                panes
                    .iter()
                    .filter(|pane| pane.session_name == name)
                    .map(|pane| &pane.process),
            );
            (name, alive)
        })
        .collect())
}

/// List the names of all sessions currently known to the daemon.
pub async fn list_sessions(rmux: &Rmux) -> Result<Vec<SessionName>> {
    rmux.list_sessions().await.context("list rmux sessions")
}

/// List retained sessions together with whether any of their panes may still be running.
///
/// `Unknown` is treated as live so a transient recovery snapshot cannot reap an active daemon.
/// A retained session is inactive only when every discovered pane has explicitly exited.
pub async fn list_session_liveness(rmux: &Rmux) -> Result<Vec<(SessionName, bool)>> {
    let names = list_sessions(rmux).await?;
    match inspect_session_liveness(rmux, &names).await {
        Ok(sessions) => Ok(sessions),
        Err(error) => {
            tracing::warn!(
                error = %error,
                session_count = names.len(),
                "shell_list: pane liveness unavailable; reporting known sessions conservatively live"
            );
            Ok(names.into_iter().map(|name| (name, true)).collect())
        }
    }
}

/// Inspect session liveness without substituting conservative user-facing values.
pub(crate) async fn list_session_liveness_strict(rmux: &Rmux) -> Result<Vec<(SessionName, bool)>> {
    let names = list_sessions(rmux).await?;
    inspect_session_liveness(rmux, &names).await
}

fn process_states_show_live<'a>(states: impl Iterator<Item = &'a PaneProcessState>) -> bool {
    let mut seen = false;
    let any_may_be_live = states
        .inspect(|_| seen = true)
        .any(|state| !matches!(state, PaneProcessState::Exited));
    any_may_be_live || !seen
}

/// Broadcast `text` to the primary pane of each named session at once.
///
/// Resolves every `SessionName` to its first pane (`pane(0, 0)`), then delivers
/// the same input to all of them via [`Rmux::broadcast`]. When `enter` is true a
/// trailing newline is appended so each shell executes the line (matching
/// [`send_text`]). Returns the number of panes that accepted the input.
///
/// A partial failure (some panes rejected the input) is surfaced as an error that
/// reports how many of the targeted panes succeeded versus failed, so the caller
/// learns the broadcast was not fully delivered rather than silently losing it.
pub async fn broadcast(rmux: &Rmux, names: &[SessionName], text: &str, enter: bool) -> Result<usize> {
    if names.is_empty() {
        return Ok(0);
    }

    let mut panes: Vec<Pane> = Vec::with_capacity(names.len());
    for name in names {
        let session = rmux
            .session(name.clone())
            .await
            .with_context(|| format!("open session {:?} for broadcast", name.as_str()))?;
        panes.push(session.pane(0, 0));
    }

    let payload = if enter { format!("{text}\n") } else { text.to_string() };

    match rmux.broadcast(&panes, Input::text(&payload)).await {
        Ok(result) => Ok(result.len()),
        Err(RmuxError::PartialBroadcast { source, .. }) => {
            let delivered = source.successes().len();
            let failed = source.failures().len();
            Err(anyhow::anyhow!(
                "broadcast partially failed: {delivered} of {} panes accepted the input, \
                 {failed} rejected it",
                delivered + failed
            ))
        }
        Err(other) => Err(anyhow::Error::new(other).context("broadcast input to rmux panes")),
    }
}

/// Kill `session`. Returns `true` when a session existed and was terminated,
/// `false` when it was already gone.
pub async fn kill_session(session: &Session) -> Result<bool> {
    session.kill().await.context("kill rmux session")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_unknown_pane_state_is_conservatively_live() {
        assert!(process_states_show_live(std::iter::empty()));
        assert!(process_states_show_live([PaneProcessState::Unknown].iter()));
    }

    #[test]
    fn session_is_inactive_only_when_every_observed_pane_exited() {
        assert!(!process_states_show_live([PaneProcessState::Exited].iter()));
        assert!(process_states_show_live(
            [PaneProcessState::Exited, PaneProcessState::Running { pid: None }].iter()
        ));
    }

    #[test]
    fn dead_pane_status_detection_does_not_hide_normal_output() {
        assert!(is_dead_pane_status("Pane is dead (status 0, Fri Aug  7 07:31:03 2026)"));
        assert!(is_dead_pane_status(
            "Pane is dead (signal 15, Fri Aug  7 07:31:03 2026)"
        ));
        assert!(!is_dead_pane_status("command printed: Pane is dead (status 0)"));
    }

    #[test]
    fn capture_rows_preserve_interior_blank_lines() {
        let output = "first\n\nsecond\nPane is dead (status 0, now)\n";
        assert_eq!(render_capture_rows(output, 2), "first\n\nsecond");
    }

    #[test]
    fn capture_rows_only_filter_a_terminal_dead_pane_status() {
        let output = "Pane is dead (status 0, now)\nstill user output\n";
        assert_eq!(render_capture_rows(output, 2), output.trim_end());
    }
}
