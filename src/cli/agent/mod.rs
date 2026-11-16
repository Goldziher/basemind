//! `basemind agent` — the CLI half of `agent_post`, `agent_inbox` and `agent_threads` (ADR-0021).
//!
//! Each command is one `run_*` function over the [`AgentOps`](crate::comms::v2::AgentOps) seam: it
//! turns its flags into the request the tool of the same name builds, through the one field check
//! the tools, the CLI and the hooks share ([`crate::comms::v2::fields`]), calls the operation and
//! renders the reply. Nothing here opens a store or reaches a daemon, so the commands run against
//! any implementation, the test store included.
//!
//! Who is calling is the [`Session`](crate::comms::v2::front::Session) the binary resolved; a
//! command chooses only `--as`. [`run`] resolves it, connects to the daemon and dispatches.
//!
//! Layout:
//! - `args.rs` — the clap grammar.
//! - `post.rs` / `inbox.rs` / `threads.rs` — one command each.
//! - `render.rs` — text and JSON output.
//! - `hook.rs` — `basemind agent hook`, what the plugin hooks ask of the binary.
//! - `run.rs` — the binary's entry: identity, daemon link, dispatch.

pub mod args;
pub mod hook;
pub mod inbox;
pub mod post;
pub mod render;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod run;
pub mod threads;

#[cfg(all(test, feature = "comms", any(unix, windows)))]
mod test_support;

use std::io::Write;

use anyhow::Result;
use serde::Serialize;

use crate::cli::exit::CliExit;
use crate::comms::v2::front::{self, Session};
use crate::comms::v2::{Caller, CommsError};

pub use crate::comms::v2::front::WaitingRead;
pub use args::{AgentCmd, CleanupArgs, InboxArgs, PostArgs, ThreadsArgs};
pub use inbox::{inbox_request, run_inbox};
pub use post::{post_request, run_post};
pub use render::Render;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub use run::run;
pub use threads::{run_threads, threads_request};

/// The caller a command acts as: the session's principal, or its sub-identity `--as`. A refused
/// name is a usage error (exit `2`).
fn caller(session: &Session<'_>, command: &str, as_name: Option<&str>) -> Result<Caller> {
    session
        .caller(as_name)
        .map_err(|error| CliExit::usage(front::as_refusal(command, &error).message))
}

/// A refusal of the flags themselves, before any operation ran: a usage error (exit `2`).
pub fn refused(command: &str, error: &CommsError) -> anyhow::Error {
    CliExit::usage(front::refusal(command, error).message)
}

/// A refusal by the operation: a runtime error (exit `1`). The message is the refusal's own, which
/// already says what to do next.
pub fn failed(command: &str, error: &CommsError) -> anyhow::Error {
    anyhow::anyhow!(front::refusal(command, error).message)
}

/// Render the report of a command that has no MCP tool (`status`, `cleanup`). The operation
/// behind either is the daemon's and not part of [`AgentOps`](crate::comms::v2::AgentOps), so the
/// caller runs it and hands over the outcome.
pub fn run_report<T: Serialize>(
    command: &str,
    report: Result<T, CommsError>,
    opts: &Render,
    out: &mut dyn Write,
) -> Result<()> {
    let report = report.map_err(|error| failed(command, &error))?;
    render::report(&report, opts, out)
}

#[cfg(all(test, feature = "comms", any(unix, windows)))]
#[path = "mod_tests.rs"]
mod tests;
