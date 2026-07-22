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
    /// Post a message to the multi-agent room on the human's behalf (issued by the UI on `/post`).
    RoomPost {
        /// An optional subject; the room derives a short one when absent.
        subject: Option<String>,
        /// The message body.
        text: String,
    },
    /// Leave the multi-agent room (issued by the UI on `/leave`); best-effort, a no-op with no room.
    RoomLeave,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_use_a_stable_tagged_shape() {
        let command = AgentCommand::PermissionDecision {
            req_id: 3,
            decision: PermissionDecision::AllowForSession,
        };
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "kind": "permission_decision", "req_id": 3, "decision": "allow_for_session" })
        );
        assert_eq!(serde_json::from_value::<AgentCommand>(json).unwrap(), command);
    }

    #[test]
    fn room_post_round_trips_with_an_optional_subject() {
        let command = AgentCommand::RoomPost {
            subject: None,
            text: "hello team".into(),
        };
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "kind": "room_post", "subject": null, "text": "hello team" })
        );
        assert_eq!(serde_json::from_value::<AgentCommand>(json).unwrap(), command);
    }

    #[test]
    fn room_leave_round_trips_as_a_bare_tag() {
        let command = AgentCommand::RoomLeave;
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json, serde_json::json!({ "kind": "room_leave" }));
        assert_eq!(serde_json::from_value::<AgentCommand>(json).unwrap(), command);
    }
}
