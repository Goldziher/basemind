//! `basemind agent inbox` — the CLI half of `agent_inbox`.

use std::io::Write;

use anyhow::Result;

use super::args::InboxArgs;
use super::render::{self, Render};
use super::{WaitingRead, caller, failed, refused};
use crate::comms::v2::front::{Session, inbox_call, inbox_read};
use crate::comms::v2::{AgentOps, CommsError, InboxCall, InboxFields};

/// The command's name in an error message.
const COMMAND: &str = "agent inbox";

/// The read `args` ask for. Refuses what `agent_inbox` refuses, a `--wait-secs` over the maximum
/// included.
pub fn inbox_request(args: InboxArgs) -> Result<InboxCall, CommsError> {
    inbox_call(
        InboxFields {
            thread: args.thread,
            id: args.id,
            unread: args.unread,
            peek: args.peek.then_some(true),
            wait_secs: args.wait_secs,
            limit: args.limit,
            cursor: args.cursor,
        },
        args.announce,
    )
}

/// Read messages and print them. A read that asks for no wait goes to `ops`; one that asks for a
/// wait goes to `wait`, whole.
pub fn run_inbox(
    ops: &dyn AgentOps,
    session: &Session<'_>,
    mut args: InboxArgs,
    wait: &WaitingRead<'_>,
    opts: &Render,
    out: &mut dyn Write,
) -> Result<()> {
    let caller = caller(session, COMMAND, args.as_name.take().as_deref())?;
    let call = inbox_request(args).map_err(|error| refused(COMMAND, &error))?;
    let mut reply = inbox_read(ops, &caller, call, wait).map_err(|error| failed(COMMAND, &error))?;
    reply.notices = session.notices(std::mem::take(&mut reply.notices));
    render::inbox(&reply, opts, out)
}

#[cfg(test)]
#[path = "inbox_tests.rs"]
mod tests;
