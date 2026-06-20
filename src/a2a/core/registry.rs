//! Agent registry — tracks registered agents by [`AgentId`] and name.
//!
//! The registry is intentionally not wrapped in `Arc`/`RwLock`; that
//! belongs at the server layer where locking strategy is decided.

use std::sync::Arc;

use ahash::AHashMap;
use chrono::Utc;
use thiserror::Error;

use crate::a2a::core::bus::{Event, MessageBus};
use crate::a2a::core::task_types::AgentCapabilities;
use crate::a2a::core::types::{AgentId, AgentInfo, AgentStatus};

/// Errors produced by the [`AgentRegistry`].
///
/// Adapted from the upstream crate-wide error enum: only the three
/// agent-registry variants are ported here, scoped to the A2A core so the
/// registry does not depend on basemind's top-level error surface.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// No agent with this name (or id) is registered.
    #[error("agent '{name}' not found")]
    NotFound {
        /// The agent name or id that could not be located.
        name: String,
    },

    /// An agent with this name is already registered.
    #[error("agent '{name}' is already registered")]
    AlreadyRegistered {
        /// The duplicate agent name.
        name: String,
    },

    /// The provided agent name is syntactically invalid.
    #[error("agent name '{name}' is invalid: {reason}")]
    InvalidName {
        /// The rejected name.
        name: String,
        /// Why the name was rejected.
        reason: String,
    },
}

/// Maximum allowed length for an agent name, in bytes.
const MAX_AGENT_NAME_LEN: usize = 64;

/// Validates an agent name against the allowed character set.
///
/// Rules: 1–64 characters, ASCII alphanumeric plus hyphens and underscores,
/// no leading or trailing hyphen.
fn validate_name(name: &str) -> Result<(), RegistryError> {
    if name.is_empty() {
        return Err(RegistryError::InvalidName {
            name: name.to_owned(),
            reason: "name must not be empty".to_owned(),
        });
    }

    if name.len() > MAX_AGENT_NAME_LEN {
        return Err(RegistryError::InvalidName {
            name: name.to_owned(),
            reason: "name must be 64 characters or fewer".to_owned(),
        });
    }

    if name.starts_with('-') || name.ends_with('-') {
        return Err(RegistryError::InvalidName {
            name: name.to_owned(),
            reason: "name must not start or end with a hyphen".to_owned(),
        });
    }

    let all_valid = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');

    if !all_valid {
        return Err(RegistryError::InvalidName {
            name: name.to_owned(),
            reason: "name may only contain ASCII alphanumeric characters, hyphens, and underscores"
                .to_owned(),
        });
    }

    Ok(())
}

/// Registry of all agents currently connected to the nexus.
///
/// Agents are indexed by [`AgentId`] for O(1) lookup. A secondary
/// `name_index` provides O(1) name uniqueness checks and
/// [`Self::find_by_name`] lookups.
pub struct AgentRegistry {
    agents: AHashMap<AgentId, AgentInfo>,
    name_index: AHashMap<String, AgentId>,
    bus: Arc<MessageBus>,
}

impl AgentRegistry {
    /// Create a new, empty registry backed by the given [`MessageBus`].
    pub fn new(bus: Arc<MessageBus>) -> Self {
        Self {
            agents: AHashMap::new(),
            name_index: AHashMap::new(),
            bus,
        }
    }

    /// Register a new agent with the given `name` and optional capabilities.
    ///
    /// Returns the newly created [`AgentInfo`] on success.
    ///
    /// # Errors
    ///
    /// - [`RegistryError::InvalidName`] — `name` fails validation rules.
    /// - [`RegistryError::AlreadyRegistered`] — an agent with `name` is already registered.
    pub fn register(
        &mut self,
        name: &str,
        capabilities: Option<AgentCapabilities>,
    ) -> Result<AgentInfo, RegistryError> {
        validate_name(name)?;

        if self.name_index.contains_key(name) {
            return Err(RegistryError::AlreadyRegistered {
                name: name.to_owned(),
            });
        }

        let now = Utc::now();
        let info = AgentInfo {
            id: AgentId::new(),
            name: name.to_owned(),
            registered_at: now,
            last_heartbeat_at: now,
            status: AgentStatus::Connected,
            capabilities,
        };

        self.name_index.insert(info.name.clone(), info.id);
        self.agents.insert(info.id, info.clone());
        self.bus.publish(Event::AgentRegistered(info.clone()));

        Ok(info)
    }

