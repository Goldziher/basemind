//! Agent configuration.
//!
//! A new `[agent]` section (distinct from basemind's documents-scoped `[llm]`) holds the per-role
//! models plus loop budgets. It reuses basemind's `LlmConfig`/`ApiKey`/`SecretString` types for
//! credentials so BYO-key handling (env refs, redaction) is shared, not reinvented.

mod role;

pub use role::{Role, RoleModels};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default number of model steps a single turn may take before stopping.
pub const DEFAULT_MAX_STEPS: u32 = 40;

fn default_max_steps() -> u32 {
    DEFAULT_MAX_STEPS
}

/// The `[agent]` configuration section.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentConfig {
    /// Per-role model definitions.
    #[serde(default)]
    pub roles: RoleModels,
    /// Maximum model steps per turn (tool-call rounds) before the turn stops.
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    /// Optional per-session cost ceiling in USD; the turn stops once exceeded.
    #[serde(default)]
    pub cost_budget_usd: Option<f64>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            roles: RoleModels::default(),
            max_steps: DEFAULT_MAX_STEPS,
            cost_budget_usd: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_steps_defaults_to_the_constant_on_deserialize() {
        // An empty `[agent]` table should still get the sane step budget, not zero. ~keep
        let cfg: AgentConfig = toml_from("");
        assert_eq!(cfg.max_steps, DEFAULT_MAX_STEPS);
        assert!(cfg.cost_budget_usd.is_none());
    }

    #[test]
    fn explicit_values_override_defaults() {
        let cfg: AgentConfig = toml_from("max_steps = 10\ncost_budget_usd = 2.5\n");
        assert_eq!(cfg.max_steps, 10);
        assert_eq!(cfg.cost_budget_usd, Some(2.5));
    }

    fn toml_from(body: &str) -> AgentConfig {
        // Deserialize via JSON to avoid pulling a toml dep into this crate's tests; the field ~keep
        // defaults (serde `default`) behave identically across formats. ~keep
        let json = json_for(body);
        serde_json::from_value(json).expect("valid config")
    }

    fn json_for(body: &str) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            let (key, value) = line.split_once('=').expect("k = v");
            let key = key.trim().to_string();
            let value = value.trim();
            let parsed: serde_json::Value = serde_json::from_str(value).expect("scalar");
            map.insert(key, parsed);
        }
        serde_json::Value::Object(map)
    }
}
