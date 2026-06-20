//! Task router — selects which agent should handle a given task.
//!
//! The [`TaskRouter`] trait is object-safe so that alternative routing
//! strategies can be swapped in without touching the server layer.

use crate::a2a::core::task_types::Task;
use crate::a2a::core::types::{AgentId, AgentInfo, AgentStatus};

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Selects an agent to handle a [`Task`] from a slice of candidate agents.
pub trait TaskRouter: Send + Sync {
    /// Return the [`AgentId`] of the selected agent, or `None` when no
    /// suitable agent is available.
    fn select_agent(&self, task: &Task, agents: &[&AgentInfo]) -> Option<AgentId>;
}

// ── DefaultTaskRouter ─────────────────────────────────────────────────────────

/// Default routing strategy used by the nexus.
///
/// Selection order (ADR-013):
///
/// 1. **Explicit assignment** — if the task's `assignee` is connected, return
///    it immediately.
/// 2. **Capability matching** — if the task metadata contains `required_tags`,
///    find a connected agent whose `capabilities.skill_tags` satisfy all tags.
/// 3. **First connected agent** — fall back to the first
///    [`AgentStatus::Connected`] agent in the slice.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultTaskRouter;

impl TaskRouter for DefaultTaskRouter {
    fn select_agent(&self, task: &Task, agents: &[&AgentInfo]) -> Option<AgentId> {
        // 1. Honour an explicit assignment when the assignee is connected.
        if let Some(assignee) = task.assignee
            && agents
                .iter()
                .any(|a| a.id == assignee && a.status == AgentStatus::Connected)
        {
            return Some(assignee);
        }

        // 2. Capability matching: find connected agents whose skill_tags
        //    satisfy all required_tags from task metadata.
        if let Some(ref metadata) = task.metadata
            && let Some(tags) = metadata.get("required_tags").and_then(|v| v.as_array())
        {
            let required: Vec<&str> = tags.iter().filter_map(|t| t.as_str()).collect();
            if !required.is_empty() {
                // When required_tags are set, only agents satisfying all tags
                // are eligible. Return None if no capable agent is connected
                // rather than silently assigning to an incapable agent.
                return agents
                    .iter()
                    .find(|a| {
                        a.status == AgentStatus::Connected
                            && a.capabilities.as_ref().is_some_and(|caps| {
                                required
                                    .iter()
                                    .all(|tag| caps.skill_tags.iter().any(|t| t == tag))
                            })
                    })
                    .map(|a| a.id);
            }
        }

        // 3. First connected agent (no required_tags constraint).
        agents
            .iter()
            .find(|a| a.status == AgentStatus::Connected)
            .map(|a| a.id)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::a2a::core::task_types::{
        ContextId, MessageRole, Part, Task, TaskId, TaskMessage, TaskState, TaskStatus,
    };
    use crate::a2a::core::types::MessageId;

    fn make_agent(id: AgentId, status: AgentStatus) -> AgentInfo {
        AgentInfo {
            id,
            name: "test-agent".to_owned(),
            registered_at: Utc::now(),
            last_heartbeat_at: Utc::now(),
            status,
            capabilities: None,
        }
    }

    fn make_task_inner(assignee: Option<AgentId>, metadata: Option<serde_json::Value>) -> Task {
        Task {
            id: TaskId::new(),
            context_id: ContextId::new(),
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: Utc::now(),
            },
            artifacts: vec![],
            history: vec![TaskMessage {
                id: MessageId::new(),
                role: MessageRole::User,
                parts: vec![Part::Text {
                    text: "do something".to_owned(),
                }],
                metadata: None,
            }],
            metadata,
            assignee,
            creator: None,
            deadline: None,
        }
    }

    fn make_task(assignee: Option<AgentId>) -> Task {
        make_task_inner(assignee, None)
    }