    /// Bump the agent's `last_heartbeat_at` to now and ensure it is
    /// [`AgentStatus::Connected`].
    ///
    /// If the agent had been previously flipped to `Disconnected` by the
    /// watchdog, an [`Event::AgentReconnected`] is fired. A live heartbeat
    /// while already `Connected` is silent — the only side-effect is the
    /// timestamp bump.
    ///
    /// # Errors
    ///
    /// - [`RegistryError::NotFound`] — no agent with `id` is registered.
    pub fn heartbeat(&mut self, id: &AgentId) -> Result<AgentInfo, RegistryError> {
        let info = self
            .agents
            .get_mut(id)
            .ok_or_else(|| RegistryError::NotFound {
                name: id.to_string(),
            })?;
        info.last_heartbeat_at = Utc::now();
        let was_disconnected = info.status == AgentStatus::Disconnected;
        if was_disconnected {
            info.status = AgentStatus::Connected;
        }
        let snapshot = info.clone();
        if was_disconnected {
            self.bus.publish(Event::AgentReconnected(snapshot.clone()));
        }
        Ok(snapshot)
    }

    /// Flip the agent to [`AgentStatus::Disconnected`] and emit
    /// [`Event::AgentDisconnected`].
    ///
    /// Idempotent: returns `None` if the agent is already disconnected, the
    /// `AgentInfo` snapshot otherwise. Used by the watchdog when an agent
    /// fails to heartbeat within `agents.timeout_secs`.
    pub fn mark_disconnected(&mut self, id: &AgentId) -> Option<AgentInfo> {
        let info = self.agents.get_mut(id)?;
        if info.status == AgentStatus::Disconnected {
            return None;
        }
        info.status = AgentStatus::Disconnected;
        let snapshot = info.clone();
        self.bus.publish(Event::AgentDisconnected(snapshot.clone()));
        Some(snapshot)
    }

    /// Remove the agent identified by `id` from the registry.
    ///
    /// Returns the removed [`AgentInfo`] on success.
    ///
    /// # Errors
    ///
    /// - [`RegistryError::NotFound`] — no agent with `id` is registered.
    pub fn deregister(&mut self, id: &AgentId) -> Result<AgentInfo, RegistryError> {
        let info = self
            .agents
            .remove(id)
            .ok_or_else(|| RegistryError::NotFound {
                name: id.to_string(),
            })?;

        self.name_index.remove(&info.name);
        self.bus.publish(Event::AgentDeregistered(*id));

        Ok(info)
    }

    /// Look up an agent by its [`AgentId`].
    pub fn get(&self, id: &AgentId) -> Option<&AgentInfo> {
        self.agents.get(id)
    }

    /// Return all registered agents in unspecified order.
    pub fn list(&self) -> Vec<&AgentInfo> {
        self.agents.values().collect()
    }

    /// Find an agent by human-readable name.
    ///
    /// Names are unique within the registry, so at most one result is returned.
    pub fn find_by_name(&self, name: &str) -> Option<&AgentInfo> {
        let id = self.name_index.get(name)?;
        self.agents.get(id)
    }

