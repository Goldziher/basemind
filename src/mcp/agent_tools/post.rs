//! `agent_post`: post a message, creating the thread when the target is a new one.

use rmcp::ErrorData as McpError;

use super::types::{AGENT_POST, AgentPostParams, AgentPostResponse};
use super::{AgentSession, caller, refusal};
use crate::comms::v2::api::{AgentOps, CommsError};
use crate::comms::v2::front::{PostFields, post_body};

/// What an agent reads to decide whether `agent_post` is the tool it wants.
pub const AGENT_POST_DESCRIPTION: &str = "Use to message other agents working in this repository. Needs `body` and \
     exactly one of `subject` (start a thread), `thread` (post into one) or `reply_to` (answer a message). A new \
     thread is open to the repository unless `to` names its members. \
     Example: {\"subject\":\"Refactoring src/auth\",\"body\":\"Taking src/auth/token.rs; tell me if you are in \
     it.\",\"paths\":[\"src/auth\"]}";

fn refuse(error: CommsError) -> McpError {
    refusal(AGENT_POST, &error)
}

/// Body of `agent_post`.
///
/// Refused before the store is asked: an invalid `as`, an empty or oversized `body`, none or
/// several of `thread` / `reply_to` / `subject`, `to` or `paths` without `subject`, and a
/// malformed id or path.
pub fn run_agent_post(
    ops: &dyn AgentOps,
    session: &AgentSession<'_>,
    params: AgentPostParams,
) -> Result<AgentPostResponse, McpError> {
    let caller = caller(session, AGENT_POST, params.as_.as_deref())?;
    let post = post_body(PostFields {
        body: params.body,
        thread: params.thread,
        reply_to: params.reply_to,
        subject: params.subject,
        to: params.to.unwrap_or_default(),
        paths: params.paths.unwrap_or_default(),
    })
    .map_err(refuse)?;
    let mut reply = ops.post(&caller, post).map_err(refuse)?;
    let notices = session.notices(std::mem::take(&mut reply.notices));
    Ok(AgentPostResponse::from_reply(reply, notices))
}

#[cfg(test)]
#[path = "post_tests.rs"]
mod tests;
