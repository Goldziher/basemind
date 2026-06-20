//! Broadcast message bus for intra-process event propagation.
//!
//! [`MessageBus`] wraps a [`tokio::sync::broadcast`] channel and provides
//! typed [`Event`] delivery to any number of concurrent subscribers.
//!
//! Adapted from the upstream nexus bus: the chat / tool variants from the
//! source (`NewMessage`, `ToolRegistered`, `ToolDeregistered`) are dropped
//! here because basemind intentionally omits the `ChatMessage` / `ToolInfo`
//! types (see [`crate::a2a::core::types`]) and models those concerns
//! elsewhere. The agent-lifecycle and task variants are preserved verbatim.

use serde::Serialize;
use tokio::sync::broadcast;

use crate::a2a::core::task_types::{ArtifactId, Task, TaskId, TaskState};
use crate::a2a::core::types::{AgentId, AgentInfo};

/// Events propagated through the message bus.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A new agent has registered with the nexus.
    AgentRegistered(AgentInfo),
    /// An agent has been removed from the registry.
    AgentDeregistered(AgentId),
    /// An agent failed to heartbeat within the timeout window and has been
    /// flipped to [`AgentStatus::Disconnected`](crate::a2a::core::types::AgentStatus).
    AgentDisconnected(AgentInfo),
    /// A previously disconnected agent resumed heartbeating and is now
    /// [`AgentStatus::Connected`](crate::a2a::core::types::AgentStatus) again.
    AgentReconnected(AgentInfo),
    /// A new task was created.
    TaskCreated(Box<Task>),
    /// A task's state changed.
    TaskStatusChanged {
        task_id: TaskId,
        old_state: TaskState,
        new_state: TaskState,
    },
    /// An artifact was added to a task.
    TaskArtifactAdded {
        task_id: TaskId,
        artifact_id: ArtifactId,
    },
}

impl Event {
    /// The canonical event type string for SSE/WebSocket topic filtering.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::AgentRegistered(_) => "agent_registered",
            Self::AgentDeregistered(_) => "agent_deregistered",
            Self::AgentDisconnected(_) => "agent_disconnected",
            Self::AgentReconnected(_) => "agent_reconnected",
            Self::TaskCreated(_) => "task_created",
            Self::TaskStatusChanged { .. } => "task_status_changed",
            Self::TaskArtifactAdded { .. } => "task_artifact_added",
        }
    }
}

/// Internal broadcast channel for intra-process event propagation.
///
/// All events are sent once and fanned out to every active subscriber.
/// Subscribers that fall behind by more than `capacity` events will receive
/// a [`broadcast::error::RecvError::Lagged`] error on their next receive.
pub struct MessageBus {
    sender: broadcast::Sender<Event>,
}

impl MessageBus {
    /// Create a new [`MessageBus`] with the given channel `capacity`.
    ///
    /// `capacity` is the maximum number of events buffered before the
    /// oldest event is overwritten for lagging receivers.
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Publish an [`Event`] to all current subscribers.
    ///
    /// If there are no active subscribers the event is silently discarded.
    /// If subscribers exist but the channel is full because they have lagged
    /// behind, the slowest receivers will see [`broadcast::error::RecvError::Lagged`]
    /// and the lagged-out events are dropped.
    pub fn publish(&self, event: Event) {
        let event_type = event.event_type();
        if let Err(broadcast::error::SendError(_)) = self.sender.send(event) {
            // SendError is only returned when there are no receivers at all,
            // which is normal during startup or quiet windows — log at TRACE
            // rather than WARN to avoid noise.
            tracing::trace!(event_type, "no subscribers; bus event dropped");
        }
    }

    /// Subscribe to future events.
    ///
    /// The returned [`broadcast::Receiver`] will only receive events
    /// published **after** this call returns.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::a2a::core::task_types::{ContextId, Task, TaskMessage, TaskState, TaskStatus};
    use crate::a2a::core::types::{AgentId, AgentInfo, AgentStatus};

    fn make_agent_info() -> AgentInfo {
        AgentInfo {
            id: AgentId::new(),
            name: "test-agent".to_owned(),
            registered_at: Utc::now(),
            last_heartbeat_at: Utc::now(),
            status: AgentStatus::Connected,
            capabilities: None,
        }
    }

    fn make_task() -> Task {
        Task {
            id: TaskId::new(),
            context_id: ContextId::new(),
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: Utc::now(),
            },
            artifacts: Vec::new(),
            history: Vec::<TaskMessage>::new(),
            metadata: None,
            assignee: None,
            creator: None,
            deadline: None,
        }
    }

    #[tokio::test]
    async fn publish_and_subscribe_should_deliver_event_to_subscriber() {
        let bus = MessageBus::new(16);
        let mut rx = bus.subscribe();

        let info = make_agent_info();
        bus.publish(Event::AgentRegistered(info.clone()));

        let received = rx.recv().await.expect("subscriber must receive the event");
        let Event::AgentRegistered(received_info) = received else {
            panic!("expected AgentRegistered, got something else");
        };
        assert_eq!(
            received_info.id, info.id,
            "received agent id must match published agent id"
        );
    }

    #[tokio::test]
    async fn multiple_subscribers_should_each_receive_same_event() {
        let bus = MessageBus::new(16);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        let task = make_task();
        bus.publish(Event::TaskCreated(Box::new(task.clone())));

        let ev1 = rx1
            .recv()
            .await
            .expect("subscriber 1 must receive the event");
        let ev2 = rx2
            .recv()
            .await
            .expect("subscriber 2 must receive the event");

        let Event::TaskCreated(t1) = ev1 else {
            panic!("subscriber 1: expected TaskCreated");
        };
        let Event::TaskCreated(t2) = ev2 else {
            panic!("subscriber 2: expected TaskCreated");
        };

        assert_eq!(t1.id, task.id, "subscriber 1 task id must match");
        assert_eq!(t2.id, task.id, "subscriber 2 task id must match");
    }

    #[tokio::test]
    async fn subscriber_after_publish_should_miss_earlier_event() {
        let bus = MessageBus::new(16);

        // Publish before subscribing.
        bus.publish(Event::AgentDeregistered(AgentId::new()));

        let mut rx = bus.subscribe();

        // Nothing should be waiting for this late subscriber.
        let result = tokio::time::timeout(std::time::Duration::from_millis(20), rx.recv()).await;

        assert!(
            result.is_err(),
            "late subscriber must not receive events published before it subscribed"
        );
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_should_not_panic() {
        let bus = MessageBus::new(16);
        // No subscribers — must complete without panicking.
        bus.publish(Event::AgentRegistered(make_agent_info()));
        bus.publish(Event::TaskCreated(Box::new(make_task())));
    }

    #[test]
    fn event_type_strings_are_stable() {
        assert_eq!(
            Event::AgentDeregistered(AgentId::new()).event_type(),
            "agent_deregistered"
        );
        assert_eq!(
            Event::TaskStatusChanged {
                task_id: TaskId::new(),
                old_state: TaskState::Submitted,
                new_state: TaskState::Working,
            }
            .event_type(),
            "task_status_changed"
        );
    }
}
