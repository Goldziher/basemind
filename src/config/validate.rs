use std::sync::OnceLock;

use jsonschema::Validator;
use serde_json::Value;

use super::{CodeSearchConfig, ConfigError};

const SCHEMA_V1: &str = include_str!("../../schema/basemind-config-v1.schema.json");

static VALIDATOR_V1: OnceLock<Validator> = OnceLock::new();

fn validator_v1() -> &'static Validator {
    VALIDATOR_V1.get_or_init(|| {
        let schema_json: Value = serde_json::from_str(SCHEMA_V1).expect("bundled schema must be valid JSON");
        jsonschema::draft202012::new(&schema_json).expect("bundled schema must be a valid Draft 2020-12 schema")
    })
}

pub fn validate_v1(value: &Value) -> Result<(), ConfigError> {
    let validator = validator_v1();
    let errors: Vec<String> = validator
        .iter_errors(value)
        .map(|e| format!("  at {}: {}", e.instance_path(), e))
        .collect();
    if !errors.is_empty() {
        return Err(ConfigError::SchemaValidation(errors.join("\n")));
    }
    validate_code_search(value)
}

/// Enforce `[code_search]` cross-field invariants (`overlap < max_characters`). The JSON schema
/// only bounds each field independently, so this catches the `overlap >= max_characters` case
/// that degenerates the chunker step. An absent table is valid — the defaults satisfy it.
fn validate_code_search(value: &Value) -> Result<(), ConfigError> {
    let Some(table) = value.get("code_search") else {
        return Ok(());
    };
    let config: CodeSearchConfig = serde_json::from_value(table.clone()).map_err(ConfigError::Deserialize)?;
    config.validate().map_err(ConfigError::SchemaValidation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn should_reject_overlap_not_less_than_max_characters() {
        let value = json!({
            "$schema": "v1",
            "code_search": { "max_characters": 64, "overlap": 200 }
        });
        let err = validate_v1(&value).expect_err("overlap >= max_characters must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("overlap"), "error names overlap: {msg}");
        assert!(msg.contains("max_characters"), "error names max_characters: {msg}");
    }

    #[test]
    fn should_accept_overlap_less_than_max_characters() {
        let value = json!({
            "$schema": "v1",
            "code_search": { "max_characters": 1500, "overlap": 200 }
        });
        validate_v1(&value).expect("a normal code_search config passes");
    }

    #[test]
    fn should_accept_absent_code_search_table() {
        let value = json!({ "$schema": "v1" });
        validate_v1(&value).expect("an absent code_search table is valid (defaults apply)");
    }
}
