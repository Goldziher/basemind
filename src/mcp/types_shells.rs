//! Param and response types for the headless agent-shell MCP tools.
//!
//! These drive the embedded rmux daemon (see [`crate::shells`]): spawn a
//! detached headless shell session, send stdin, capture the visible output, and
//! kill it. The whole module is gated on `feature = "shells"`.
//!
//! Split into its own file to keep `types.rs` under the 1000-line cap.

#![cfg(feature = "shells")]

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::path::RelPath;

/// One environment-variable override for a spawned shell, in `KEY` / `VALUE` form.
///
/// Modelled as a struct (rather than a raw `"K=V"` string) so the schema is
/// self-documenting and the values are not re-parsed.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ShellEnv {
    /// Environment variable name.
    pub key: String,
    /// Environment variable value.
    pub value: String,
}

/// Parameters for `shell_spawn`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ShellSpawnParams {
    /// Command line to run in the session's initial pane, interpreted by the
    /// login shell (e.g. `bash -lc '<command>'`). Required.
    pub command: String,
    /// Optional repository-relative working directory for the spawned process.
    /// Forward-slash separated, no leading `/`.
    #[serde(default)]
    pub cwd: Option<RelPath>,
    /// Optional environment-variable overrides applied to the spawned process.
    #[serde(default)]
    pub env: Option<Vec<ShellEnv>>,
    /// Optional human-readable title for the session (advisory; not used for
    /// addressing — use the returned `session_id`).
    #[serde(default)]
    pub title: Option<String>,
}

/// Response from `shell_spawn`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ShellSpawnResponse {
    /// Stable basemind-minted identifier for the spawned session. Pass this to
    /// `shell_send` / `shell_capture` / `shell_kill`.
    pub session_id: String,
    /// A `rmux attach -t <name>` command an operator can run in a terminal to
    /// attach to (observe) the otherwise-headless session.
    pub attach_command: String,
}

/// Parameters for `shell_send`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ShellSendParams {
    /// The `session_id` returned by `shell_spawn`.
    pub session_id: String,
    /// Text to write to the session's stdin.
    pub text: String,
    /// When `true` (default), a trailing newline is appended so the line is
    /// executed. Set `false` to send a raw keystroke fragment without a return.
    #[serde(default = "default_true")]
    pub enter: bool,
}

fn default_true() -> bool {
    true
}

/// Parameters for `shell_capture`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ShellCaptureParams {
    /// The `session_id` returned by `shell_spawn`.
    pub session_id: String,
    /// Optional cap on how many trailing (most-recent) non-blank lines of the
    /// visible screen to return. Omit to return the whole visible screen.
    #[serde(default)]
    pub lines: Option<usize>,
}

/// Response from `shell_capture`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ShellCaptureResponse {
    /// The captured visible screen text (trailing blank lines trimmed).
    pub text: String,
}

/// Parameters for `shell_kill`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ShellKillParams {
    /// The `session_id` returned by `shell_spawn`.
    pub session_id: String,
}

/// Response from `shell_kill`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ShellKillResponse {
    /// The `session_id` that was targeted.
    pub session_id: String,
    /// `true` when a live session was terminated, `false` when it was already
    /// gone (already exited or never existed).
    pub killed: bool,
}

/// Parameters for `shell_broadcast`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ShellBroadcastParams {
    /// The `session_id`s (from `shell_spawn`) to deliver `text` to. Every id must
    /// be a known, live session of this server; an unknown id fails the whole
    /// broadcast without sending to any pane.
    pub session_ids: Vec<String>,
    /// Text to write to each session's stdin.
    pub text: String,
    /// When `true` (default), a trailing newline is appended so each line is
    /// executed. Set `false` to send a raw keystroke fragment without a return.
    #[serde(default = "default_true")]
    pub enter: bool,
}

/// Response from `shell_broadcast`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ShellBroadcastResponse {
    /// The number of session panes that accepted the input.
    pub delivered: usize,
}

/// Parameters for `shell_list`. Takes no arguments.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ShellListParams {}

/// One session in a `shell_list` response.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ShellSessionView {
    /// The basemind-minted `session_id` for this session.
    pub session_id: String,
    /// The underlying rmux session name.
    pub name: String,
    /// `true` when the daemon still reports this session as live, `false` when it
    /// has exited but the mapping has not been forgotten yet.
    pub alive: bool,
}

/// Response from `shell_list`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ShellListResponse {
    /// The sessions this server spawned, each flagged with its liveness.
    pub sessions: Vec<ShellSessionView>,
}
