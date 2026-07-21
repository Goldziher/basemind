//! The provider pool: one ready client per configured role.
//!
//! Each [`Role`] resolves to a [`ResolvedRole`] — a [`ModelClient`] plus the per-request knobs
//! (model string, temperature, max-tokens) the turn-loop needs. Unconfigured roles fall back to
//! `default`'s resolved client, so `for_role` always returns something usable.

mod build;

pub use build::build_model_client;

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{AgentConfig, Role, RoleModels};
use crate::error::{AgentError, Result};
use crate::model::ModelClient;

/// A role resolved to a ready client plus the per-request knobs from its [`LlmConfig`].
#[derive(Clone)]
pub struct ResolvedRole {
    /// The client to send requests through.
    pub client: Arc<dyn ModelClient>,
    /// The `provider/model` routing string for the request.
    pub model: String,
    /// Sampling temperature, if configured.
    pub temperature: Option<f64>,
    /// Max tokens to generate, if configured.
    pub max_tokens: Option<u64>,
}

/// Holds one resolved client per configured chat role, with `default` always present.
pub struct ProviderPool {
    default: ResolvedRole,
    roles: HashMap<Role, ResolvedRole>,
}

impl ProviderPool {
    /// Build the pool from an [`AgentConfig`]. The `default` role must carry a non-empty model;
    /// other chat roles are built only when explicitly configured (embeddings are not a chat role
    /// and are skipped here).
    pub fn from_config(config: &AgentConfig) -> Result<Self> {
        let default = resolve(&config.roles, Role::Default)?;
        let mut roles = HashMap::new();
        for role in [Role::Small, Role::Plan, Role::Title, Role::Summarize] {
            // Build a distinct client only for an explicitly-configured role; an unset role shares ~keep
            // `default` via `for_role`'s fallback. (An unset role resolves to `&default`, so no ~keep
            // model-diffing is needed here.) ~keep
            if is_explicit(&config.roles, role) {
                roles.insert(role, resolve(&config.roles, role)?);
            }
        }
        Ok(Self { default, roles })
    }

    /// Build a pool from a single already-resolved role; every role falls back to it. Available
    /// only under test / the `test-util` feature: it lets a controlled smoke drive the real runner
    /// with a scripted [`ModelClient`](crate::model::ModelClient) instead of resolving live provider
    /// clients from config.
    #[cfg(any(test, feature = "test-util"))]
    pub fn single(default: ResolvedRole) -> Self {
        Self {
            default,
            roles: HashMap::new(),
        }
    }

    /// The resolved role, falling back to `default` when the role has no distinct client.
    pub fn for_role(&self, role: Role) -> &ResolvedRole {
        if role == Role::Default {
            return &self.default;
        }
        self.roles.get(&role).unwrap_or(&self.default)
    }
}

fn is_explicit(roles: &RoleModels, role: Role) -> bool {
    match role {
        Role::Default => true,
        Role::Small => roles.small.is_some(),
        Role::Plan => roles.plan.is_some(),
        Role::Title => roles.title.is_some(),
        Role::Summarize => roles.summarize.is_some(),
        Role::Embed => roles.embed.is_some(),
    }
}

fn resolve(roles: &RoleModels, role: Role) -> Result<ResolvedRole> {
    let llm = roles.resolve(role);
    let client = build_model_client(llm).map_err(|e| AgentError::Config(format!("role {role:?}: {e}")))?;
    Ok(ResolvedRole {
        client,
        model: llm.model.clone(),
        temperature: llm.temperature,
        max_tokens: llm.max_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RoleModels;
    use basemind::config::{ApiKey, LlmConfig};

    fn model(name: &str) -> LlmConfig {
        LlmConfig {
            model: name.into(),
            api_key: ApiKey::Literal("sk-test".into()),
            ..Default::default()
        }
    }

    #[test]
    fn builds_default_and_falls_back_for_unconfigured_roles() {
        let cfg = AgentConfig {
            roles: RoleModels {
                default: model("anthropic/claude-sonnet-4"),
                small: Some(model("anthropic/claude-haiku-4")),
                ..Default::default()
            },
            ..Default::default()
        };
        let pool = ProviderPool::from_config(&cfg).expect("pool builds");
        assert_eq!(pool.for_role(Role::Default).model, "anthropic/claude-sonnet-4");
        assert_eq!(pool.for_role(Role::Small).model, "anthropic/claude-haiku-4");
        // Unconfigured plan role falls back to default's model. ~keep
        assert_eq!(pool.for_role(Role::Plan).model, "anthropic/claude-sonnet-4");
    }

    #[test]
    fn from_config_errors_when_default_has_no_model() {
        let cfg = AgentConfig::default(); // default LlmConfig has an empty model ~keep
        assert!(ProviderPool::from_config(&cfg).is_err());
    }
}
