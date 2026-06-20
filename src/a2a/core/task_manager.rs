//! Task manager — owns all in-memory task state and publishes task events.
//!
//! [`TaskManager`] is intentionally not wrapped in `Arc`/`RwLock`; locking
//! belongs at the server layer. All mutation is `&mut self`.

use std::sync::Arc;

use ahash::AHashMap;
use chrono::Utc;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::a2a::core::bus::{Event, MessageBus};
use crate::a2a::core::task_types::{
    Artifact, ArtifactId, ContextId, Task, TaskFilter, TaskId, TaskMessage, TaskState, TaskStatus,
};
use crate::a2a::core::types::AgentId;

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors produced by [`TaskManager`] operations.
///
/// Ported from the upstream nexus error enum, narrowed to the task-system
/// variants the manager actually raises. The upstream `Error` couples HTTP /
/// gRPC status codes and many unrelated domains into one type; that coupling is
/// nexus-specific and intentionally dropped here in favour of a focused,
/// task-scoped error.
#[derive(Debug, thiserror::Error)]
// Variant names are ported verbatim from the upstream nexus error enum; the
// `Task` prefix is part of the established task-system error vocabulary.
#[allow(clippy::enum_variant_names)]
pub enum TaskError {
    /// No task exists with the requested id.
    #[error("no task with id '{id}'")]
    TaskNotFound {
        /// The id that failed to resolve.
        id: String,
    },

    /// The requested state transition is not permitted by the state machine.
    #[error("invalid task transition for '{task_id}': {from} -> {to}")]
    TaskInvalidTransition {
        /// The task whose transition was rejected.
        task_id: String,
        /// The current state, formatted for display.
        from: String,
        /// The requested target state, formatted for display.
        to: String,
    },

    /// The task is already in a terminal state and cannot be modified.
    #[error("task '{task_id}' is in terminal state {state} and cannot be modified")]
    TaskAlreadyTerminal {
        /// The task that is already terminal.
        task_id: String,
        /// The terminal state, formatted for display.
        state: String,
    },
}

// ── Task-scoped events ────────────────────────────────────────────────────────

/// Events scoped to task lifecycle changes.
///
/// These are broadcast on a dedicated channel independent of the global
/// [`MessageBus`] so consumers can subscribe to task events alone.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// Variants mirror `bus::Event`'s task variants 1:1 (mapped in `publish`); the
// shared `Task` prefix is the intended, ported naming.
#[allow(clippy::enum_variant_names)]
pub enum TaskEvent {
    /// A new task was created.
    ///
    /// Carries an [`Arc<Task>`] shared with the mirrored [`Event::TaskCreated`]
    /// so fan-out across both channels is a refcount bump, not a deep clone.
    TaskCreated(Arc<Task>),
    /// A task's state changed.
    TaskStatusChanged {
        task_id: TaskId,
        old_state: TaskState,
        new_state: TaskState,
        /// Post-mutation task snapshot, shared via [`Arc`].
        task: Arc<Task>,
    },
    /// An artifact was appended to a task.
    TaskArtifactAdded {
        task_id: TaskId,
        artifact_id: ArtifactId,
        /// Post-mutation task snapshot, shared via [`Arc`].
        task: Arc<Task>,
    },
}

// ── TaskManager ───────────────────────────────────────────────────────────────

/// Manages tasks across the agent nexus.
///
/// Tasks are indexed by [`TaskId`] for O(1) lookup. A secondary
/// `context_index` maps each [`ContextId`] to its member task IDs for
/// context-scoped queries.
pub struct TaskManager {
    tasks: AHashMap<TaskId, Task>,
    context_index: AHashMap<ContextId, Vec<TaskId>>,
    bus: Arc<MessageBus>,
    event_tx: broadcast::Sender<TaskEvent>,
}

