//! Agent and task watchdog.
//!
//! Spawns a background ticker that:
//!
//! 1. Flips agents to [`AgentStatus::Disconnected`] when they fail to
//!    heartbeat within `timeout`.
//! 2. Fails any non-terminal task assigned to a freshly-disconnected
//!    agent so the work is visibly halted rather than silently stuck.
//! 3. Fails tasks whose [`Task::deadline`](crate::a2a::core::task_types::Task::deadline)
//!    has passed (when set).
//!
//! The watchdog is intentionally simple — one task, one tick interval,
//! linear scans. Single-host A2A workloads stay well below the threshold
//! where a smarter index would matter.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::a2a::core::registry::AgentRegistry;
use crate::a2a::core::task_manager::TaskManager;
use crate::a2a::core::task_types::{MessageRole, Part, TaskFilter, TaskId, TaskMessage, TaskState};
use crate::a2a::core::types::{AgentStatus, MessageId};

/// Spawn the agent + task watchdog and return its `JoinHandle`.
///
/// The task ticks every `interval` and exits when `cancel` fires. `timeout`
/// is the silence window after which an agent is flipped to
/// [`AgentStatus::Disconnected`]; tasks assigned to that agent are
/// transitioned to [`TaskState::Failed`] (or [`TaskState::Rejected`] when they
/// never started) in the same pass.
pub fn spawn(
    registry: Arc<RwLock<AgentRegistry>>,
    tasks: Arc<RwLock<TaskManager>>,
    interval: Duration,
    timeout: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately; skip it so newly-registered agents
        // get at least one full window before being scrutinised.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    sweep(&registry, &tasks, timeout).await;
                }
                _ = cancel.cancelled() => {
                    tracing::debug!("watchdog received shutdown signal");
                    return;
                }
            }
        }
    })
}

