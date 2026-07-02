//! Headless agent-shell subcommands: 1:1 with the `shell_*` MCP tools.
//!
//! Each handler builds the matching `Shell*Params` struct and dispatches to the
//! identical `#[tool]` method on the in-process [`BasemindServer`]. Sessions are
//! backed by the embedded rmux daemon (an external process basemind re-execs
//! itself as), so a session spawned from one CLI invocation survives the process
//! exit and is addressable from the next — the same daemon a running `serve`
//! shares. Gated on `feature = "shells"`.

use std::io::Write;

use anyhow::{Result, anyhow};
use clap::Subcommand;

use crate::mcp::BasemindServer;
use crate::mcp::params::*;

use super::render::emit;
use super::run_tool;

#[derive(Subcommand, Debug)]
pub enum ShellsCmd {
    /// Spawn a detached headless shell session and print its `session_id`.
    Spawn {
        /// Command line to run in the session's initial pane (via the login shell).
        command: String,
        /// Repository-relative working directory (forward-slash, no leading `/`).
        #[arg(long)]
        cwd: Option<String>,
        /// Environment override in `KEY=VALUE` form. Repeatable.
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Advisory human-readable session title.
        #[arg(long)]
        title: Option<String>,
    },
    /// Write text to a session's stdin (a trailing newline is appended unless `--no-enter`).
    Send {
        /// The `session_id` returned by `spawn`.
        session_id: String,
        /// Text to write to the session's stdin.
        text: String,
        /// Send the text as a raw keystroke fragment without a trailing newline.
        #[arg(long)]
        no_enter: bool,
    },
    /// Capture the visible screen of a session.
    Capture {
        /// The `session_id` returned by `spawn`.
        session_id: String,
        /// Return only the last N non-blank lines (omit for the whole visible screen).
        #[arg(long)]
        lines: Option<usize>,
    },
    /// Kill a session.
    Kill {
        /// The `session_id` returned by `spawn`.
        session_id: String,
    },
    /// Write the same text to several sessions' stdin at once.
    Broadcast {
        /// Text to write to each session's stdin.
        text: String,
        /// Target `session_id`s. Repeatable; every id must be a live session.
        #[arg(long = "session", value_name = "SESSION_ID", required = true)]
        session_ids: Vec<String>,
        /// Send the text as a raw keystroke fragment without a trailing newline.
        #[arg(long)]
        no_enter: bool,
    },
    /// List the sessions this server has spawned, with liveness.
    List,
}

/// Parse a `KEY=VALUE` override into a [`ShellEnv`]. The value may itself contain
/// `=`; only the first `=` splits the pair.
fn parse_env(raw: &str) -> Result<ShellEnv> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| anyhow!("invalid --env {raw:?}: expected KEY=VALUE"))?;
    Ok(ShellEnv {
        key: key.to_string(),
        value: value.to_string(),
    })
}

pub async fn run(
    server: &BasemindServer,
    cmd: ShellsCmd,
    json: bool,
    out: &mut impl Write,
) -> Result<()> {
    match cmd {
        ShellsCmd::Spawn {
            command,
            cwd,
            env,
            title,
        } => {
            let env = if env.is_empty() {
                None
            } else {
                Some(env.iter().map(|e| parse_env(e)).collect::<Result<_>>()?)
            };
            let p = ShellSpawnParams {
                command,
                cwd: cwd.map(|c| c.as_str().into()),
                env,
                title,
            };
            let r = run_tool("shell_spawn", server.shell_spawn(Parameters(p)).await)?;
            emit("shell_spawn", &r, json, out)
        }
        ShellsCmd::Send {
            session_id,
            text,
            no_enter,
        } => {
            let p = ShellSendParams {
                session_id,
                text,
                enter: !no_enter,
            };
            let r = run_tool("shell_send", server.shell_send(Parameters(p)).await)?;
            emit("shell_send", &r, json, out)
        }
        ShellsCmd::Capture { session_id, lines } => {
            let p = ShellCaptureParams { session_id, lines };
            let r = run_tool("shell_capture", server.shell_capture(Parameters(p)).await)?;
            emit("shell_capture", &r, json, out)
        }
        ShellsCmd::Kill { session_id } => {
            let p = ShellKillParams { session_id };
            let r = run_tool("shell_kill", server.shell_kill(Parameters(p)).await)?;
            emit("shell_kill", &r, json, out)
        }
        ShellsCmd::Broadcast {
            text,
            session_ids,
            no_enter,
        } => {
            let p = ShellBroadcastParams {
                session_ids,
                text,
                enter: !no_enter,
            };
            let r = run_tool(
                "shell_broadcast",
                server.shell_broadcast(Parameters(p)).await,
            )?;
            emit("shell_broadcast", &r, json, out)
        }
        ShellsCmd::List => {
            let p = ShellListParams {};
            let r = run_tool("shell_list", server.shell_list(Parameters(p)).await)?;
            emit("shell_list", &r, json, out)
        }
    }
}
