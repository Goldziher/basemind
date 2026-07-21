//! Per-task model roles.
//!
//! The user can pin a different model to each task (`default`, `small`, `plan`, `title`,
//! `summarize`, `embed`) — the "define default models for different stuff" requirement. Each role
//! is a full basemind [`LlmConfig`] (reusing its `ApiKey`/`SecretString` credential plumbing), and
//! any unset role falls back to `default`.

use basemind::config::LlmConfig;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A task role that can be pinned to its own model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The main coding loop.
    Default,
    /// A cheap/fast model (titles, quick classification).
    Small,
    /// Planning / architecture.
    Plan,
    /// Conversation titling.
    Title,
    /// Compaction / summarization.
    Summarize,
    /// Embeddings.
    Embed,
}

/// Per-role model definitions. `default` is required; the rest fall back to it when unset.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct RoleModels {
    /// The main coding-loop model (required).
    #[serde(default)]
    pub default: LlmConfig,
    /// A cheaper/faster model.
    #[serde(default)]
    pub small: Option<LlmConfig>,
    /// A planning model.
    #[serde(default)]
    pub plan: Option<LlmConfig>,
    /// A titling model.
    #[serde(default)]
    pub title: Option<LlmConfig>,
    /// A summarization/compaction model.
    #[serde(default)]
    pub summarize: Option<LlmConfig>,
    /// An embedding model.
    #[serde(default)]
    pub embed: Option<LlmConfig>,
}

impl RoleModels {
    /// Resolve a role to its [`LlmConfig`], falling back to `default` when the role is unset.
    pub fn resolve(&self, role: Role) -> &LlmConfig {
        let specific = match role {
            Role::Default => return &self.default,
            Role::Small => &self.small,
            Role::Plan => &self.plan,
            Role::Title => &self.title,
            Role::Summarize => &self.summarize,
            Role::Embed => &self.embed,
        };
        specific.as_ref().unwrap_or(&self.default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use basemind::config::ApiKey;

    fn model(name: &str) -> LlmConfig {
        LlmConfig {
            model: name.into(),
            api_key: ApiKey::Literal("sk-test".into()),
            ..Default::default()
        }
    }

    #[test]
    fn unset_roles_fall_back_to_default() {
        let roles = RoleModels {
            default: model("anthropic/claude-sonnet-4"),
            ..Default::default()
        };
        assert_eq!(roles.resolve(Role::Default).model, "anthropic/claude-sonnet-4");
        assert_eq!(roles.resolve(Role::Small).model, "anthropic/claude-sonnet-4");
        assert_eq!(roles.resolve(Role::Summarize).model, "anthropic/claude-sonnet-4");
    }

    #[test]
    fn a_configured_role_overrides_default() {
        let roles = RoleModels {
            default: model("anthropic/claude-sonnet-4"),
            small: Some(model("anthropic/claude-haiku-4")),
            ..Default::default()
        };
        assert_eq!(roles.resolve(Role::Small).model, "anthropic/claude-haiku-4");
        assert_eq!(roles.resolve(Role::Plan).model, "anthropic/claude-sonnet-4");
    }
}
