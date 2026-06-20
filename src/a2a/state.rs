//! Shared state backing the A2A transport adapters.
//!
//! [`A2aState`] owns the single [`TaskFacade`] instance, the [`MessageBus`],
//! the push-notification store, and the optional bearer token shared by every
//! transport binding. Both the gRPC service ([`crate::a2a::grpc`]) and the
//! JSON-RPC/SSE handlers consume the same state through the shared axum app —
//! keep this holder transport-agnostic.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::a2a::core::bus::MessageBus;
use crate::a2a::core::push_notifications::PushNotificationStore;
use crate::a2a::core::registry::AgentRegistry;
use crate::a2a::core::router::DefaultTaskRouter;
use crate::a2a::core::task_facade::TaskFacade;
use crate::a2a::core::task_manager::TaskManager;

/// Capacity of the shared [`MessageBus`] broadcast channel.
///
/// Sized to match the server's `MAX_CONCURRENT_REQUESTS` (1024, defined in
/// `crate::a2a::server`) so that even under a full-concurrency burst SSE/gRPC
/// subscribers do not silently lag-drop task-lifecycle events. Kept in
/// lock-step with that limit — bump both together.
const BUS_CAPACITY: usize = 1024;

/// Static descriptor used to populate the A2A agent card.
///
/// Held by [`A2aState`] so every transport adapter can answer
/// `get_extended_agent_card` without reaching into global config.
#[derive(Clone, Debug)]
pub struct AgentCardInfo {
    /// Human-readable agent name.
    pub name: String,
    /// Human-readable agent description.
    pub description: String,
    /// Agent version string.
    pub version: String,
    /// Advertised gRPC endpoint URL (empty until the server binds an address).
    pub grpc_url: String,
    /// Advertised JSON-RPC/HTTP endpoint URL — the agent card's primary
    /// interface (empty until the axum server binds an address).
    pub http_url: String,
    /// Whether the server requires bearer auth. Drives the card's advertised
    /// `securitySchemes`; set in lock-step with [`A2aState::auth_token`].
    pub requires_auth: bool,
}

impl Default for AgentCardInfo {
    fn default() -> Self {
        Self {
            name: "basemind".to_owned(),
            description: "basemind agent context + A2A task server".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            grpc_url: String::new(),
            http_url: String::new(),
            requires_auth: false,
        }
    }
}

/// Shared, cheaply-cloneable state backing the A2A transport adapters.
///
/// Every field is an `Arc` (or `Arc<RwLock<_>>`) so cloning the state hands out
/// a new handle onto the same underlying domain graph rather than duplicating
/// it.
#[derive(Clone)]
pub struct A2aState {
    /// Canonical task API shared by all transport adapters.
    pub task_facade: Arc<TaskFacade>,
    /// Intra-process event bus for task-lifecycle fan-out.
    pub bus: Arc<MessageBus>,
    /// Per-task webhook configuration store.
    pub push_notifications: Arc<RwLock<PushNotificationStore>>,
    /// Static agent-card descriptor.
    pub card: AgentCardInfo,
    /// Expected bearer token, or `None` when auth is disabled. Shared (`Arc`)
    /// so the auth middleware reads it without copying the secret per request.
    pub auth_token: Option<Arc<str>>,
}

impl A2aState {
    /// Build the full A2A domain graph wired around a fresh [`MessageBus`].
    ///
    /// Auth is disabled by default; enable it with [`A2aState::with_auth_token`].
    pub fn new(card: AgentCardInfo) -> Self {
        let bus = Arc::new(MessageBus::new(BUS_CAPACITY));
        let task_manager = Arc::new(RwLock::new(TaskManager::new(Arc::clone(&bus))));
        let registry = Arc::new(RwLock::new(AgentRegistry::new(Arc::clone(&bus))));
        let task_facade = Arc::new(TaskFacade::new(
            task_manager,
            registry,
            Box::new(DefaultTaskRouter),
        ));
        let push_notifications = Arc::new(RwLock::new(PushNotificationStore::new()));

        Self {
            task_facade,
            bus,
            push_notifications,
            card,
            auth_token: None,
        }
    }

    /// Enable (or disable) bearer auth, keeping [`AgentCardInfo::requires_auth`]
    /// in lock-step so the advertised card matches what the server enforces.
    #[must_use]
    pub fn with_auth_token(mut self, token: Option<Arc<str>>) -> Self {
        self.card.requires_auth = token.is_some();
        self.auth_token = token;
        self
    }
}

impl Default for A2aState {
    fn default() -> Self {
        Self::new(AgentCardInfo::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_builds_with_basemind_card() {
        let state = A2aState::default();
        assert_eq!(state.card.name, "basemind");
    }
}
