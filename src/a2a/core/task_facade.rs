//! Unified task operations facade (ADR-014).
//!
//! [`TaskFacade`] is the single entry point for all task operations across
//! every transport adapter (MCP, gRPC, REST). It orchestrates validation,
//! state transitions, routing, and bus event publishing.

use std::sync::Arc;

use thiserror::Error;
use tokio::sync::RwLock;

use crate::a2a::core::registry::{AgentRegistry, RegistryError};
use crate::a2a::core::router::{TaskRouteContext, TaskRouter};
use crate::a2a::core::task_manager::{TaskError, TaskEvent, TaskManager};
use crate::a2a::core::task_types::{
    Artifact, ContextId, Task, TaskFilter, TaskId, TaskMessage, TaskState,
};
use crate::a2a::core::types::{AgentId, AgentInfo, AgentStatus};

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors produced by [`TaskFacade`] operations.
///
/// The upstream nexus facade returned the monolithic crate-wide `Error`. Here
/// the facade composes the sibling modules' focused, locally-scoped errors:
/// task-manager failures surface as [`FacadeError::Task`] and registry failures
/// (e.g. heartbeat lookups) as [`FacadeError::Registry`]. The two facade-native
/// variants below cover the pinned-assignee preflight, which has no analogue in
/// either sibling enum.
#[derive(Debug, Error)]
pub enum FacadeError {
    /// A pinned assignee was supplied but no agent with that id is registered.
    #[error("agent '{name}' not found")]
    AgentNotFound {
        /// The agent id that failed to resolve.
        name: String,
    },

    /// A pinned assignee was supplied but that agent is currently disconnected.
    #[error("agent '{name}' is disconnected")]
    AgentDisconnected {
        /// The disconnected agent's name.
        name: String,
    },

