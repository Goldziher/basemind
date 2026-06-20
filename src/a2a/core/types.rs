//! Shared identity and agent metadata types for the A2A core.
//!
//! This module holds the A2A-internal identity newtypes ([`AgentId`],
//! [`MessageId`]) and the agent metadata ([`AgentInfo`], [`AgentStatus`]) that
//! the task system depends on. These are distinct from basemind's
//! `comms::ids::AgentId` — they are the A2A protocol's own identity surface and
//! must not be merged with the comms identity.
//!
//! Chat / tool / conversation types from the upstream source are intentionally
//! omitted here; basemind models those concerns elsewhere.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::a2a::core::task_types::AgentCapabilities;

/// Unique identifier for a registered agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(Uuid);

impl AgentId {
    /// Create a new random [`AgentId`].
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for AgentId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

/// Unique identifier for a single message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(Uuid);

impl MessageId {
    /// Create a new random [`MessageId`].
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for MessageId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for MessageId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

/// Lifecycle state of a registered agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// The agent is connected and accepting work.
    Connected,
    /// The agent is not currently reachable.
    Disconnected,
}

/// Metadata for a registered agent.
///
/// Does not include runtime state such as active tasks; those live in the
/// registry and are looked up by [`AgentId`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Stable identity for this agent.
    pub id: AgentId,
    /// Human-readable agent name (e.g. `"claude-code-1"`).
    pub name: String,
    /// Wall-clock time at which the agent first registered.
    pub registered_at: DateTime<Utc>,
    /// Wall-clock time of the most recent heartbeat (or registration, when
    /// no heartbeat has yet been observed). Used by the connection watchdog
    /// to detect dead agents and flip them to [`AgentStatus::Disconnected`]
    /// after `agents.timeout_secs` of silence.
    #[serde(default = "Utc::now")]
    pub last_heartbeat_at: DateTime<Utc>,
    /// Current connectivity status.
    pub status: AgentStatus,
    /// Optional capabilities advertised by this agent (ADR-015).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<AgentCapabilities>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_new_produces_unique_ids() {
        let a = AgentId::new();
        let b = AgentId::new();
        assert_ne!(a, b, "two freshly generated AgentIds must not be equal");
    }

    #[test]
    fn message_id_new_produces_unique_ids() {
        let a = MessageId::new();
        let b = MessageId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn agent_id_display_is_valid_uuid_string() {
        let id = AgentId::new();
        let s = id.to_string();
        // A UUID v4 hyphenated string is always 36 characters.
        assert_eq!(s.len(), 36, "display should be a hyphenated UUID: {s}");
        assert!(
            s.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "display must only contain hex digits and hyphens: {s}"
        );
    }

    #[test]
    fn agent_info_round_trips_through_json() {
        let original = AgentInfo {
            id: AgentId::new(),
            name: "claude-code-1".to_owned(),
            registered_at: Utc::now(),
            last_heartbeat_at: Utc::now(),
            status: AgentStatus::Connected,
            capabilities: None,
        };
        let json = serde_json::to_string(&original).expect("serialization must succeed");
        let recovered: AgentInfo =
            serde_json::from_str(&json).expect("deserialization must succeed");

        assert_eq!(original.id, recovered.id, "id must survive round-trip");
        assert_eq!(
            original.name, recovered.name,
            "name must survive round-trip"
        );
        assert_eq!(
            original.status, recovered.status,
            "status must survive round-trip"
        );
    }

    #[test]
    fn agent_status_serializes_as_snake_case() {
        let connected =
            serde_json::to_string(&AgentStatus::Connected).expect("serialization must succeed");
        assert_eq!(connected, r#""connected""#);

        let disconnected =
            serde_json::to_string(&AgentStatus::Disconnected).expect("serialization must succeed");
        assert_eq!(disconnected, r#""disconnected""#);
    }
}
