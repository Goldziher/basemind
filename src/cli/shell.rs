//! `basemind shell` — the CLI half of the `shell` domain.
//!
//! Real clap subcommands rather than a `--mode` flag, so each operation keeps its own `--help` and
//! its own argument validation; they map one-to-one onto the MCP `shell` tool's [`ShellMode`]
//! values, which is what `tests/cli_parity.rs` asserts.
//!
//! Sessions are backed by the embedded rmux daemon (an external process basemind re-execs itself
//! as), so a session spawned from one CLI invocation survives the process exit and is addressable
//! from the next — the same daemon a running `serve` shares. Gated on `feature = "shells"`.
//!
//! Each handler leaves every field its mode does not use `None`: the helper rejects a field
//! belonging to another mode, so populating them blindly would fail the call.

use std::io::Write;

use anyhow::{Result, anyhow};
use clap::Subcommand;

use crate::mcp::BasemindServer;
use crate::mcp::params::*;

use super::render::{Emit, emit};
use super::run_tool;

#[derive(Subcommand, Debug)]
pub enum ShellCmd {
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
    /// List every session the shell daemon currently hosts, with liveness.
    List,
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

pub async fn run(server: &BasemindServer, cmd: ShellCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let p = match cmd {
        ShellCmd::Spawn {
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
            ShellParams {
                command: Some(command),
                cwd: cwd.map(|c| c.as_str().into()),
                env,
                title,
                ..ShellParams::new(ShellMode::Spawn)
            }
        }
        ShellCmd::Send {
            session_id,
            text,
            no_enter,
        } => ShellParams {
            session_id: Some(session_id),
            text: Some(text),
            enter: Some(!no_enter),
            ..ShellParams::new(ShellMode::Send)
        },
        ShellCmd::Capture { session_id, lines } => ShellParams {
            session_id: Some(session_id),
            lines,
            ..ShellParams::new(ShellMode::Capture)
        },
        ShellCmd::Kill { session_id } => ShellParams {
            session_id: Some(session_id),
            ..ShellParams::new(ShellMode::Kill)
        },
        ShellCmd::List => ShellParams::new(ShellMode::List),
        ShellCmd::Broadcast {
            text,
            session_ids,
            no_enter,
        } => ShellParams {
            session_ids: Some(session_ids),
            text: Some(text),
            enter: Some(!no_enter),
            ..ShellParams::new(ShellMode::Broadcast)
        },
    };

    let key = p.mode.telemetry_key();
    let r = run_tool(key, server.shell(Parameters(Lenient(p))).await)?;
    emit(key, &r, opts, out)
}
