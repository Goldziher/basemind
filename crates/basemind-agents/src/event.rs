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
    /// The arguments, as the harness rendered them (usually JSON), cut to their first 64 KiB.
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

    /// Appends to the output. Trimming is amortized: the output may grow to twice the limit
    /// before it is cut, so a stream of tiny increments does not copy 64 KiB each time.
    pub(crate) fn append_output(&mut self, text: &str) {
        self.output.push_str(text);
        if self.output.len() > 2 * OUTPUT_LIMIT {
            self.bound_output();
        }
    }

    /// Cuts the output to its last 64 KiB.
    pub(crate) fn bound_output(&mut self) {
        if self.output.len() > OUTPUT_LIMIT {
            let mut at = self.output.len() - OUTPUT_LIMIT;
            while !self.output.is_char_boundary(at) {
                at += 1;
            }
            self.output = format!("{OMITTED}{}", &self.output[at..]);
        }
    }

    /// Cuts the input to its first 64 KiB.
    pub(crate) fn bound_input(&mut self) {
        if self.input.len() > OUTPUT_LIMIT {
            let mut at = OUTPUT_LIMIT;
            while !self.input.is_char_boundary(at) {
                at -= 1;
            }
            self.input.truncate(at);
            self.input.push_str("\n… later input omitted …");
        }
    }
}

const OUTPUT_LIMIT: usize = 64 * 1024;
const OMITTED: &str = "… earlier output omitted …\n";

/// The tool calls an adapter tracks, at most [`MAX_TRACKED_TOOLS`] of them: the oldest is
/// forgotten first, so a session with endless tool calls holds a bounded table.
#[derive(Default)]
pub(crate) struct Tools {
    calls: std::collections::HashMap<String, ToolCall>,
    order: std::collections::VecDeque<String>,
}

/// How many tool calls an adapter remembers.
pub(crate) const MAX_TRACKED_TOOLS: usize = 64;

impl Tools {
    /// The call `id`, created by `new` when it is not tracked.
    pub fn get_or_insert(&mut self, id: &str, new: impl FnOnce() -> ToolCall) -> &mut ToolCall {
        if !self.calls.contains_key(id) {
            if self.order.len() >= MAX_TRACKED_TOOLS
                && let Some(oldest) = self.order.pop_front()
            {
                self.calls.remove(&oldest);
            }
            self.order.push_back(id.to_owned());
            self.calls.insert(id.to_owned(), new());
        }
        self.calls.get_mut(id).expect("the call was just inserted")
    }

    /// Tracks `call`, replacing a call with the same id.
    pub fn insert(&mut self, call: ToolCall) -> &mut ToolCall {
        let id = call.id.clone();
        let slot = self.get_or_insert(&id, || call.clone());
        *slot = call;
        slot
    }

    pub fn get(&self, id: &str) -> Option<&ToolCall> {
        self.calls.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut ToolCall> {
        self.calls.get_mut(id)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut ToolCall> {
        self.calls.values_mut()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.calls.len()
    }
}

/// What an approval request would let the harness do, as the harness itself classifies it
/// (Claude Code's tool name, ACP's tool kind, the Codex request method). A policy deciding on
/// an MCP tool should check the full tool name in `params`, not the kind alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalKind {
    /// Run a shell command.
    Command,
    /// Create, edit, move or delete files.
    FileChange,
    /// Grant the harness further permissions, such as network access or writable paths, or
    /// switch it into a mode that asks less (Claude Code's `ExitPlanMode`, an ACP mode switch).
    Permissions,
    /// Call a tool of an MCP server. Claude Code and Codex report these; ACP does not say which
    /// tool calls are MCP calls, so an ACP request is never of this kind.
    McpTool,
    /// Fetch from the network or search the web.
    Fetch,
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

/// The longest [`Event::Error`] text.
const ERROR_BYTES: usize = 512;

/// `text` as an [`Event::Error`] carries it: on one line, with control characters replaced,
/// cut to its first [`ERROR_BYTES`].
pub(crate) fn error_line(text: &str) -> String {
    let mut line = one_line(text);
    if line.len() > ERROR_BYTES {
        let mut end = ERROR_BYTES - '…'.len_utf8();
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push('…');
    }
    line
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
    /// The policy deferred it while the most requests a session holds (16) were already
    /// pending, or it was too large (over 1 MiB) to hold, so it was denied.
    TooManyPending,
    /// What an allow would grant could not be shown in full (a Codex file change whose
    /// changes were not announced, or are over 1 MiB), so it was denied without asking.
    GrantUnknown,
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
    /// The session failed, or a control could not be delivered. One line of at most 512 bytes
    /// with control characters replaced, and without what the harness printed on stderr: fit
    /// to show the model that launched the session. A failure is preceded by
    /// [`Event::Diagnostics`] with the full text.
    Error(String),
    /// The full text of the failure the next [`Event::Error`] reports, and the last 8 KiB the
    /// harness wrote to stderr. For the launcher's own logs: it may carry paths, account
    /// details or anything else the harness printed, and is not for a model.
    Diagnostics(String),
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