/// One scan: flip stale agents and fail their in-flight tasks plus any
/// tasks whose own deadlines have passed.
async fn sweep(
    registry: &Arc<RwLock<AgentRegistry>>,
    tasks: &Arc<RwLock<TaskManager>>,
    timeout: Duration,
) {
    let now = Utc::now();
    let cutoff = now
        - chrono::Duration::from_std(timeout)
            .unwrap_or_else(|_| chrono::Duration::seconds(i64::MAX / 2));

    // ── Phase 1: collect agents whose last heartbeat is older than cutoff
    //    (read lock only).
    let stale_ids: Vec<_> = {
        let reg = registry.read().await;
        reg.list()
            .iter()
            .filter(|info| info.status == AgentStatus::Connected && info.last_heartbeat_at < cutoff)
            .map(|info| info.id)
            .collect()
    };

    if stale_ids.is_empty() {
        // Skip the agent write lock when nothing changed.
        // Task deadlines are still scanned below.
    } else {
        // ── Phase 2: flip each stale agent under a write lock.
        let mut reg = registry.write().await;
        for id in &stale_ids {
            if let Some(info) = reg.mark_disconnected(id) {
                tracing::info!(
                    agent = %info.name,
                    last_heartbeat = %info.last_heartbeat_at,
                    "agent flipped to Disconnected after heartbeat timeout"
                );
            }
        }
        drop(reg);

        // ── Phase 3: fail in-flight tasks for those agents.
        for id in &stale_ids {
            let mut mgr = tasks.write().await;
            let in_flight: Vec<(TaskId, TaskState)> = mgr
                .list_filtered(&TaskFilter {
                    assignee: Some(*id),
                    ..TaskFilter::default()
                })
                .into_iter()
                .filter(|t| !t.status.state.is_terminal())
                .map(|t| (t.id, t.status.state))
                .collect();

            for (task_id, current) in in_flight {
                // Submitted tasks haven't started yet → Rejected.
                // Working / Interrupted tasks → Failed.
                let target = if current == TaskState::Submitted {
                    TaskState::Rejected
                } else {
                    TaskState::Failed
                };
                let reason = TaskMessage {
                    id: MessageId::new(),
                    role: MessageRole::Agent,
                    parts: vec![Part::Text {
                        text: format!("assignee {id} disconnected"),
                    }],
                    metadata: None,
                };
                if let Err(e) = mgr.update_status(&task_id, target, Some(reason)) {
                    tracing::warn!(
                        error = %e,
                        %task_id,
                        ?current,
                        ?target,
                        "failed to mark task on assignee disconnect"
                    );
                }
            }
        }
    }

    // ── Phase 4: deadline-driven task failure (orthogonal to agent
    //    disconnects — a task can also fail because its own deadline expired
    //    while its assignee is healthy).
    let expired: Vec<(TaskId, TaskState)> = {
        let mgr = tasks.read().await;
        mgr.list()
            .iter()
            .filter(|t| !t.status.state.is_terminal() && t.deadline.is_some_and(|d| d < now))
            .map(|t| (t.id, t.status.state))
            .collect()
    };

    if !expired.is_empty() {
        let mut mgr = tasks.write().await;
        for (task_id, current) in expired {
            // Same Submitted → Rejected, else → Failed split as for the
            // disconnected-assignee path.
            let target = if current == TaskState::Submitted {
                TaskState::Rejected
            } else {
                TaskState::Failed
            };
            let reason = TaskMessage {
                id: MessageId::new(),
                role: MessageRole::Agent,
                parts: vec![Part::Text {
                    text: "task deadline exceeded".to_owned(),
                }],
                metadata: None,
            };
            if let Err(e) = mgr.update_status(&task_id, target, Some(reason)) {
                tracing::warn!(error = %e, %task_id, "failed to mark deadline-expired task");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::core::bus::MessageBus;
    use crate::a2a::core::registry::AgentRegistry;

    // Uses real tokio time (no `start_paused`) because the watchdog compares
    // `chrono::Utc::now()` against `info.last_heartbeat_at`, which is also
    // wall-clock based. Pausing tokio's virtual clock would not advance
    // chrono's wall clock and the test would race forever.

    #[tokio::test]
    async fn watchdog_flips_stale_agent_to_disconnected() {
        let bus = Arc::new(MessageBus::new(64));
        let registry = Arc::new(RwLock::new(AgentRegistry::new(Arc::clone(&bus))));
        let tasks = Arc::new(RwLock::new(TaskManager::new(Arc::clone(&bus))));

        let agent_id = {
            let mut reg = registry.write().await;
            reg.register("worker", None).unwrap().id
        };

        let cancel = CancellationToken::new();
        let handle = spawn(
            Arc::clone(&registry),
            Arc::clone(&tasks),
            Duration::from_millis(50),
            Duration::from_millis(80),
            cancel.clone(),
        );

        // Wait long enough for at least one tick after the heartbeat goes
        // stale: 80ms timeout + 50ms tick + slack.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let info = registry.read().await.get(&agent_id).cloned().unwrap();
        assert_eq!(
            info.status,
            AgentStatus::Disconnected,
            "watchdog must flip stale agent"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn heartbeat_resets_disconnect_timer() {
        let bus = Arc::new(MessageBus::new(64));
        let registry = Arc::new(RwLock::new(AgentRegistry::new(Arc::clone(&bus))));
        let tasks = Arc::new(RwLock::new(TaskManager::new(Arc::clone(&bus))));

        let agent_id = {
            let mut reg = registry.write().await;
            reg.register("worker", None).unwrap().id
        };

        let cancel = CancellationToken::new();
        let handle = spawn(
            Arc::clone(&registry),
            Arc::clone(&tasks),
            Duration::from_millis(50),
            Duration::from_millis(200),
            cancel.clone(),
        );

        // Send fresh heartbeats faster than the timeout — agent must stay
        // Connected throughout.
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut reg = registry.write().await;
            reg.heartbeat(&agent_id).unwrap();
        }

        let status = registry.read().await.get(&agent_id).unwrap().status.clone();
        assert_eq!(
            status,
            AgentStatus::Connected,
            "heartbeats must keep the agent connected"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn watchdog_fails_in_flight_tasks_for_disconnected_agent() {
        use crate::a2a::core::task_types::{MessageRole, Part, TaskMessage};
        use crate::a2a::core::types::MessageId;

        let bus = Arc::new(MessageBus::new(64));
        let registry = Arc::new(RwLock::new(AgentRegistry::new(Arc::clone(&bus))));
        let tasks = Arc::new(RwLock::new(TaskManager::new(Arc::clone(&bus))));

        // Register an agent and create a task assigned to it.
        let agent_id = {
            let mut reg = registry.write().await;
            reg.register("worker", None).unwrap().id
        };
        let task_id = {
            let mut mgr = tasks.write().await;
            let msg = TaskMessage {
                id: MessageId::new(),
                role: MessageRole::User,
                parts: vec![Part::Text {
                    text: "do work".into(),
                }],
                metadata: None,
            };
            mgr.create_task(msg, None, Some(agent_id), None, None)
                .unwrap()
                .id
        };

        let cancel = CancellationToken::new();
        let handle = spawn(
            Arc::clone(&registry),
            Arc::clone(&tasks),
            Duration::from_millis(50),
            Duration::from_millis(80),
            cancel.clone(),
        );

        tokio::time::sleep(Duration::from_millis(300)).await;

        let task = tasks.read().await.get(&task_id).cloned().unwrap();
        // Submitted-but-never-started → Rejected. Working tasks would go to
        // Failed; a separate test covers that split.
        assert_eq!(
            task.status.state,
            TaskState::Rejected,
            "submitted task must be Rejected when assignee disconnects pre-pickup"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn watchdog_fails_working_task_when_assignee_disconnects() {
        use crate::a2a::core::task_types::{MessageRole, Part, TaskMessage};
        use crate::a2a::core::types::MessageId;

        let bus = Arc::new(MessageBus::new(64));
        let registry = Arc::new(RwLock::new(AgentRegistry::new(Arc::clone(&bus))));
        let tasks = Arc::new(RwLock::new(TaskManager::new(Arc::clone(&bus))));

        let agent_id = {
            let mut reg = registry.write().await;
            reg.register("worker", None).unwrap().id
        };
        let task_id = {
            let mut mgr = tasks.write().await;
            let msg = TaskMessage {
                id: MessageId::new(),
                role: MessageRole::User,
                parts: vec![Part::Text { text: "x".into() }],
                metadata: None,
            };
            let task = mgr
                .create_task(msg, None, Some(agent_id), None, None)
                .unwrap();
            // Move the task into Working.
            mgr.update_status(&task.id, TaskState::Working, None)
                .unwrap();
            task.id
        };

        let cancel = CancellationToken::new();
        let handle = spawn(
            Arc::clone(&registry),
            Arc::clone(&tasks),
            Duration::from_millis(50),
            Duration::from_millis(80),
            cancel.clone(),
        );

        tokio::time::sleep(Duration::from_millis(300)).await;

        let task = tasks.read().await.get(&task_id).cloned().unwrap();
        assert_eq!(
            task.status.state,
            TaskState::Failed,
            "working task must be Failed when assignee disconnects"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn watchdog_fails_tasks_past_deadline() {
        use crate::a2a::core::task_types::{MessageRole, Part, TaskMessage};
        use crate::a2a::core::types::MessageId;

        let bus = Arc::new(MessageBus::new(64));
        let registry = Arc::new(RwLock::new(AgentRegistry::new(Arc::clone(&bus))));
        let tasks = Arc::new(RwLock::new(TaskManager::new(Arc::clone(&bus))));

        let task_id = {
            let mut mgr = tasks.write().await;
            let msg = TaskMessage {
                id: MessageId::new(),
                role: MessageRole::User,
                parts: vec![Part::Text { text: "x".into() }],
                metadata: None,
            };
            // Deadline already in the past.
            mgr.create_task_with_deadline(
                msg,
                None,
                None,
                None,
                None,
                Some(chrono::Utc::now() - chrono::Duration::seconds(1)),
            )
            .unwrap()
            .id
        };

        let cancel = CancellationToken::new();
        let handle = spawn(
            Arc::clone(&registry),
            Arc::clone(&tasks),
            Duration::from_millis(50),
            // Long agent timeout so the task watchdog phase is what fires.
            Duration::from_secs(60),
            cancel.clone(),
        );

        tokio::time::sleep(Duration::from_millis(150)).await;

        let task = tasks.read().await.get(&task_id).cloned().unwrap();
        // Submitted-with-past-deadline → Rejected (mirror disconnect path).
        assert_eq!(
            task.status.state,
            TaskState::Rejected,
            "submitted task with past deadline must be Rejected by the watchdog"
        );

        cancel.cancel();
        let _ = handle.await;
    }
}
