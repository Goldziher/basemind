//! Task router — selects which agent should handle a given task.
//!
//! The [`TaskRouter`] trait is object-safe so that alternative routing
//! strategies can be swapped in without touching the server layer.

use crate::a2a::core::task_types::ContextId;
use crate::a2a::core::types::{AgentId, AgentInfo, AgentStatus};

// ── Route context ─────────────────────────────────────────────────────────────

/// The slice of a prospective task that routing actually inspects.
///
/// Passing this borrow instead of a synthetic [`Task`](crate::a2a::core::task_types::Task)
/// lets the facade route a task *before* it is created, with no throwaway
/// allocation of a fully-populated task just to read a couple of fields.
#[derive(Clone, Copy, Debug)]
pub struct TaskRouteContext<'a> {
    /// An explicitly pinned assignee, if the caller requested one.
    pub assignee: Option<AgentId>,
    /// The (resolved) context the task will belong to.
    pub context_id: ContextId,
    /// Caller-supplied metadata; routing reads `required_tags` from it.
    pub metadata: Option<&'a serde_json::Value>,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Selects an agent to handle a prospective task from a slice of candidates.
pub trait TaskRouter: Send + Sync {
    /// Return the [`AgentId`] of the selected agent, or `None` when no
    /// suitable agent is available.
    fn select_agent(
        &self,
        context: &TaskRouteContext<'_>,
        agents: &[&AgentInfo],
    ) -> Option<AgentId>;
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
    fn select_agent(
        &self,
        context: &TaskRouteContext<'_>,
        agents: &[&AgentInfo],
    ) -> Option<AgentId> {
        // 1. Honour an explicit assignment when the assignee is connected.
        if let Some(assignee) = context.assignee
            && agents
                .iter()
                .any(|a| a.id == assignee && a.status == AgentStatus::Connected)
        {
            return Some(assignee);
        }

        // 2. Capability matching: find connected agents whose skill_tags
        //    satisfy all required_tags from task metadata.
        if let Some(metadata) = context.metadata
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
    use crate::a2a::core::task_types::ContextId;

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

    /// Build a route context with no metadata for the given pinned assignee.
    fn ctx(assignee: Option<AgentId>) -> TaskRouteContext<'static> {
        TaskRouteContext {
            assignee,
            context_id: ContextId::new(),
            metadata: None,
        }
    }

    /// Build a route context whose metadata borrows the supplied value.
    fn ctx_with_metadata(metadata: &serde_json::Value) -> TaskRouteContext<'_> {
        TaskRouteContext {
            assignee: None,
            context_id: ContextId::new(),
            metadata: Some(metadata),
        }
    }

    #[test]
    fn explicit_assignment_returns_assignee() {
        let id = AgentId::new();
        let agent = make_agent(id, AgentStatus::Connected);
        let context = ctx(Some(id));
        let router = DefaultTaskRouter;

        let selected = router.select_agent(&context, &[&agent]);

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
        let context = ctx(Some(id));
        let router = DefaultTaskRouter;

        let selected = router.select_agent(&context, &[&agent]);

        assert_eq!(
            selected, None,
            "should return None when assignee is Disconnected and no other agents available"
        );
    }

    #[test]
    fn falls_back_to_connected_agent() {
        let id = AgentId::new();
        let connected = make_agent(id, AgentStatus::Connected);
        let context = ctx(None);
        let router = DefaultTaskRouter;

        let selected = router.select_agent(&context, &[&connected]);

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

    fn tags_metadata(tags: Vec<&str>) -> serde_json::Value {
        serde_json::json!({ "required_tags": tags })
    }

    #[test]
    fn capability_matching_selects_tagged_agent() {
        let capable_id = AgentId::new();
        let plain_id = AgentId::new();
        let capable = make_agent_with_tags(capable_id, AgentStatus::Connected, vec!["code.review"]);
        let plain = make_agent(plain_id, AgentStatus::Connected);
        let metadata = tags_metadata(vec!["code.review"]);
        let context = ctx_with_metadata(&metadata);
        let router = DefaultTaskRouter;

        let selected = router.select_agent(&context, &[&plain, &capable]);

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
        let metadata = tags_metadata(vec!["code.review"]);
        let context = ctx_with_metadata(&metadata);
        let router = DefaultTaskRouter;

        let selected = router.select_agent(&context, &[&capable, &fallback]);

        assert_eq!(
            selected, None,
            "should return None when only capable agent is disconnected"
        );
    }

    #[test]
    fn no_capability_match_returns_none() {
        let id = AgentId::new();
        let agent = make_agent_with_tags(id, AgentStatus::Connected, vec!["code.fix"]);
        let metadata = tags_metadata(vec!["code.review"]);
        let context = ctx_with_metadata(&metadata);
        let router = DefaultTaskRouter;

        let selected = router.select_agent(&context, &[&agent]);

        assert_eq!(
            selected, None,
            "should return None when required_tags are unmatched"
        );
    }

    #[test]
    fn empty_required_tags_skips_matching() {
        let id = AgentId::new();
        let agent = make_agent(id, AgentStatus::Connected);
        let metadata = tags_metadata(vec![]);
        let context = ctx_with_metadata(&metadata);
        let router = DefaultTaskRouter;

        let selected = router.select_agent(&context, &[&agent]);

        assert_eq!(
            selected,
            Some(id),
            "empty required_tags should skip capability matching"
        );
    }
}
