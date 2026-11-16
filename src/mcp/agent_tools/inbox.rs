//! `agent_inbox`: read messages, bodies inline, marking them read unless told otherwise.

use rmcp::ErrorData as McpError;

use super::types::{AGENT_INBOX, AgentInboxParams, AgentInboxResponse};
use super::{AgentSession, WaitingRead, caller, refusal};
use crate::comms::v2::api::{AgentOps, Caller, CommsError, InboxCall, InboxFields, InboxReply};
use crate::comms::v2::front::{inbox_call, inbox_read};

/// What an agent reads to decide whether `agent_inbox` is the tool it wants.
pub const AGENT_INBOX_DESCRIPTION: &str = "Use to read messages other agents sent you, bodies inline. No arguments \
     returns your unread messages from every thread and marks them read; `peek` leaves them unread, `unread:false` \
     rereads history, `id` returns one whole message. `more:true` means call again; a body over 4 KB is cut \
     (`truncated`) and read whole by `id`. \
     Example: {\"wait_secs\":30}";

fn refuse(error: CommsError) -> McpError {
    inbox_refusal(&error)
}

/// Body of `agent_inbox`.
///
/// A call without `wait_secs`, or with 0, is answered by `ops` at once and `waiting_read` is not
/// called. Refused before either is asked: an invalid `as`, `wait_secs` over 40, `thread` beside
/// `id`, any of `unread`, `peek`, `limit`, `cursor` or `wait_secs` beside `id`, `wait_secs`
/// beside `unread: false`, and `cursor` on a read that marks. The announcing read is not the
/// tool's: the hooks ask for it through the CLI.
///
/// The messages are the store's, uncut and undropped here: a read that marks has already moved
/// the read position past every one of them.
pub fn run_agent_inbox(
    ops: &dyn AgentOps,
    session: &AgentSession<'_>,
    params: AgentInboxParams,
    waiting_read: &WaitingRead<'_>,
) -> Result<AgentInboxResponse, McpError> {
    let (caller, call) = prepare_agent_inbox(session, params)?;
    let reply = inbox_read(ops, &caller, call, waiting_read).map_err(refuse)?;
    Ok(finish_agent_inbox(session, reply))
}

/// The caller and the read of one `agent_inbox` call, refused as [`run_agent_inbox`] refuses
/// them. For a front end that makes the read itself, such as the daemon's async wait.
pub fn prepare_agent_inbox(
    session: &AgentSession<'_>,
    params: AgentInboxParams,
) -> Result<(Caller, InboxCall), McpError> {
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
        false,
    )
    .map_err(refuse)?;
    Ok((caller, call))
}

/// The tool's answer to a read [`prepare_agent_inbox`] prepared, or its refusal.
pub fn finish_agent_inbox(session: &AgentSession<'_>, mut reply: InboxReply) -> AgentInboxResponse {
    let notices = session.notices(std::mem::take(&mut reply.notices));
    AgentInboxResponse::from_reply(reply, notices)
}

/// A refused read, worded for `agent_inbox`.
pub fn inbox_refusal(error: &CommsError) -> McpError {
    refusal(AGENT_INBOX, error)
}

#[cfg(all(test, feature = "comms", any(unix, windows)))]
#[path = "inbox_tests.rs"]
mod tests;