    #[test]
    fn explicit_assignment_returns_assignee() {
        let id = AgentId::new();
        let agent = make_agent(id, AgentStatus::Connected);
        let task = make_task(Some(id));
        let router = DefaultTaskRouter;

        let selected = router.select_agent(&task, &[&agent]);

        assert_eq!(
            selected,
            Some(id),
            "should return the explicitly assigned connected agent"
        );
    }

    #[test]
    fn explicit_assignment_skips_disconnected() {
        let id = AgentId::new();
        let agent = make_agent(id, AgentStatus::Disconnected);
        let task = make_task(Some(id));
        let router = DefaultTaskRouter;

        let selected = router.select_agent(&task, &[&agent]);

        assert_eq!(
            selected, None,
            "should return None when assignee is Disconnected and no other agents available"
        );
    }

    #[test]
    fn falls_back_to_connected_agent() {
        let id = AgentId::new();
        let connected = make_agent(id, AgentStatus::Connected);
        let task = make_task(None);
        let router = DefaultTaskRouter;

        let selected = router.select_agent(&task, &[&connected]);

        assert_eq!(
            selected,
            Some(id),
            "should fall back to the first connected agent when no assignee"
        );
    }

    // ── capability matching ──────────────────────────────────────────────────

    fn make_agent_with_tags(id: AgentId, status: AgentStatus, tags: Vec<&str>) -> AgentInfo {
        use crate::a2a::core::task_types::AgentCapabilities;
        AgentInfo {
            id,
            name: "tagged-agent".to_owned(),
            registered_at: Utc::now(),
            last_heartbeat_at: Utc::now(),
            status,
            capabilities: Some(AgentCapabilities {
                supported_input_modes: vec![],
                supported_output_modes: vec![],
                streaming: false,
                skill_tags: tags.into_iter().map(String::from).collect(),
            }),
        }
    }

    fn make_task_with_tags(tags: Vec<&str>) -> Task {
        let metadata = serde_json::json!({ "required_tags": tags });
        make_task_inner(None, Some(metadata))
    }

    #[test]
    fn capability_matching_selects_tagged_agent() {
        let capable_id = AgentId::new();
        let plain_id = AgentId::new();
        let capable = make_agent_with_tags(capable_id, AgentStatus::Connected, vec!["code.review"]);
        let plain = make_agent(plain_id, AgentStatus::Connected);
        let task = make_task_with_tags(vec!["code.review"]);
        let router = DefaultTaskRouter;

        let selected = router.select_agent(&task, &[&plain, &capable]);

        assert_eq!(
            selected,
            Some(capable_id),
            "should select agent with matching skill tags"
        );
    }

    #[test]
    fn capability_matching_skips_disconnected_agent() {
        let capable_id = AgentId::new();
        let fallback_id = AgentId::new();
        let capable =
            make_agent_with_tags(capable_id, AgentStatus::Disconnected, vec!["code.review"]);
        let fallback = make_agent(fallback_id, AgentStatus::Connected);
        let task = make_task_with_tags(vec!["code.review"]);
        let router = DefaultTaskRouter;

        let selected = router.select_agent(&task, &[&capable, &fallback]);

        assert_eq!(
            selected, None,
            "should return None when only capable agent is disconnected"
        );
    }

    #[test]
    fn no_capability_match_returns_none() {
        let id = AgentId::new();
        let agent = make_agent_with_tags(id, AgentStatus::Connected, vec!["code.fix"]);
        let task = make_task_with_tags(vec!["code.review"]);
        let router = DefaultTaskRouter;

        let selected = router.select_agent(&task, &[&agent]);

        assert_eq!(
            selected, None,
            "should return None when required_tags are unmatched"
        );
    }

    #[test]
    fn empty_required_tags_skips_matching() {
        let id = AgentId::new();
        let agent = make_agent(id, AgentStatus::Connected);
        let task = make_task_with_tags(vec![]);
        let router = DefaultTaskRouter;

        let selected = router.select_agent(&task, &[&agent]);

        assert_eq!(
            selected,
            Some(id),
            "empty required_tags should skip capability matching"
        );
    }
}
