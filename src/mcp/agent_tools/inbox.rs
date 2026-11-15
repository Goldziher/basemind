//! `agent_inbox`: read messages, bodies inline, marking them read unless told otherwise.

use rmcp::ErrorData as McpError;

use super::types::{AGENT_INBOX, AgentInboxParams, AgentInboxResponse};
use super::{AgentSession, WaitingRead, caller, refusal};
use crate::comms::v2::api::{AgentOps, CommsError, InboxFields};
use crate::comms::v2::front::{inbox_call, inbox_read};

/// What an agent reads to decide whether `agent_inbox` is the tool it wants.
pub const AGENT_INBOX_DESCRIPTION: &str = "Use to read messages other agents sent you, bodies inline. No arguments \
     returns your unread messages from every thread and marks them read; `peek` leaves them unread, `unread:false` \
     rereads history, `id` returns one whole message. `more:true` means call again; a body over 4 KB is cut \
     (`truncated`) and read whole by `id`. \
     Example: {\"wait_secs\":30}";

fn refuse(error: CommsError) -> McpError {
    refusal(AGENT_INBOX, &error)
}

/// Body of `agent_inbox`.
///
/// A call without `wait_secs`, or with 0, is answered by `ops` at once and `waiting_read` is not
/// called. Refused before either is asked: an invalid `as`, `wait_secs` over 40, `thread` beside
/// `id`, any of `unread`, `peek`, `announce`, `limit`, `cursor` or `wait_secs` beside `id`,
/// `wait_secs` beside `unread: false`, `peek`, `unread: false` or `wait_secs` beside `announce`,
/// and `cursor` on a read that marks or announces.
///
/// The messages are the store's, uncut and undropped here: a read that marks has already moved
/// the read position past every one of them.
pub fn run_agent_inbox(
    ops: &dyn AgentOps,
    session: &AgentSession<'_>,
    params: AgentInboxParams,
    waiting_read: &WaitingRead<'_>,
) -> Result<AgentInboxResponse, McpError> {
    let caller = caller(session, AGENT_INBOX, params.as_.as_deref())?;
    let call = inbox_call(
        InboxFields {
            thread: params.thread,
            id: params.id,
            unread: params.unread,
            peek: params.peek,
            wait_secs: params.wait_secs,
            limit: params.limit,
            cursor: params.cursor,
        },
        params.announce.unwrap_or(false),
    )
    .map_err(refuse)?;
    let mut reply = inbox_read(ops, &caller, call, waiting_read).map_err(refuse)?;
    let notices = session.notices(std::mem::take(&mut reply.notices));
    Ok(AgentInboxResponse::from_reply(reply, notices))
}

#[cfg(test)]
#[path = "inbox_tests.rs"]
mod tests;
