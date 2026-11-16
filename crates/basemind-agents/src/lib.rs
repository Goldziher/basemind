#![deny(missing_docs)]

//! Launch a coding agent in another harness.
//!
//! One host (say, Claude Code) starts an agent in a different harness (Codex, Cursor, ...),
//! hands it a brief and an identity, and follows what it does. The identity is exported as
//! `BASEMIND_AGENT_ID` and, when the spec names an MCP server, basemind is injected into the
//! harness, so launcher and launched agent can talk over basemind's agent comms.
//!
//! This crate is the provider layer only: no MCP tool, no CLI, no daemon. A caller builds a
//! [`LaunchSpec`], gives it to a [`Registry`], and reads [`Event`]s from the [`Session`].
//!
//! # Safety defaults
//!
//! - **Approvals are denied.** Every approval request of the harness is answered `deny`
//!   unless the spec carries an [`ApprovalPolicy`].
//! - **No permission bypass.** [`PermissionMode::Bypass`], and any extra argument that would
//!   switch a harness's permission checks off, is refused unless the spec carries a
//!   [`BypassOptIn`] naming that harness and its mode.
//! - **Confined working directory.** It has to resolve inside [`LaunchSpec::root`].
//! - **Limits.** At the wall-time limit, or when the harness starts more turns than
//!   [`Limits::max_turns`], the whole process group is killed.
//! - **Allow-listed environment.** The child receives [`ENV_ALLOWLIST`], the spec's additions
//!   and the identity. Credentials in the launcher's environment do not reach it.
//!
//! # Harnesses
//!
//! Claude Code (`stream-json`) and Codex (`app-server`) are driven by adapters derived from
//! [monocode](https://github.com/hardbeat920/monocode) (MIT, see `ATTRIBUTIONS.md`). Agents
//! that speak the Agent Client Protocol go through the `agent-client-protocol` crate.
//!
//! The core is synchronous: threads and channels, no async runtime. The ACP connection runs
//! on `futures::executor::block_on` inside the session's own thread.

mod acp;
mod claude;
mod codex;
mod error;
mod event;
mod frames;
mod guard;
mod harness;
mod identity;
mod line;
mod pipe;
mod process;
mod queue;
mod registry;
mod session;
mod spec;
#[cfg(test)]
mod test_support;

pub use acp::AcpAdapter;
pub use claude::ClaudeCodeAdapter;
pub use codex::CodexAdapter;
pub use error::LaunchError;
pub use event::{ApprovalKind, ApprovalRequest, ApprovalSource, Event, FinishStatus, ToolCall, ToolState};
pub use harness::{AcpFlavor, Harness, HarnessKind};
pub use identity::{AgentIdentity, ENV_AGENT_ID, ENV_HOST};
pub use registry::{Adapter, Registry};
pub use session::{CommandPlan, Session};
pub use spec::{
    ApprovalPolicy, ApprovalVerdict, BypassOptIn, ENV_ALLOWLIST, LaunchSpec, Limits, McpServerSpec, PermissionMode,
};
