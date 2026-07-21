//! Resolve the agent configuration for a session.
//!
//! Precedence, highest first: the `[agent]` table in the repo's `basemind.toml` (full per-role
//! model config — the "define default models for different tasks" requirement); otherwise a
//! single default role built from the environment. An explicit `BASEMIND_AGENT_MODEL` always
//! overrides the default role's model, so a config file can pin the specialised roles while the
//! env var swaps the main model per invocation.

use std::path::Path;

use anyhow::{Context, Result};
use basemind::config::{ApiKey, LlmConfig};
use basemind_agent::config::AgentConfig;

/// Default model when neither the config file nor the environment names one.
pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-4";
/// Environment variable naming the main-role model (overrides the file's default role).
pub const MODEL_ENV: &str = "BASEMIND_AGENT_MODEL";
/// Environment variable holding the provider API key used for the fallback role.
pub const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// Load the agent config for `root`. Reads the `[agent]` table from `basemind.toml` when present,
/// otherwise synthesises a single default role from the environment. A non-empty
/// `BASEMIND_AGENT_MODEL` overrides the resolved default-role model in either case.
pub fn load_agent_config(root: &Path) -> Result<AgentConfig> {
    let model_override = std::env::var(MODEL_ENV).ok().filter(|value| !value.trim().is_empty());
    let mut config = match read_agent_table(root)? {
        Some(config) => config,
        None => AgentConfig {
            roles: default_roles(model_override.clone()),
            ..Default::default()
        },
    };
    if let Some(model) = model_override {
        config.roles.default.model = model;
    }
    if config.roles.default.model.trim().is_empty() {
        config.roles.default.model = DEFAULT_MODEL.to_string();
    }
    Ok(config)
}

/// The model name of the resolved default role — shown in the status bar.
pub fn default_model_name(config: &AgentConfig) -> &str {
    &config.roles.default.model
}

/// Build the single-role fallback config from the environment.
fn default_roles(model_override: Option<String>) -> basemind_agent::config::RoleModels {
    basemind_agent::config::RoleModels {
        default: LlmConfig {
            model: model_override.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            api_key: ApiKey::Env {
                env: API_KEY_ENV.to_string(),
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Parse the `[agent]` table out of the repo's `basemind.toml`, if the file exists and has one.
fn read_agent_table(root: &Path) -> Result<Option<AgentConfig>> {
    let path = basemind::config::resolve_config_path(root);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read config {}", path.display())),
    };
    let document: toml::Value = toml::from_str(&raw).with_context(|| format!("parse config {}", path.display()))?;
    let Some(agent) = document.get("agent") else {
        return Ok(None);
    };
    let config = agent
        .clone()
        .try_into()
        .context("deserialize the [agent] table (check role model / api_key fields)")?;
    Ok(Some(config))
}