    /// Clear all entries and repopulate from `agents`.
    ///
    /// Used during daemon startup to restore persisted state. No bus events
    /// are published.
    pub fn restore(&mut self, agents: Vec<AgentInfo>) {
        self.agents.clear();
        self.name_index.clear();
        for info in agents {
            if validate_name(&info.name).is_err() {
                tracing::warn!(name = %info.name, "skipping agent with invalid name during restore");
                continue;
            }
            self.name_index.insert(info.name.clone(), info.id);
            self.agents.insert(info.id, info);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_registry() -> AgentRegistry {
        let bus = Arc::new(MessageBus::new(16));
        AgentRegistry::new(bus)
    }

    // ── register ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn register_agent_succeeds() {
        let mut registry = make_registry();
        let info = registry
            .register("claude-code-1", None)
            .expect("register must succeed for a valid, unique name");

        assert_eq!(
            info.name, "claude-code-1",
            "returned AgentInfo must carry the registered name"
        );
        assert_eq!(
            info.status,
            AgentStatus::Connected,
            "newly registered agent must start as Connected"
        );
    }

    #[tokio::test]
    async fn register_duplicate_name_fails() {
        let mut registry = make_registry();
        registry
            .register("worker", None)
            .expect("first registration must succeed");

        let err = registry
            .register("worker", None)
            .expect_err("second registration with the same name must fail");

        assert!(
            matches!(err, RegistryError::AlreadyRegistered { ref name } if name == "worker"),
            "expected AgentAlreadyRegistered(\"worker\"), got: {err:?}"
        );
    }

    #[tokio::test]
    async fn register_invalid_name_fails_empty_string() {
        let mut registry = make_registry();
        let err = registry
            .register("", None)
            .expect_err("empty name must be rejected");

        assert!(
            matches!(err, RegistryError::InvalidName { ref name, .. } if name.is_empty()),
            "expected AgentInvalidName for empty string, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn register_name_too_long_fails() {
        let mut registry = make_registry();
        let long_name = "a".repeat(65);
        let err = registry
            .register(&long_name, None)
            .expect_err("name exceeding 64 chars must be rejected");

        assert!(
            matches!(err, RegistryError::InvalidName { .. }),
            "expected AgentInvalidName for overlong name, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn register_name_with_leading_hyphen_fails() {
        let mut registry = make_registry();
        let err = registry
            .register("-bad", None)
            .expect_err("leading hyphen must be rejected");

        assert!(
            matches!(err, RegistryError::InvalidName { .. }),
            "expected AgentInvalidName for leading hyphen, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn register_name_with_trailing_hyphen_fails() {
        let mut registry = make_registry();
        let err = registry
            .register("bad-", None)
            .expect_err("trailing hyphen must be rejected");

        assert!(
            matches!(err, RegistryError::InvalidName { .. }),
            "expected AgentInvalidName for trailing hyphen, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn register_name_with_invalid_char_fails() {
        let mut registry = make_registry();
        let err = registry
            .register("bad name", None)
            .expect_err("space in name must be rejected");

        assert!(
            matches!(err, RegistryError::InvalidName { .. }),
            "expected AgentInvalidName for space in name, got: {err:?}"
        );
    }

    // ── deregister ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn deregister_removes_agent() {
        let mut registry = make_registry();
        let info = registry
            .register("to-remove", None)
            .expect("register must succeed");

        registry
            .deregister(&info.id)
            .expect("deregister must succeed for a known id");

        assert!(
            registry.list().is_empty(),
            "list must be empty after deregistering the only agent"
        );
    }

    #[tokio::test]
    async fn deregister_unknown_fails() {
        let mut registry = make_registry();
        let unknown_id = AgentId::new();

        let err = registry
            .deregister(&unknown_id)
            .expect_err("deregister with unknown id must fail");

        assert!(
            matches!(err, RegistryError::NotFound { .. }),
            "expected AgentNotFound, got: {err:?}"
        );
    }

    // ── get / list / find_by_name ─────────────────────────────────────────────

    #[tokio::test]
    async fn get_returns_registered_agent() {
        let mut registry = make_registry();
        let info = registry
            .register("lookup-me", None)
            .expect("register must succeed");

        let found = registry
            .get(&info.id)
            .expect("get must return the registered agent");

        assert_eq!(
            found.id, info.id,
            "get must return the agent with the requested id"
        );
    }

    #[tokio::test]
    async fn find_by_name_works() {
        let mut registry = make_registry();
        registry
            .register("alpha", None)
            .expect("register must succeed");
        let beta = registry
            .register("beta", None)
            .expect("register must succeed");

        let found = registry
            .find_by_name("beta")
            .expect("find_by_name must locate a registered agent");

        assert_eq!(
            found.id, beta.id,
            "find_by_name must return the correct agent"
        );
    }

    #[tokio::test]
    async fn find_by_name_returns_none_for_unknown() {
        let registry = make_registry();
        assert!(
            registry.find_by_name("ghost").is_none(),
            "find_by_name must return None when name is not registered"
        );
    }

    // ── restore ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn restore_populates_registry() {
        let mut registry = make_registry();
        let id1 = AgentId::new();
        let id2 = AgentId::new();
        let agents = vec![
            AgentInfo {
                id: id1,
                name: "alpha".to_owned(),
                registered_at: Utc::now(),
                last_heartbeat_at: Utc::now(),
                status: AgentStatus::Connected,
                capabilities: None,
            },
            AgentInfo {
                id: id2,
                name: "beta".to_owned(),
                registered_at: Utc::now(),
                last_heartbeat_at: Utc::now(),
                status: AgentStatus::Disconnected,
                capabilities: None,
            },
        ];

        registry.restore(agents);

        assert_eq!(registry.list().len(), 2, "restore must populate 2 agents");
        assert!(
            registry.find_by_name("alpha").is_some(),
            "find_by_name must locate restored agent"
        );
        assert_eq!(
            registry.find_by_name("alpha").unwrap().id,
            id1,
            "restored agent id must match"
        );
    }

    #[tokio::test]
    async fn restore_clears_previous_state() {
        let mut registry = make_registry();
        registry
            .register("old-agent", None)
            .expect("register must succeed");

        let new_agent = AgentInfo {
            id: AgentId::new(),
            name: "new-agent".to_owned(),
            registered_at: Utc::now(),
            last_heartbeat_at: Utc::now(),
            status: AgentStatus::Connected,
            capabilities: None,
        };
        registry.restore(vec![new_agent]);

        assert_eq!(registry.list().len(), 1, "restore must replace, not append");
        assert!(
            registry.find_by_name("old-agent").is_none(),
            "old agent must be gone after restore"
        );
        assert!(
            registry.find_by_name("new-agent").is_some(),
            "new agent must be present after restore"
        );
    }

    // ── bus events ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn register_publishes_agent_registered_event() {
        let bus = Arc::new(MessageBus::new(16));
        let mut registry = AgentRegistry::new(Arc::clone(&bus));
        let mut rx = bus.subscribe();

        let info = registry
            .register("event-agent", None)
            .expect("register must succeed");

        let event = rx
            .recv()
            .await
            .expect("bus must deliver the AgentRegistered event");

        let Event::AgentRegistered(received) = event else {
            panic!("expected AgentRegistered event, got something else");
        };

        assert_eq!(
            received.id, info.id,
            "event must carry the id of the registered agent"
        );
        assert_eq!(
            received.name, "event-agent",
            "event must carry the name of the registered agent"
        );
    }

    #[tokio::test]
    async fn deregister_publishes_agent_deregistered_event() {
        let bus = Arc::new(MessageBus::new(16));
        let mut registry = AgentRegistry::new(Arc::clone(&bus));

        let info = registry
            .register("to-deregister", None)
            .expect("register must succeed");

        let mut rx = bus.subscribe();

        registry
            .deregister(&info.id)
            .expect("deregister must succeed");

        let event = rx
            .recv()
            .await
            .expect("bus must deliver the AgentDeregistered event");

        let Event::AgentDeregistered(deregistered_id) = event else {
            panic!("expected AgentDeregistered event, got something else");
        };

        assert_eq!(
            deregistered_id, info.id,
            "event must carry the id of the deregistered agent"
        );
    }
}
