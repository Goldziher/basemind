// Adapted from https://github.com/hardbeat920/monocode (crates/monocode-core/src/{provider,agent,tool}.rs) at 09d04436babb39ad72198cdf22f735067027fc04, MIT.
//! What a session reports and what it can be told.

/// How far a tool call has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolState {
    /// Started and not yet settled.
    Running,
    /// Finished successfully.
    Completed,
    /// Finished with an error, or was declined.
    Failed,
    /// Abandoned because its turn was interrupted.
    Cancelled,
}

/// One tool call of the launched agent. The same `id` is reported again as the call
/// progresses; the latest report replaces the earlier ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    /// The harness's id for the call.
    pub id: String,
    /// The tool name; an MCP tool is `<server>:<tool>` under Codex.
    pub name: String,
    /// The arguments, as the harness rendered them (usually JSON).
    pub input: String,
    /// The output so far, cut to its last 64 KiB.
    pub output: String,
    /// How far the call has got.
    pub state: ToolState,
}

impl ToolCall {
    pub(crate) fn new(id: &str, name: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            input: String::new(),
            output: String::new(),
            state: ToolState::Running,
        }
    }

    pub(crate) fn append_output(&mut self, text: &str) {
        self.output.push_str(text);
        self.bound_output();
    }

    pub(crate) fn bound_output(&mut self) {
        const LIMIT: usize = 64 * 1024;
        if self.output.len() > LIMIT {
            let mut at = self.output.len() - LIMIT;
            while !self.output.is_char_boundary(at) {
                at += 1;
            }
            self.output = format!("… earlier output omitted …\n{}", &self.output[at..]);
        }
    }
}

/// What an approval request would let the harness do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalKind {
    /// Run a shell command.
    Command,
    /// Create, edit, move or delete files.
    FileChange,
    /// Grant the harness further permissions, such as network access or writable paths.
    Permissions,
    /// Call a tool of an MCP server.
    McpTool,
    /// Call another tool of the harness.
    Tool,
}

/// A request by the harness to do something its permission mode does not cover.
///
/// `title` and `detail` are rendered from the grant itself: the command, the file changes,
/// the permissions or the tool input that an allow would approve. A reason the agent gave is
/// only appended to `detail`, marked as the agent's, and never stands in for the grant.
/// A policy that decides by content should read `kind` and `params`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ApprovalRequest {
    /// The id to pass to [`Session::approve`](crate::Session::approve) or
    /// [`Session::deny`](crate::Session::deny).
    pub id: String,
    /// What the request would allow.
    pub kind: ApprovalKind,
    /// A one-line question, such as `Allow Bash?`.
    pub title: String,
    /// The grant in readable form, followed by the agent's reason when it gave one.
    pub detail: String,
    /// The harness's request as it arrived on the wire: Claude Code's `can_use_tool` request,
    /// the params of a Codex approval request, or the ACP `session/request_permission` params.
    pub params: serde_json::Value,
}

/// The `detail` of a request: the grant, then the agent's own reason, labelled as such.
pub(crate) fn approval_detail(grant: &str, reason: Option<&str>) -> String {
    match reason.filter(|reason| !reason.trim().is_empty()) {
        Some(reason) => format!("{grant}\n\nReason given by the agent (unverified): {reason}"),
        None => grant.to_owned(),
    }
}

/// `text` on one line, with control characters replaced, for an approval title.
pub(crate) fn one_line(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Who settled an approval request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalSource {
    /// No policy was supplied, so the request was denied.
    DefaultDeny,
    /// The spec's [`ApprovalPolicy`](crate::ApprovalPolicy) decided.
    Policy,
    /// The caller answered through the session.
    Caller,
    /// The harness withdrew the request, or its turn or the session ended first.
    Withdrawn,
    /// The policy deferred it while the most requests a session holds were already pending,
    /// so it was denied.
    TooManyPending,
}

/// How a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FinishStatus {
    /// Every turn the limits allow ran to its end.
    Completed,
    /// [`Session::stop`](crate::Session::stop) was called or the session was dropped.
    Stopped,
    /// The wall-time limit passed and the process group was killed.
    WallTimeExceeded,
    /// The harness started more turns than allowed and the process group was killed.
    TurnLimitExceeded,
    /// The harness failed; an [`Event::Error`] precedes this.
    Failed,
}

/// One thing a session reports.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// The harness's own id for the conversation, usable to resume it there.
    HarnessSession(String),
    /// Assistant text, as an increment.
    Text(String),
    /// Reasoning text, as an increment.
    Reasoning(String),
    /// A tool call started or progressed.
    ToolCall(ToolCall),
    /// The harness asks for permission. Unless the policy answered
    /// [`ApprovalVerdict::Ask`](crate::ApprovalVerdict::Ask), an [`Event::ApprovalResolved`]
    /// follows at once.
    ApprovalRequest(ApprovalRequest),
    /// An approval request was settled.
    ApprovalResolved {
        /// The request's id.
        id: String,
        /// Whether the harness was allowed to proceed.
        allowed: bool,
        /// Who settled it.
        source: ApprovalSource,
    },
    /// A transient note from the harness, such as a reconnect.
    Status(String),
    /// A turn ended. `turn` counts from 1.
    TurnCompleted {
        /// Which turn ended.
        turn: u32,
    },
    /// The session failed, or a follow-up could not be delivered.
    Error(String),
    /// The session is over and its process group is gone. Always the last event.
    Finished(FinishStatus),
}

/// What the session handle tells the adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Control {
    Answer { id: String, allow: bool },
    FollowUp(String),
    Stop,
}
