//! `agent_threads`: list, discover and describe threads, or make one change to one thread.

use rmcp::ErrorData as McpError;

use super::types::{AGENT_THREADS, AgentThreadsParams, AgentThreadsResponse};
use super::{AgentSession, caller, refusal};
use crate::comms::v2::api::{AgentOps, CommsError, ThreadsCall, ThreadsFields};

/// What an agent reads to decide whether `agent_threads` is the tool it wants.
pub const AGENT_THREADS_DESCRIPTION: &str = "Use to see which agent threads exist and who is in them, or to change \
     membership. No arguments lists your threads with unread counts and peers; `query` or `path` also finds open \
     threads of this repository; `thread` alone describes one. With `thread`, at most one of `join`, `leave`, \
     `invite`, `archive` changes it. \
     Example: {\"thread\":\"t-abcdef\",\"join\":true}";

fn refuse(error: CommsError) -> McpError {
    refusal(AGENT_THREADS, &error)
}

/// Body of `agent_threads`.
///
/// Refused before the store is asked: an invalid `as`, more than one of `join`, `leave`, `invite`
/// and `archive`, one of them without `thread`, a listing field (`query`, `path`, `cursor`,
/// `archived`, `limit`) beside `thread`, an empty `invite`, a `query` under three characters, and
/// a malformed id or path.
pub fn run_agent_threads(
    ops: &dyn AgentOps,
    session: &AgentSession<'_>,
    params: AgentThreadsParams,
) -> Result<AgentThreadsResponse, McpError> {
    let caller = caller(session, AGENT_THREADS, params.as_.as_deref())?;
    let call = ThreadsCall::from_fields(ThreadsFields {
        query: params.query,
        path: params.path,
        thread: params.thread,
        join: params.join.unwrap_or(false),
        leave: params.leave.unwrap_or(false),
        invite: params.invite,
        archive: params.archive.unwrap_or(false),
        archived: params.archived.unwrap_or(false),
        limit: params.limit,
        cursor: params.cursor,
    })
    .map_err(refuse)?;
    let answered = match call {
        ThreadsCall::Query(query) => ops.threads_query(&caller, query),
        ThreadsCall::Update(update) => ops.threads_update(&caller, update),
    };
    let mut reply = answered.map_err(refuse)?;
    let notices = session.notices(std::mem::take(&mut reply.notices));
    Ok(AgentThreadsResponse::from_reply(reply, notices))
}

#[cfg(test)]
#[path = "threads_tests.rs"]
mod tests;
