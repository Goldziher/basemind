//! `basemind agent inbox` — the CLI half of `agent_inbox`.

use std::io::Write;
use std::time::Duration;

use anyhow::Result;

use super::args::InboxArgs;
use super::render::{self, Render};
use super::{acting_as, failed, refused};
use crate::comms::v2::{AgentOps, Caller, CommsError, InboxBody, InboxCall, InboxFields, InboxReply};

/// The command's name in an error message.
const COMMAND: &str = "agent inbox";

/// A read that may wait: make `InboxBody` for `Caller`, waiting up to the given time for a message
/// when nothing is unread, and report how the wait ended in the reply.
///
/// [`AgentOps::inbox`] never blocks and the wait belongs to the daemon, so the command is handed
/// the daemon's waiting read and does not wait by itself.
pub type WaitingRead<'a> = dyn Fn(&Caller, InboxBody, Duration) -> Result<InboxReply, CommsError> + 'a;

/// The read `args` ask for and the caller making it. Refuses what `agent_inbox` refuses, a
/// `--wait-secs` over the maximum included.
pub fn inbox_request(caller: &Caller, args: InboxArgs) -> Result<(Caller, InboxCall), CommsError> {
    let caller = acting_as(caller, args.as_name.as_deref())?;
    let call = InboxCall::from_fields(InboxFields {
        thread: args.thread,
        id: args.id,
        unread: args.unread,
        peek: args.peek.then_some(true),
        wait_secs: args.wait_secs,
        limit: args.limit,
        cursor: args.cursor,
    })?;
    Ok((caller, call))
}

/// Read messages and print them. A read that asks for no wait goes to `ops`; one that asks for a
/// wait goes to `wait`, whole.
pub fn run_inbox(
    ops: &dyn AgentOps,
    caller: &Caller,
    args: InboxArgs,
    wait: &WaitingRead<'_>,
    opts: &Render,
    out: &mut dyn Write,
) -> Result<()> {
    let (caller, call) = inbox_request(caller, args).map_err(|error| refused(COMMAND, &error))?;
    let reply = if call.wait.is_zero() {
        ops.inbox(&caller, call.body)
    } else {
        wait(&caller, call.body, call.wait)
    };
    let reply = reply.map_err(|error| failed(COMMAND, &error))?;
    render::inbox(&reply, opts, out)
}

#[cfg(test)]
#[path = "inbox_tests.rs"]
mod tests;
