//! The three agent tools of ADR-0021: `agent_post`, `agent_inbox` and `agent_threads`.
//!
//! Each tool is one function over [`AgentOps`](crate::comms::v2::AgentOps): it turns the flat
//! arguments into a request through the shared `from_fields` constructors, builds the
//! [`Caller`] from what the server holds about the session, calls the store, and shapes the
//! reply. Nothing here is registered with the router yet, and nothing here knows a transport.
//!
//! Who is calling never comes from the arguments. The principal, the repository and the replay
//! key are the session's ([`AgentSession`]); the only thing a caller chooses is `as`, which can
//! name a sub-identity of that principal and nothing else.
//!
//! The 4 KB cut of a body and the 16 KB response budget are the store's: a read that marks has
//! already advanced the read position past what it returned, so dropping a message here would
//! lose it. The tools report both, as `truncated` on a message and `more` on the reply.
//!
//! The tools are deferred, not always-loaded, and are therefore not in `VERB_TOOLS`; their
//! descriptions and schemas have a byte ceiling of their own, [`AGENT_TOOLS_BYTE_CEILING`].

mod inbox;
mod post;
mod threads;
mod types;

use rmcp::ErrorData as McpError;

use crate::comms::v2::api::{Caller, CommsError, DEFAULT_LIMIT, Principal, ReplayKey, RepoKey};
use crate::comms::v2::identity::compose;

pub use inbox::{AGENT_INBOX_DESCRIPTION, WaitingRead, run_agent_inbox};
pub use post::{AGENT_POST_DESCRIPTION, run_agent_post};
pub use threads::{AGENT_THREADS_DESCRIPTION, run_agent_threads};
pub use types::{
    AGENT_INBOX, AGENT_POST, AGENT_THREADS, AGENT_TOOLS, AgentInboxParams, AgentInboxResponse, AgentMessage,
    AgentPostParams, AgentPostResponse, AgentThread, AgentThreadsParams, AgentThreadsResponse,
};

/// Bytes the three descriptions and input schemas may take together (ADR-0021). The tools are
/// loaded on demand, so this is what a session pays once it coordinates with another agent.
pub const AGENT_TOOLS_BYTE_CEILING: usize = 5_000;

/// What the server holds about the session a call arrives on. None of it is read from a request.
#[derive(Clone, Copy, Debug)]
pub struct AgentSession<'a> {
    /// The identity the session resolved to.
    pub principal: &'a Principal,
    /// The repository the session works in.
    pub repo: &'a RepoKey,
    /// The relay's key for this request, when it came through a relay.
    pub replay_key: Option<&'a ReplayKey>,
    /// Notices only the tool layer knows, appended to the reply after the daemon's: the
    /// rejected-id notice on the session's first agent-tool reply, nothing afterwards.
    pub notices: &'a [String],
}

impl AgentSession<'_> {
    /// The caller of one `tool` call: the session's principal, or its sub-identity `as_`.
    fn caller(&self, tool: &str, as_: Option<&str>) -> Result<Caller, McpError> {
        let actor = compose(self.principal, as_)
            .map_err(|error| McpError::invalid_params(format!("`{tool}`: `as`: {error}"), None))?;
        let caller = Caller::new(actor, self.repo.clone());
        Ok(match self.replay_key {
            Some(key) => caller.with_replay_key(key.clone()),
            None => caller,
        })
    }

    /// The notices of a reply: the daemon's, then the tool layer's.
    fn notices(&self, mut from_reply: Vec<String>) -> Vec<String> {
        from_reply.extend(self.notices.iter().cloned());
        from_reply
    }
}

/// A refusal as the error a verb-tool body returns, which the shim reports as an `isError`
/// result. A [`CommsError`] is already worded as what to do next; the tool name is put in front.
fn refusal(tool: &str, error: &CommsError) -> McpError {
    let message = format!("`{tool}`: {error}");
    match error {
        CommsError::Backend { .. } | CommsError::StoreNewer { .. } => McpError::internal_error(message, None),
        _ => McpError::invalid_params(message, None),
    }
}

/// `limit` as the caller chose it: the default page size counts as the field left out, so that
/// passing it beside `id` or a change is not refused as a field that does not apply.
fn chosen_limit(limit: Option<u32>) -> Option<u32> {
    limit.filter(|limit| *limit != DEFAULT_LIMIT)
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
