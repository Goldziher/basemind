//! Shared identity types for the A2A core.
//!
//! This module holds the A2A-internal identity newtypes ([`AgentId`],
//! [`MessageId`]) that the task system depends on. These are distinct from
//! basemind's `comms::ids::AgentId` — they are the A2A protocol's own identity
//! surface and must not be merged with the comms identity.
//!
//! Chat / tool / conversation types from the upstream source are intentionally
//! omitted here; basemind models those concerns elsewhere.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
}
