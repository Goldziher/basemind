//! `agent_inbox`: read messages, bodies inline, marking them read unless told otherwise.

use std::time::Duration;

use rmcp::ErrorData as McpError;

use super::types::{AGENT_INBOX, AgentInboxParams, AgentInboxResponse};
use super::{AgentSession, chosen_limit, refusal};
use crate::comms::v2::api::{AgentOps, Caller, CommsError, InboxBody, InboxCall, InboxFields, InboxReply, InboxSelect};

/// What an agent reads to decide whether `agent_inbox` is the tool it wants.
pub const AGENT_INBOX_DESCRIPTION: &str = "Use to read messages other agents sent you, bodies inline. No arguments \
     returns your unread messages from every thread and marks them read; `peek` leaves them unread, `unread:false` \
     rereads history, `id` returns one whole message. `more:true` means call again; a body over 4 KB is cut \
     (`truncated`) and read whole by `id`. \
     Example: {\"wait_secs\":30}";

/// A read that waits: the daemon's side of `wait_secs`.
///
/// [`AgentOps::inbox`] never blocks, and the tool body neither sleeps nor spawns. When a call asks
/// to wait, the checked read and the wait (above zero, at most 40 seconds) are handed to this
/// hook, which owns everything a wait needs: it registers the replay key as in flight, watches
/// [`AgentOps::arrivals`], checks that the client is still connected before the read that marks,
/// and sets [`InboxReply::wait`] to how the wait ended.
pub type WaitingRead<'a> = dyn Fn(&Caller, InboxBody, Duration) -> Result<InboxReply, CommsError> + 'a;

fn refuse(error: CommsError) -> McpError {
    refusal(AGENT_INBOX, &error)
}

/// Body of `agent_inbox`.
///
/// A call without `wait_secs`, or with 0, is answered by `ops` at once and `waiting_read` is not
/// called. Refused before either is asked: an invalid `as`, `wait_secs` over 40, `thread` beside
/// `id`, any of `unread`, `peek`, `limit`, `cursor` or `wait_secs` beside `id`, `wait_secs`
/// beside `unread: false`, and `cursor` on a read that marks.
pub fn run_agent_inbox(
    ops: &dyn AgentOps,
    session: &AgentSession<'_>,
    params: AgentInboxParams,
    waiting_read: &WaitingRead<'_>,
) -> Result<AgentInboxResponse, McpError> {
    let caller = session.caller(AGENT_INBOX, params.as_.as_deref())?;
    let InboxCall { body, wait } = InboxCall::from_fields(InboxFields {
        thread: params.thread,
        id: params.id,
        unread: params.unread,
        peek: params.peek,
        wait_secs: params.wait_secs,
        limit: chosen_limit(params.limit),
        cursor: params.cursor,
    })
    .map_err(refuse)?;
    let whole = matches!(body.select, InboxSelect::Message(_));
    let read = if wait.is_zero() {
        ops.inbox(&caller, body)
    } else {
        waiting_read(&caller, body, wait)
    };
    let mut reply = read.map_err(refuse)?;
    let notices = session.notices(std::mem::take(&mut reply.notices));
    Ok(AgentInboxResponse::from_reply(reply, whole, notices))
}

#[cfg(test)]
#[path = "inbox_tests.rs"]
mod tests;
