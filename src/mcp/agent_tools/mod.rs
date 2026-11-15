//! The three agent tools of ADR-0021: `agent_post`, `agent_inbox` and `agent_threads`.
//!
//! Each tool is one function over [`AgentOps`](crate::comms::v2::AgentOps): it turns the flat
//! arguments into a request and builds the [`Caller`] from what the server holds about the
//! session, both through [`front`](crate::comms::v2::front), which the CLI uses as well, calls the
//! store, and shapes the reply. Nothing here is registered with the router yet, and nothing here knows a transport.
//!
//! Who is calling never comes from the arguments. The principal, the repository and the replay
//! key are the session's ([`AgentSession`]); the only thing a caller chooses is `as`, which can
//! name a sub-identity of that principal and nothing else.
//!
//! The 4 KiB cut of a body and the 16 KiB response budget are the store's: a read that marks has
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

use crate::comms::v2::api::{Caller, CommsError};
use crate::comms::v2::front::{self, Refusal};

pub use crate::comms::v2::front::{Session as AgentSession, WaitingRead};
pub use inbox::{AGENT_INBOX_DESCRIPTION, run_agent_inbox};
pub use post::{AGENT_POST_DESCRIPTION, run_agent_post};
pub use threads::{AGENT_THREADS_DESCRIPTION, run_agent_threads};
pub use types::{
    AGENT_INBOX, AGENT_POST, AGENT_THREADS, AGENT_TOOLS, AgentInboxParams, AgentInboxResponse, AgentMessage,
    AgentPostParams, AgentPostResponse, AgentThread, AgentThreadsParams, AgentThreadsResponse,
};

/// Bytes the three descriptions and input schemas may take together (ADR-0021). The tools are
/// loaded on demand, so this is what a session pays once it coordinates with another agent.
pub const AGENT_TOOLS_BYTE_CEILING: usize = 5_000;

fn into_error(refusal: Refusal) -> McpError {
    if refusal.store_failed {
        McpError::internal_error(refusal.message, None)
    } else {
        McpError::invalid_params(refusal.message, None)
    }
}

/// How a tool is named in front of a refusal.
fn surface(tool: &str) -> String {
    format!("`{tool}`")
}

/// The caller of one `tool` call: the session's principal, or its sub-identity `as_`.
fn caller(session: &AgentSession<'_>, tool: &str, as_: Option<&str>) -> Result<Caller, McpError> {
    session
        .caller(as_)
        .map_err(|error| into_error(front::as_refusal(&surface(tool), &error)))
}

/// A refusal as the error a verb-tool body returns, which the shim reports as an `isError`
/// result. A store that failed is an internal error; anything else is the caller's to fix.
fn refusal(tool: &str, error: &CommsError) -> McpError {
    into_error(front::refusal(&surface(tool), error))
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
