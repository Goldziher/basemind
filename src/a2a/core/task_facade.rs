//! Unified task operations facade (ADR-014).
//!
//! [`TaskFacade`] is the single entry point for all task operations across
//! every transport adapter (MCP, gRPC, REST). It orchestrates validation,
//! state transitions, routing, and bus event publishing.

use std::sync::Arc;

use thiserror::Error;
use tokio::sync::RwLock;

use crate::a2a::core::task_manager::{TaskError, TaskManager};
use crate::a2a::core::task_types::{ContextId, Task, TaskFilter, TaskId, TaskMessage};
use crate::a2a::core::types::AgentId;

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors produced by [`TaskFacade`] operations.
///
/// The facade composes the task manager's focused, locally-scoped error:
/// every task-manager failure (create, cancel, lookup) surfaces as
/// [`FacadeError::Task`]. Agent registration, routing, and execution are out of
/// scope for the experimental A2A server, so no registry/router error variants
/// are carried.
#[derive(Debug, Error)]
pub enum FacadeError {
    /// A task-manager operation failed (create, cancel, lookup).
    #[error(transparent)]
    Task(#[from] TaskError),
}

// ── TaskFacade ──────────────────────────────────────────────────────────────────

/// Canonical task API used by all transport adapters.
///
/// Every protocol adapter (gRPC, MCP, REST) calls through the facade.
/// Protocol-specific type conversion happens at the adapter boundary.
pub struct TaskFacade {
    tasks: Arc<RwLock<TaskManager>>,
}

impl TaskFacade {
    /// Create a new facade backed by the given task manager.
    pub fn new(tasks: Arc<RwLock<TaskManager>>) -> Self {
        Self { tasks }
    }

    /// Submit a new task in the [`TaskState::Submitted`](crate::a2a::core::task_types::TaskState::Submitted)
    /// state.
    ///
    /// `assignee` is honoured verbatim when supplied; the experimental server
    /// has no agent registry or router, so unassigned tasks simply stay
    /// unassigned until an executor is built out.
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
        let mut mgr = self.tasks.write().await;
        let task =
            mgr.create_task_with_deadline(message, context_id, assignee, None, metadata, deadline)?;
        Ok(task)
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
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::a2a::core::bus::MessageBus;
    use crate::a2a::core::task_types::{MessageRole, Part, TaskState};
    use crate::a2a::core::types::MessageId;

    fn make_facade() -> TaskFacade {
        let bus = Arc::new(MessageBus::new(64));
        TaskFacade::new(Arc::new(RwLock::new(TaskManager::new(Arc::clone(&bus)))))
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
}
