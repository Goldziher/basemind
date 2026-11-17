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
//! # Launch policy
//!
//! An engine does not call the [`Registry`] directly: it launches through a [`Supervisor`],
//! which enforces a [`LaunchPolicy`] first. The policy is off by default
//! ([`LaunchPolicy::default`] allows nothing) and names the harness flavors that may run and
//! each one's permission mode (a bypass only with an opt-in naming the flavor and mode), the
//! principals that may launch, the deepest a launched agent may sit, how many sessions may run
//! at once and per launcher, the longest wall time, and an optional budget. A launched agent
//! gets a principal of its own (`launched-<flavor>-<id>-<hex>`), its depth in
//! [`ENV_AGENT_DEPTH`] and a secret in [`ENV_LAUNCH_TOKEN`].
//!
//! Every caller carries the [`Peer`] the kernel reported for its connection, and the supervisor
//! decides by that process rather than by what the caller claims; nothing is trusted by
//! default. Each harness leads a POSIX session of its own, so a process inside one (by session
//! id or ancestry) is that session, at the depth the supervisor recorded, and its token has to
//! be that session's. The engine binds each configured launcher and the operator to one process
//! ([`Supervisor::register_launcher`], [`Supervisor::register_operator`]); a caller is that
//! principal, or the operator ([`Supervisor::operator`]), only from that process or a
//! descendant of it. Any other process is refused. Every registration, launch, stop, exit and
//! approval decision, and every refusal (coalesced, so a looping caller cannot flood the log),
//! goes to an [`AuditSink`] ([`FileAuditSink`]: mode 0600, one JSON object per line, capped with
//! room kept for launches, exits and allows) with fixed outcome codes, the caller as verified,
//! and no brief, command, environment value or token. The supervisor owns the sessions (status, stop, approvals,
//! reaping), kills the detached processes it recorded for a flavor known to leave them
//! ([`Caveat`]) after its session ends, and stops every session when dropped.
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
//! process group survives the session (the session still ends on time), unless the flavor
//! carries [`Caveat::DetachedServer`] and the [`Supervisor`] saw it, as it does OpenCode's own
//! server; a project file written during a session is only seen by the next launch;
//! `BASEMIND_AGENT_ID` is self-asserted; a registration is only as trustworthy as the channel the
//! engine takes it from, and a process of the same user that can trace or inject into the
//! registered process acts as it; no caller can be built where the kernel does not report a
//! connection's peer process or the crate cannot read the process table (other than Linux and
//! macOS); what the ACP agents' modes other than OpenCode's
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
mod audit;
mod claude;
mod codex;
mod detached;
mod error;
mod event;
mod frames;
mod gitconfig;
mod guard;
mod harness;
mod identity;
mod line;
mod peer;
mod pipe;
mod policy;
mod process;
mod procinfo;
mod queue;
mod refusals;
mod registry;
mod session;
mod spec;
mod supervisor;
mod supervisor_admit;
mod supervisor_codes;
mod supervisor_identity;
mod supervisor_pump;
mod supervisor_types;
#[cfg(test)]
mod test_support;

pub use acp::AcpAdapter;
pub use audit::{
    AuditApproval, AuditEvent, AuditLimits, AuditRecord, AuditSink, DEFAULT_AUDIT_MAX_BYTES, FileAuditSink,
    MemoryAuditSink,
};
pub use claude::ClaudeCodeAdapter;
pub use codex::CodexAdapter;
pub use error::LaunchError;
pub use event::{ApprovalKind, ApprovalRequest, ApprovalSource, Event, FinishStatus, ToolCall, ToolState};
pub use harness::{AcpFlavor, Harness, HarnessKind};
pub use identity::{AgentIdentity, ENV_AGENT_DEPTH, ENV_AGENT_ID, ENV_HOST, ENV_LAUNCH_TOKEN};
pub use peer::Peer;
pub use policy::{
    Budget, Caveat, FlavorPolicy, LAUNCHED_PREFIX, LaunchPolicy, MAX_CONCURRENT_CEILING, MAX_DEPTH_CEILING,
    MAX_STOP_GRACE_CEILING, MAX_WALL_TIME_CEILING, PolicyError, PolicySpec, caveats,
};
pub use registry::{Adapter, Registry};
pub use session::{CommandPlan, Session};
pub use spec::{
    ApprovalPolicy, ApprovalVerdict, BypassOptIn, ENV_ALLOWLIST, LaunchConfig, LaunchRequest, Limits, McpServerSpec,
    PermissionMode, UnverifiedModeOptIn,
};
pub use supervisor::{
    Caller, Launched, SessionId, SessionObserver, SessionState, SessionStatus, Supervisor, SupervisorError,
    launch_error_code,
};
