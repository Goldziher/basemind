//! Agent comms v2 (ADR-0021): stable identity, repository-open threads, three verb tools.
//!
//! This module is the shared contract only. [`api`] holds the ids, requests, replies, limits and
//! refusals that the daemon, the `agent_post` / `agent_inbox` / `agent_threads` tools, the CLI and
//! the hooks will all use, and [`AgentOps`] is the seam between them. Nothing here is wired into
//! the daemon, the wire protocol or the MCP surface yet: the v1 `agents` and `workspace` tools
//! still run on the types in the parent module, and the two share no state.
//!
//! The module compiles without the `comms` feature, because it depends on no store and no
//! transport; the store-backed implementation that replaces v1 will be gated like the rest.

pub mod api;
/// In-memory reference implementation, for tests of anything built on [`AgentOps`].
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
pub mod ids;

pub use api::{
    Actor, AgentOps, CommsError, IdKind, InboxMessage, InboxReply, InboxRequest, InboxSelect, InvalidId, Mark,
    MessageId, PostReply, PostRequest, PostTarget, Principal, ReplayKey, RepoKey, SubName, ThreadAction, ThreadId,
    ThreadQuery, ThreadSummary, ThreadUpdate, ThreadsReply, WaitOutcome,
};
#[cfg(any(test, feature = "test-support"))]
pub use fake::FakeAgents;