impl TaskManager {
    /// Create a new [`TaskManager`] backed by `bus`.
    pub fn new(bus: Arc<MessageBus>) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            tasks: AHashMap::new(),
            context_index: AHashMap::new(),
            bus,
            event_tx,
        }
    }

    /// Subscribe to task-lifecycle events.
    ///
    /// The returned receiver only captures events published **after** this
    /// call returns.
    pub fn subscribe(&self) -> broadcast::Receiver<TaskEvent> {
        self.event_tx.subscribe()
    }

    /// Create a new task from an initial message.
    ///
    /// A fresh [`TaskId`] is always generated. When `context_id` is `None` a
    /// new [`ContextId`] is generated automatically. The task enters the
    /// [`TaskState::Submitted`] state and a [`TaskEvent::TaskCreated`] event
    /// is published.
    pub fn create_task(
        &mut self,
        message: TaskMessage,
        context_id: Option<ContextId>,
        assignee: Option<AgentId>,
        creator: Option<AgentId>,
        metadata: Option<serde_json::Value>,
    ) -> Result<Task, TaskError> {
        self.create_task_with_deadline(message, context_id, assignee, creator, metadata, None)
    }

    /// Same as [`Self::create_task`] but accepts an explicit `deadline`.
    ///
    /// The watchdog periodically scans non-terminal tasks and transitions any
    /// whose `deadline` has passed to [`TaskState::Failed`].
    pub fn create_task_with_deadline(
        &mut self,
        message: TaskMessage,
        context_id: Option<ContextId>,
        assignee: Option<AgentId>,
        creator: Option<AgentId>,
        metadata: Option<serde_json::Value>,
        deadline: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Task, TaskError> {
        let id = TaskId::new();
        let context_id = context_id.unwrap_or_default();
        let now = Utc::now();

        let task = Task {
            id,
            context_id,
            status: TaskStatus {
                state: TaskState::Submitted,
                message: Some(message.clone()),
                timestamp: now,
            },
            history: vec![message],
            artifacts: Vec::new(),
            assignee,
            creator,
            metadata,
            deadline,
        };

        self.context_index.entry(context_id).or_default().push(id);
        // Snapshot once into an `Arc`. The map entry and the caller return each
        // need an owned `Task` (two unavoidable clones), but the event payload is
        // now an `Arc::clone` (refcount bump) instead of a deep `Task` clone per
        // bus subscriber — that fan-out clone is the cost this removes.
        let snapshot = Arc::new(task);
        self.tasks.insert(id, Task::clone(&snapshot));

        self.publish(TaskEvent::TaskCreated(Arc::clone(&snapshot)));
        Ok(Task::clone(&snapshot))
    }

    /// Look up a task by its [`TaskId`].
    pub fn get(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.get(id)
    }

    /// Look up a task mutably by its [`TaskId`].
    pub fn get_mut(&mut self, id: &TaskId) -> Option<&mut Task> {
        self.tasks.get_mut(id)
    }

    /// Return all tasks in unspecified order.
    pub fn list(&self) -> Vec<&Task> {
        self.tasks.values().collect()
    }

    /// Return tasks matching `filter`.
    ///
    /// All non-`None` filter fields must match; unset fields are ignored.
    pub fn list_filtered(&self, filter: &TaskFilter) -> Vec<&Task> {
        // Use the context index for O(1) context-scoped lookups when possible.
        let candidates: Box<dyn Iterator<Item = &Task>> = match &filter.context_id {
            Some(ctx) => {
                let ids = self.context_index.get(ctx);
                Box::new(
                    ids.into_iter()
                        .flatten()
                        .filter_map(|id| self.tasks.get(id)),
                )
            }
            None => Box::new(self.tasks.values()),
        };

        candidates
            .filter(|t| {
                filter.state.as_ref().is_none_or(|s| &t.status.state == s)
                    && filter
                        .assignee
                        .as_ref()
                        .is_none_or(|a| t.assignee.as_ref() == Some(a))
            })
            .collect()
    }

    /// Transition a task to `new_state`, optionally appending a status message.
    ///
    /// Validates the transition via [`TaskState::can_transition_to`].
    ///
    /// # Errors
    ///
    /// - [`TaskError::TaskNotFound`] — no task with `task_id`.
    /// - [`TaskError::TaskAlreadyTerminal`] — task is in a terminal state.
    /// - [`TaskError::TaskInvalidTransition`] — the transition is not allowed.
    pub fn update_status(
        &mut self,
        task_id: &TaskId,
        new_state: TaskState,
        message: Option<TaskMessage>,
    ) -> Result<Task, TaskError> {
        // Single mutable lookup avoids double-borrow and the expect("checked above") pattern.
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| TaskError::TaskNotFound {
                id: task_id.to_string(),
            })?;

        let old_state = task.status.state;

        if old_state.is_terminal() {
            return Err(TaskError::TaskAlreadyTerminal {
                task_id: task_id.to_string(),
                state: format!("{old_state:?}"),
            });
        }

        if !old_state.can_transition_to(new_state) {
            return Err(TaskError::TaskInvalidTransition {
                task_id: task_id.to_string(),
                from: format!("{old_state:?}"),
                to: format!("{new_state:?}"),
            });
        }

        let now = Utc::now();

        task.status = TaskStatus {
            state: new_state,
            message: message.clone(),
            timestamp: now,
        };

        if let Some(msg) = message {
            task.history.push(msg);
        }

        // Snapshot once into an `Arc`; reuse it (refcount bump) for the event
        // and clone the inner once for the owned return value.
        let snapshot = Arc::new(task.clone());
        self.publish(TaskEvent::TaskStatusChanged {
            task_id: *task_id,
            old_state,
            new_state,
            task: Arc::clone(&snapshot),
        });

        Ok(Task::clone(&snapshot))
    }

    /// Append an artifact to a task.
    ///
    /// # Errors
    ///
    /// - [`TaskError::TaskNotFound`] — no task with `task_id`.
    /// - [`TaskError::TaskAlreadyTerminal`] — task is in a terminal state.
    pub fn add_artifact(
        &mut self,
        task_id: &TaskId,
        artifact: Artifact,
    ) -> Result<Task, TaskError> {
        let artifact_id = artifact.id;
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| TaskError::TaskNotFound {
                id: task_id.to_string(),
            })?;

        if task.status.state.is_terminal() {
            return Err(TaskError::TaskAlreadyTerminal {
                task_id: task_id.to_string(),
                state: format!("{:?}", task.status.state),
            });
        }

        task.artifacts.push(artifact);

        // Snapshot once into an `Arc`; reuse it (refcount bump) for the event
        // and clone the inner once for the owned return value.
        let snapshot = Arc::new(task.clone());
        self.publish(TaskEvent::TaskArtifactAdded {
            task_id: *task_id,
            artifact_id,
            task: Arc::clone(&snapshot),
        });

        Ok(Task::clone(&snapshot))
    }

    /// Cancel a task by transitioning it to [`TaskState::Canceled`].
    ///
    /// # Errors
    ///
    /// Propagates errors from [`Self::update_status`].
    pub fn cancel(
        &mut self,
        task_id: &TaskId,
        message: Option<TaskMessage>,
    ) -> Result<Task, TaskError> {
        self.update_status(task_id, TaskState::Canceled, message)
    }

    /// Return active (non-terminal) tasks assigned to `agent_id`.
    ///
    /// Terminal states are [`TaskState::Completed`], [`TaskState::Canceled`],
    /// [`TaskState::Failed`], and [`TaskState::Rejected`].
    pub fn tasks_for_agent(&self, agent_id: &AgentId) -> Vec<&Task> {
        self.tasks
            .values()
            .filter(|t| t.assignee.as_ref() == Some(agent_id) && !t.status.state.is_terminal())
            .collect()
    }

    /// Clear all task state and repopulate from `tasks`.
    ///
    /// Used during daemon startup to restore persisted state. No events are
    /// published.
    pub fn restore(&mut self, tasks: Vec<Task>) {
        self.tasks.clear();
        self.context_index.clear();

        for task in tasks {
            self.context_index
                .entry(task.context_id)
                .or_default()
                .push(task.id);
            self.tasks.insert(task.id, task);
        }
    }

    // ── private helpers ───────────────────────────────────────────────────────

    /// Publish a task event on the task-scoped channel and mirror it to the
    /// global bus so SSE/WebSocket subscribers see task events too.
    fn publish(&self, event: TaskEvent) {
        // Mirror to global bus. The task payloads are shared via `Arc`, so the
        // mirror is a refcount bump rather than a deep `Task` clone.
        let bus_event = match &event {
            TaskEvent::TaskCreated(task) => Event::TaskCreated(Arc::clone(task)),
            TaskEvent::TaskStatusChanged {
                task_id,
                old_state,
                new_state,
                task,
            } => Event::TaskStatusChanged {
                task_id: *task_id,
                old_state: *old_state,
                new_state: *new_state,
                task: Arc::clone(task),
            },
            TaskEvent::TaskArtifactAdded {
                task_id,
                artifact_id,
                task,
            } => Event::TaskArtifactAdded {
                task_id: *task_id,
                artifact_id: *artifact_id,
                task: Arc::clone(task),
            },
        };
        self.bus.publish(bus_event);
        // Task-scoped channel. SendError on a tokio broadcast means no
        // receivers — log at TRACE to avoid noise during startup.
        if let Err(tokio::sync::broadcast::error::SendError(dropped)) = self.event_tx.send(event) {
            let event_type = match &dropped {
                TaskEvent::TaskCreated(_) => "task_created",
                TaskEvent::TaskStatusChanged { .. } => "task_status_changed",
                TaskEvent::TaskArtifactAdded { .. } => "task_artifact_added",
            };
            tracing::trace!(event_type, "no task-event subscribers; event dropped");
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::core::task_types::{MessageRole, Part};
    use crate::a2a::core::types::MessageId;

    fn make_manager() -> TaskManager {
        let bus = Arc::new(MessageBus::new(16));
        TaskManager::new(bus)
    }

    fn make_message() -> TaskMessage {
        TaskMessage {
            id: MessageId::new(),
            role: MessageRole::User,
            parts: vec![Part::Text {
                text: "hello".to_owned(),
            }],
            metadata: None,
        }
    }

    fn make_artifact() -> Artifact {
        Artifact {
            id: ArtifactId::new(),
            name: Some("output.txt".to_owned()),
            description: None,
            parts: vec![Part::Text {
                text: "result".to_owned(),
            }],
            metadata: None,
        }
    }

    // ── create_task ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_task_succeeds() {
        let mut mgr = make_manager();
        let task = mgr
            .create_task(make_message(), None, None, None, None)
            .expect("create_task must succeed");

        assert_eq!(
            task.status.state,
            TaskState::Submitted,
            "new task must start in Submitted state"
        );
        assert_eq!(
            task.history.len(),
            1,
            "history must contain the initial message"
        );
    }

    #[tokio::test]
    async fn create_task_generates_context_id_when_none() {
        let mut mgr = make_manager();
        let task1 = mgr
            .create_task(make_message(), None, None, None, None)
            .expect("first create_task must succeed");
        let task2 = mgr
            .create_task(make_message(), None, None, None, None)
            .expect("second create_task must succeed");

        assert_ne!(
            task1.context_id, task2.context_id,
            "each task with no explicit context_id must get a unique one"
        );
    }

    // ── get ───────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_returns_created_task() {
        let mut mgr = make_manager();
        let task = mgr
            .create_task(make_message(), None, None, None, None)
            .expect("create_task must succeed");

        let found = mgr.get(&task.id).expect("get must return the created task");
        assert_eq!(found.id, task.id, "retrieved task id must match");
    }

    // ── list_filtered ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_filtered_by_state() {
        let mut mgr = make_manager();
        let task1 = mgr
            .create_task(make_message(), None, None, None, None)
            .expect("first create must succeed");
        let task2 = mgr
            .create_task(make_message(), None, None, None, None)
            .expect("second create must succeed");

        mgr.update_status(&task2.id, TaskState::Working, None)
            .expect("transition to Working must succeed");

        let filter = TaskFilter {
            state: Some(TaskState::Submitted),
            context_id: None,
            assignee: None,
        };
        let results = mgr.list_filtered(&filter);

        assert_eq!(results.len(), 1, "only one task should be Submitted");
        assert_eq!(results[0].id, task1.id, "the Submitted task must be task1");
    }

    // ── update_status ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn update_status_valid_transition() {
        let mut mgr = make_manager();
        let task = mgr
            .create_task(make_message(), None, None, None, None)
            .expect("create must succeed");

        let updated = mgr
            .update_status(&task.id, TaskState::Working, None)
            .expect("Submitted → Working is a valid transition");

        assert_eq!(
            updated.status.state,
            TaskState::Working,
            "task must be in Working state after transition"
        );
    }

    #[tokio::test]
    async fn update_status_invalid_transition() {
        let mut mgr = make_manager();
        let task = mgr
            .create_task(make_message(), None, None, None, None)
            .expect("create must succeed");

        let err = mgr
            .update_status(&task.id, TaskState::Completed, None)
            .expect_err("Submitted → Completed must be rejected");

        assert!(
            matches!(err, TaskError::TaskInvalidTransition { .. }),
            "expected TaskInvalidTransition, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn update_status_terminal_rejects() {
        let mut mgr = make_manager();
        let task = mgr
            .create_task(make_message(), None, None, None, None)
            .expect("create must succeed");

        mgr.update_status(&task.id, TaskState::Working, None)
            .expect("Submitted → Working");
        mgr.update_status(&task.id, TaskState::Completed, None)
            .expect("Working → Completed");

        let err = mgr
            .update_status(&task.id, TaskState::Working, None)
            .expect_err("Completed → Working must be rejected");

        assert!(
            matches!(err, TaskError::TaskAlreadyTerminal { .. }),
            "expected TaskAlreadyTerminal, got: {err:?}"
        );
    }

    // ── add_artifact ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn add_artifact_succeeds() {
        let mut mgr = make_manager();
        let task = mgr
            .create_task(make_message(), None, None, None, None)
            .expect("create must succeed");

        let artifact = make_artifact();
        let artifact_id = artifact.id;

        let updated = mgr
            .add_artifact(&task.id, artifact)
            .expect("add_artifact must succeed");

        assert_eq!(
            updated.artifacts.len(),
            1,
            "task must have exactly one artifact"
        );
        assert_eq!(
            updated.artifacts[0].id, artifact_id,
            "artifact id must match"
        );
    }

    // ── cancel ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn cancel_from_working_succeeds() {
        let mut mgr = make_manager();
        let task = mgr
            .create_task(make_message(), None, None, None, None)
            .expect("create must succeed");

        mgr.update_status(&task.id, TaskState::Working, None)
            .expect("Submitted → Working");

        let canceled = mgr
            .cancel(&task.id, None)
            .expect("cancel must succeed from Working");

        assert_eq!(
            canceled.status.state,
            TaskState::Canceled,
            "task must be in Canceled state after cancel()"
        );
    }

    // ── tasks_for_agent ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn tasks_for_agent_returns_active_only() {
        let mut mgr = make_manager();
        let agent = AgentId::new();

        let task1 = mgr
            .create_task(make_message(), None, Some(agent), None, None)
            .expect("create task1 must succeed");
        let task2 = mgr
            .create_task(make_message(), None, Some(agent), None, None)
            .expect("create task2 must succeed");

        // Complete task1: Submitted → Working → Completed
        mgr.update_status(&task1.id, TaskState::Working, None)
            .expect("Submitted → Working");
        mgr.update_status(&task1.id, TaskState::Completed, None)
            .expect("Working → Completed");

        let active = mgr.tasks_for_agent(&agent);
        assert_eq!(
            active.len(),
            1,
            "only the non-terminal task must be returned"
        );
        assert_eq!(active[0].id, task2.id, "the active task must be task2");
    }

    // ── restore ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn restore_populates_tasks() {
        let mut mgr = make_manager();
        let original = mgr
            .create_task(make_message(), None, None, None, None)
            .expect("create must succeed");
        let snapshot = vec![original.clone()];

        let mut mgr2 = make_manager();
        mgr2.restore(snapshot);

        let found = mgr2
            .get(&original.id)
            .expect("restored task must be retrievable by id");
        assert_eq!(found.id, original.id, "restored task id must match");
        assert_eq!(
            found.context_id, original.context_id,
            "restored context_id must match"
        );
    }
}
