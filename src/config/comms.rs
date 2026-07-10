//! `[comms]` configuration sub-tree.
//!
//! Governs the user-global agent-comms broker daemon and the identity an MCP / CLI client
//! presents to it. The whole tree is feature-independent at the type level (it derives the
//! same `schemars` schema whether or not the `comms` cargo feature is compiled in) so the
//! published config schema is stable across feature matrices; the daemon itself only reads
//! these values when built with `--features comms`.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default daemon idle timeout: 30 minutes. After this many seconds with no live subscribers
/// the broker may shed in-RAM caches (it keeps the socket bound — see the daemon lifecycle).
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 1800;
/// Default per-room message retention cap. Oldest front-matter beyond this is eligible for
/// pruning so a long-lived room cannot grow without bound.
const DEFAULT_MAX_MESSAGES_PER_ROOM: u32 = 1000;
/// Default message retention window: 7 days in seconds. Comms history is durable-but-disposable
/// scratch, not a source of truth.
const DEFAULT_RETENTION_SECS: u64 = 604_800;
/// Default cap on the number of concurrently registered rooms.
const DEFAULT_MAX_ROOMS: u32 = 256;

/// `[comms]` config sub-tree. See module docs for the lifecycle context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommsConfig {
    /// Master switch. Only meaningful when the `comms` cargo feature is compiled in; when
    /// `false` the MCP / CLI comms tools are wired but the server never connects to a daemon.
    #[serde(default = "CommsConfig::default_enabled")]
    pub enabled: bool,
    /// Stable identity this process presents to the broker. When unset, the identity resolver
    /// falls back to `BASEMIND_AGENT_ID`, then a generated-and-persisted per-session id, then
    /// `"anon"`. Validated through [`crate::comms::ids::AgentId`] at resolution time.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Seconds of zero-subscriber idle before the broker sheds caches. Keeps the socket bound.
    #[serde(default = "CommsConfig::default_idle_timeout_secs")]
    #[schemars(range(min = 1))]
    pub idle_timeout_secs: u64,
    /// Per-room front-matter retention cap before old messages become prune-eligible.
    #[serde(default = "CommsConfig::default_max_messages_per_room")]
    #[schemars(range(min = 1))]
    pub max_messages_per_room: u32,
    /// Message retention window in seconds. Older messages are eligible for pruning.
    #[serde(default = "CommsConfig::default_retention_secs")]
    #[schemars(range(min = 1))]
    pub retention_secs: u64,
    /// Hard cap on the number of concurrently registered rooms.
    #[serde(default = "CommsConfig::default_max_rooms")]
    #[schemars(range(min = 1))]
    pub max_rooms: u32,
    /// Optional explicit workspace root the daemon associates this client with. When unset the
    /// client uses its discovered repo / cwd for scope context.
    #[serde(default)]
    pub workspace_root: Option<PathBuf>,
}

impl CommsConfig {
    fn default_enabled() -> bool {
        true
    }
    fn default_idle_timeout_secs() -> u64 {
        DEFAULT_IDLE_TIMEOUT_SECS
    }
    fn default_max_messages_per_room() -> u32 {
        DEFAULT_MAX_MESSAGES_PER_ROOM
    }
    fn default_retention_secs() -> u64 {
        DEFAULT_RETENTION_SECS
    }
    fn default_max_rooms() -> u32 {
        DEFAULT_MAX_ROOMS
    }
}

impl Default for CommsConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            agent_id: None,
            idle_timeout_secs: Self::default_idle_timeout_secs(),
            max_messages_per_room: Self::default_max_messages_per_room(),
            retention_secs: Self::default_retention_secs(),
            max_rooms: Self::default_max_rooms(),
            workspace_root: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_values() {
        let c = CommsConfig::default();
        assert!(c.enabled);
        assert_eq!(c.agent_id, None);
        assert_eq!(c.idle_timeout_secs, 1800);
        assert_eq!(c.max_messages_per_room, 1000);
        assert_eq!(c.retention_secs, 604_800);
        assert_eq!(c.max_rooms, 256);
        assert_eq!(c.workspace_root, None);
    }

    #[test]
    fn round_trips_through_toml_with_partial_keys() {
        let toml = r#"
            enabled = false
            agent_id = "claude-code"
            max_rooms = 8
        "#;
        let c: CommsConfig = toml::from_str(toml).expect("parse");
        assert!(!c.enabled);
        assert_eq!(c.agent_id.as_deref(), Some("claude-code"));
        assert_eq!(c.max_rooms, 8);
        assert_eq!(c.retention_secs, 604_800);
    }

    #[test]
    fn rejects_unknown_fields() {
        let toml = "bogus_key = 1\n";
        assert!(toml::from_str::<CommsConfig>(toml).is_err());
    }
}
