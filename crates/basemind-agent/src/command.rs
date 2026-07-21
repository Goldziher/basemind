//! Commands sent from a UI into the engine.
//!
//! Serde-serializable for the same reason as [`crate::event::AgentEvent`]: the values must cross
//! an in-process channel now and a msgpack frame later without changing shape.

use serde::{Deserialize, Serialize};

/// A user's decision on a pending permission request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Allow this one call.
    Allow,
    /// Allow this call and remember the claim for the rest of the session.
    AllowForSession,
    /// Deny this call.
    Deny,
}

/// A command the UI issues to the engine. Internally tagged on `kind`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentCommand {
    /// Queue a user message; starts a turn if the session is idle.
    UserMessage {
        /// The message text.
        text: String,
    },
    /// Reply to an outstanding [`crate::event::AgentEvent::PermissionRequested`].
    PermissionDecision {
        /// The request id being answered.
        req_id: u64,
        /// The decision.
        decision: PermissionDecision,
    },
    /// Cooperatively cancel the in-flight turn (aborts the stream and running tools).
    Cancel,
    /// Gracefully shut the session down (flush persistence, drop clients).
    Shutdown,
}