    /// A task-manager operation failed (create, transition, artifact, lookup).
    #[error(transparent)]
    Task(#[from] TaskError),

    /// A registry operation failed (e.g. heartbeat lookup).
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

// ── TaskFacade ──────────────────────────────────────────────────────────────────

/// Canonical task API used by all transport adapters.
///
/// Every protocol adapter (gRPC, MCP, REST) calls through the facade.
/// Protocol-specific type conversion happens at the adapter boundary.
pub struct TaskFacade {
    tasks: Arc<RwLock<TaskManager>>,
    registry: Arc<RwLock<AgentRegistry>>,
    router: Box<dyn TaskRouter>,
}

impl TaskFacade {
    /// Create a new facade backed by the given task manager, agent registry,
    /// and router.
    pub fn new(
        tasks: Arc<RwLock<TaskManager>>,
        registry: Arc<RwLock<AgentRegistry>>,
        router: Box<dyn TaskRouter>,
    ) -> Self {
        Self {
            tasks,
            registry,
            router,
        }
    }

    /// Submit a new task. Routes to an available agent if possible.
    ///
    /// Routing is resolved before task creation so the assignee is set
    /// atomically — no window where the task exists without an assignee.
    pub async fn submit_task(
        &self,
        message: TaskMessage,
        context_id: Option<ContextId>,
        assignee: Option<AgentId>,
        metadata: Option<serde_json::Value>,
    ) -> Result<Task, FacadeError> {
        self.submit_task_with_deadline(message, context_id, assignee, metadata, None)
            .await
    }

    /// Same as [`Self::submit_task`] but accepts an explicit `deadline`.
    pub async fn submit_task_with_deadline(
        &self,
        message: TaskMessage,
        context_id: Option<ContextId>,
        assignee: Option<AgentId>,
        metadata: Option<serde_json::Value>,
        deadline: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Task, FacadeError> {
        // Resolve the assignee under a single registry read lock so a pinned
        // agent cannot flip to Disconnected between the preflight check and the
        // routing decision (TOCTOU). The router inspects only the prospective
        // task's metadata/assignee, so no throwaway `Task` is allocated.
        let resolved_assignee = {
            let registry = self.registry.read().await;

            // If the caller pinned an explicit assignee, reject early when that
            // agent is currently disconnected. Silently re-routing a pinned
            // assignment would surprise callers who picked a specific agent.
            if let Some(id) = assignee {
                match registry.get(&id) {
                    None => {
                        return Err(FacadeError::AgentNotFound {
                            name: id.to_string(),
                        });
                    }
                    Some(info) if info.status != AgentStatus::Connected => {
                        return Err(FacadeError::AgentDisconnected {
                            name: info.name.clone(),
                        });
                    }
                    _ => {}
                }
                // A pinned, connected assignee is honoured verbatim — no need to
                // run the router.
                assignee
            } else {
                let route_context = TaskRouteContext {
                    assignee: None,
                    context_id: context_id.unwrap_or_default(),
                    metadata: metadata.as_ref(),
                };
                let agents = registry.list();
                self.router.select_agent(&route_context, &agents)
            }
        };

        let mut mgr = self.tasks.write().await;
        let task = mgr.create_task_with_deadline(
            message,
            context_id,
            resolved_assignee,
            None,
            metadata,
            deadline,
        )?;
        Ok(task)
    }

    /// Bump an agent's `last_heartbeat_at` and return the updated info.
    pub async fn heartbeat(&self, agent_id: &AgentId) -> Result<AgentInfo, FacadeError> {
        let mut reg = self.registry.write().await;
        Ok(reg.heartbeat(agent_id)?)
    }

    /// Transition a task to a new state.
    pub async fn update_status(
        &self,
        task_id: &TaskId,
        new_state: TaskState,
        message: Option<TaskMessage>,
    ) -> Result<Task, FacadeError> {
        let mut mgr = self.tasks.write().await;
        Ok(mgr.update_status(task_id, new_state, message)?)
    }

    /// Append an artifact to a task.
    pub async fn add_artifact(
        &self,
        task_id: &TaskId,
        artifact: Artifact,
    ) -> Result<Task, FacadeError> {
        let mut mgr = self.tasks.write().await;
        Ok(mgr.add_artifact(task_id, artifact)?)
    }

    /// Cancel a task.
    pub async fn cancel_task(
        &self,
        task_id: &TaskId,
        message: Option<TaskMessage>,
    ) -> Result<Task, FacadeError> {
        let mut mgr = self.tasks.write().await;
        Ok(mgr.cancel(task_id, message)?)
    }

    /// Get a task by ID.
    pub async fn get_task(&self, task_id: &TaskId) -> Result<Task, FacadeError> {
        let mgr = self.tasks.read().await;
        mgr.get(task_id).cloned().ok_or_else(|| {
            FacadeError::Task(TaskError::TaskNotFound {
                id: task_id.to_string(),
            })
        })
    }

    /// List tasks matching a filter.
    pub async fn list_tasks(&self, filter: &TaskFilter) -> Vec<Task> {
        let mgr = self.tasks.read().await;
        mgr.list_filtered(filter).into_iter().cloned().collect()
    }

    /// Subscribe to task-lifecycle events.
    pub async fn subscribe(&self) -> tokio::sync::broadcast::Receiver<TaskEvent> {
        let mgr = self.tasks.read().await;
        mgr.subscribe()
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::a2a::core::bus::MessageBus;
    use crate::a2a::core::registry::AgentRegistry;
    use crate::a2a::core::router::DefaultTaskRouter;
    use crate::a2a::core::task_types::{MessageRole, Part};
    use crate::a2a::core::types::MessageId;

    fn make_facade() -> TaskFacade {
        let bus = Arc::new(MessageBus::new(64));
        TaskFacade::new(
            Arc::new(RwLock::new(TaskManager::new(Arc::clone(&bus)))),
            Arc::new(RwLock::new(AgentRegistry::new(Arc::clone(&bus)))),
            Box::new(DefaultTaskRouter),
        )
    }

    fn make_message() -> TaskMessage {
        TaskMessage {
            id: MessageId::new(),
            role: MessageRole::User,
            parts: vec![Part::Text {
                text: "do something".to_owned(),
            }],
            metadata: None,
        }
    }

    #[tokio::test]
    async fn submit_task_creates_in_submitted_state() {
        let facade = make_facade();
        let task = facade
            .submit_task(make_message(), None, None, None)
            .await
            .expect("submit must succeed");

        assert_eq!(task.status.state, TaskState::Submitted);
    }

    #[tokio::test]
    async fn submit_then_get_returns_same_task() {
        let facade = make_facade();
        let task = facade
            .submit_task(make_message(), None, None, None)
            .await
            .expect("submit must succeed");

        let fetched = facade.get_task(&task.id).await.expect("get must succeed");
        assert_eq!(fetched.id, task.id);
    }

    #[tokio::test]
    async fn update_status_transitions_task() {
        let facade = make_facade();
        let task = facade
            .submit_task(make_message(), None, None, None)
            .await
            .expect("submit must succeed");

        let updated = facade
            .update_status(&task.id, TaskState::Working, None)
            .await
            .expect("transition must succeed");
        assert_eq!(updated.status.state, TaskState::Working);
    }

    #[tokio::test]
    async fn cancel_task_from_submitted() {
        let facade = make_facade();
        let task = facade
            .submit_task(make_message(), None, None, None)
            .await
            .expect("submit must succeed");

        let canceled = facade
            .cancel_task(&task.id, None)
            .await
            .expect("cancel must succeed");
        assert_eq!(canceled.status.state, TaskState::Canceled);
    }

    #[tokio::test]
    async fn list_tasks_empty_initially() {
        let facade = make_facade();
        let tasks = facade.list_tasks(&TaskFilter::default()).await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn get_unknown_task_returns_task_not_found() {
        let facade = make_facade();
        let err = facade
            .get_task(&TaskId::new())
            .await
            .expect_err("get of an unknown task must fail");

        assert!(
            matches!(err, FacadeError::Task(TaskError::TaskNotFound { .. })),
            "expected FacadeError::Task(TaskNotFound), got: {err:?}"
        );
    }

    #[tokio::test]
    async fn submit_routes_to_connected_agent() {
        let bus = Arc::new(MessageBus::new(64));
        let registry = Arc::new(RwLock::new(AgentRegistry::new(Arc::clone(&bus))));

        // Register a connected agent.
        let agent = {
            let mut reg = registry.write().await;
            reg.register("worker", None).expect("register must succeed")
        };

        let facade = TaskFacade::new(
            Arc::new(RwLock::new(TaskManager::new(Arc::clone(&bus)))),
            Arc::clone(&registry),
            Box::new(DefaultTaskRouter),
        );

        let task = facade
            .submit_task(make_message(), None, None, None)
            .await
            .expect("submit must succeed");

        assert_eq!(
            task.assignee,
            Some(agent.id),
            "task should be routed to the connected agent"
        );
    }
}
