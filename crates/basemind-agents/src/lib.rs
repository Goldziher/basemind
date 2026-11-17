#![deny(missing_docs)]

//! Launch a coding agent in another harness.
//!
//! One host (say, Claude Code) starts an agent in a different harness (Codex, Cursor, ...),
//! hands it a brief and an identity, and follows what it does. The identity is exported as
//! `BASEMIND_AGENT_ID` and, when the config names an MCP server, basemind is injected into the
//! harness, so launcher and launched agent can talk over basemind's agent comms.
//!
//! This crate is the provider layer only: no MCP tool, no CLI, no daemon. A launch has two
//! halves. The [`LaunchConfig`] is trusted: it comes from the operator's configuration and
//! names the executable, the root, the MCP server, the environment values, the opt-ins and the
//! caps. The [`LaunchRequest`] is what a model may choose: the brief, a working directory
//! inside the root, the model and effort, and limits within the caps. A caller hands both to
//! a [`Registry`] and reads [`Event`]s from the [`Session`].
//!
//! # Safety defaults
//!
//! What the crate enforces, against hostile model-chosen fields (the request) and a hostile
//! child alike. The config is trusted:
//!
//! - **Approvals are denied.** Every approval request of the harness is answered `deny`
//!   unless the config carries an [`ApprovalPolicy`]. A request shows the grant itself (the
//!   command, the file changes, the permissions, the tool input) as an [`ApprovalKind`] and the
//!   raw wire params; an agent's stated reason is only appended. A Codex file change carries
//!   the item's full changes, or is denied unasked when they are unknown or over 1 MiB; a
//!   Codex MCP tool call goes to the policy too. An ACP allow takes the `allow_once` option
//!   only. Requests get ids of the crate's own; at most 16 wait for the caller, a further one
//!   is denied, and two sharing a wire id are both denied.
//! - **No permission bypass.** [`PermissionMode::Bypass`] is refused unless the config carries
//!   a [`BypassOptIn`] naming that harness and its mode.
//! - **No unverified mode without consent.** An ACP agent in any other mode needs an
//!   [`UnverifiedModeOptIn`] naming the harness and the mode, since what it does unasked was
//!   not verified. OpenCode supervised needs none: every OpenCode launch is configured to ask
//!   for every permission. An ACP agent that switches its own session mode fails the session.
//! - **No free-form arguments.** A request names a model and an effort, which the adapter
//!   turns into that harness's flags after checking their grammar; nothing else of a request
//!   reaches the command line, its limits are clamped to the config's caps, and its brief is
//!   at most 1 MiB and may not start with `/`, which a harness runs as a slash command.
//! - **No project configuration.** Claude Code is asked for no setting sources, hooks disabled
//!   and only the MCP server the config names, and the session fails unless its `system/init`
//!   frame reports the requested permission mode, exactly that server as loaded from
//!   `--mcp-config` and only built-in plugins (verified on Claude Code 2.1.325). Codex, Gemini,
//!   Cursor and OpenCode are not launched over a project file of theirs (`.codex`, `.gemini`,
//!   ...) in the working directory or above it. Only MCP tools the config names are
//!   pre-approved, and only Claude Code can pre-approve.
//! - **Executables.** The harness executable and the MCP server's command are resolved to
//!   absolute paths outside the root, with no link on the way inside it; the child's `PATH`
//!   keeps only absolute entries outside the root.
//! - **Working directory.** It is relative, has to resolve inside [`LaunchConfig::root`], and
//!   is checked again in the child after it changes into it. The agent is not sandboxed to
//!   the root.
//! - **Allow-listed environment.** The child receives [`ENV_ALLOWLIST`], the config's additions
//!   and the identity. An addition must be a provider credential (`*_API_KEY`, `*_TOKEN`) or
//!   one of a few provider settings, and may not redirect configuration, code loading, an
//!   endpoint or basemind. Credentials in the launcher's environment do not reach the child.
//! - **Limits.** At the wall-time limit, or when the harness starts more turns than
//!   [`Limits::max_turns`], the process group is killed. A stop, and a session that failed,
//!   get [`Limits::stop_grace`].
//! - **Bounded resources.** A frame is at most 16 MiB and 256 Ki JSON values, the event
//!   queue holds at most 1024 events and 16 MiB, and every channel and table is bounded; an
//!   ACP agent that leaves the client's lines unread fails the session. Dropping a session
//!   waits at most 2 s for its supervisor, even when a descendant holds the harness's pipes.
//!
//! What it does not do: the config's executable, its MCP server's command and its
//! environment values are used as given, so the configuration has to restrict them; the
//! harnesses' user-level configuration still applies; a descendant that leaves the harness's
//! process group survives the session (the session still ends on time), as OpenCode's own
//! server does; a project file written during a session is only seen by the next launch;
//! `BASEMIND_AGENT_ID` is self-asserted; what the ACP agents' modes other than OpenCode's
//! supervised one ask for was not run against a live harness; the launcher must not reap
//! children it did not start and must ignore SIGPIPE; and nothing is launched off Unix. The
//! README lists what was verified against which harness version.
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
mod gitconfig;
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
    ApprovalPolicy, ApprovalVerdict, BypassOptIn, ENV_ALLOWLIST, LaunchConfig, LaunchRequest, Limits, McpServerSpec,
    PermissionMode, UnverifiedModeOptIn,
};
