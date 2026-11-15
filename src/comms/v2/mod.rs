//! Agent comms v2 (ADR-0021): stable identity, repository-open threads, three verb tools.
//!
//! [`api`] holds the requests, replies, limits and rules that the daemon, the `agent_post` /
//! `agent_inbox` / `agent_threads` tools, the CLI and the hooks will all use; [`ids`], [`error`],
//! [`cursor`] and [`fields`] are its parts, and [`AgentOps`] is the seam between a front end and
//! a store. Nothing here is wired into the daemon, the wire protocol or the MCP surface yet: the
//! v1 `agents` and `workspace` tools still run on the types in the parent module, and the two
//! share no state.
//!
//! The contract compiles without the `comms` feature, because it depends on no store and no
//! transport. [`rules`] holds the decisions every implementation shares, and `store`, gated
//! like the rest of the daemon, is the Fjall-backed implementation that replaces v1.

pub mod api;
/// The ADR's rules as scenarios, for any [`AgentOps`] implementation to be run against.
#[cfg(any(test, feature = "test-support"))]
pub mod conformance;
pub mod cursor;
pub mod error;
/// In-memory reference implementation, for tests of anything built on [`AgentOps`].
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
pub mod fields;
/// What the tools and the CLI do the same way: the session, the requests, the refusal wording.
pub mod front;
pub mod identity;
pub mod ids;
/// The rules every [`AgentOps`] implementation shares, as pure functions.
pub mod rules;
/// The Fjall-backed [`AgentOps`]: the store the daemon will serve from.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod store;

pub use api::{
    Actor, AgentOps, Caller, CommsError, Cursor, IdKind, InboxBody, InboxCall, InboxFields, InboxMessage, InboxReply,
    InboxSelect, InvalidId, Mark, MessageId, PostBody, PostReply, PostTarget, Principal, ReadKind, ReplayKey, RepoKey,
    SubName, ThreadAction, ThreadFilter, ThreadId, ThreadQuery, ThreadSummary, ThreadUpdate, ThreadsCall,
    ThreadsFields, ThreadsReply, WaitOutcome,
};
#[cfg(any(test, feature = "test-support"))]
pub use conformance::ConformanceHarness;
#[cfg(any(test, feature = "test-support"))]
pub use fake::FakeAgents;
