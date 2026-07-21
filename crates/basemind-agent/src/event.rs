//! Events streamed from the engine to a UI.
//!
//! Serde-serializable so the same values cross an in-process channel now or a length-delimited
//! msgpack frame later (see [`crate::transport`]). New variants and fields are additive.

use serde::{Deserialize, Serialize};

/// Why a turn stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished normally.
    Stop,
    /// The step budget (`max_steps`) was reached.
    MaxSteps,
    /// The model hit its output length limit.
    Length,
    /// A provider content filter halted generation.
    ContentFilter,
    /// The user cancelled the turn.
    Cancelled,
    /// The session cost budget was exceeded.
    BudgetExceeded,
    /// The turn ended on an error.
    Error,
}

/// An event emitted by the engine during a turn. Internally tagged on `kind` for stable,
/// self-describing wire frames.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A new turn began.
    TurnStarted {
        /// Monotonic turn number within the session.
        turn: u64,
    },
    /// A streaming assistant text delta (reasoning/thinking already stripped upstream).
    TextDelta {
        /// The turn this delta belongs to.
        turn: u64,
        /// Monotonic sequence within the turn (UIs may use it to order/dedupe).
        seq: u64,
        /// The text fragment.
        text: String,
    },
    /// A tool call was fully assembled and is about to run.
    ToolStarted {
        /// The turn this call belongs to.
        turn: u64,
        /// Provider-assigned tool-call id (pairs with [`AgentEvent::ToolResult`]).
        call_id: String,
        /// The tool name (namespaced, e.g. `code:outline`).
        name: String,
        /// The parsed arguments (or `null` if they were unparseable).
        args: serde_json::Value,
    },
    /// Incremental output from a long-running tool (e.g. shell stdout).
    ToolProgress {
        /// The tool-call id this progress belongs to.
        call_id: String,
        /// A chunk of the tool's streaming output.
        chunk: String,
    },
    /// A tool finished.
    ToolResult {
        /// The tool-call id.
        call_id: String,
        /// Whether the tool succeeded.
        ok: bool,
        /// A short, human-facing summary of the result.
        summary: String,
    },
    /// The engine is blocked awaiting a permission decision for a tool call.
    PermissionRequested {
        /// Correlates with the [`crate::command::AgentCommand::PermissionDecision`] reply.
        req_id: u64,
        /// The tool-call awaiting approval.
        call_id: String,
        /// The tool name.
        tool: String,
        /// The action being requested (e.g. `write`, `exec`).
        action: String,
        /// The target of the action (path, command, host).
        target: String,
    },
    /// Token/cost accounting for a turn (delta plus running session totals).
    Usage {
        /// The turn this usage belongs to.
        turn: u64,
        /// Cumulative input tokens for the session.
        input_tokens: u64,
        /// Cumulative output tokens for the session.
        output_tokens: u64,
        /// Estimated cumulative cost in USD, if the provider exposes pricing.
        cost_usd: Option<f64>,
    },
    /// History was compacted to fit the context window.
    Compacted {
        /// How many messages were replaced by a summary.
        removed_messages: usize,
        /// Approximate token size of the produced summary.
        summary_tokens: u32,
    },
    /// A turn finished.
    TurnFinished {
        /// The turn number.
        turn: u64,
        /// Why it stopped.
        reason: StopReason,
        /// How many model steps the turn took.
        steps: u32,
    },
    /// A non-fatal or fatal error occurred.
    Error {
        /// The turn it occurred in, if any.
        turn: Option<u64>,
        /// The error message.
        message: String,
        /// Whether the session cannot continue.
        fatal: bool,
    },
}
