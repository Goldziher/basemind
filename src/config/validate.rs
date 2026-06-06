use std::sync::OnceLock;

use jsonschema::Validator;
use serde_json::Value;

use super::ConfigError;

const SCHEMA_V1: &str = include_str!("../../schema/gitmind-config-v1.schema.json");

static VALIDATOR_V1: OnceLock<Validator> = OnceLock::new();

fn validator_v1() -> &'static Validator {
    VALIDATOR_V1.get_or_init(|| {
        let schema_json: Value =
            serde_json::from_str(SCHEMA_V1).expect("bundled schema must be valid JSON");
        jsonschema::draft202012::new(&schema_json)
            .expect("bundled schema must be a valid Draft 2020-12 schema")
    })
}

pub fn validate_v1(value: &Value) -> Result<(), ConfigError> {
    let validator = validator_v1();
    let errors: Vec<String> = validator
        .iter_errors(value)
        .map(|e| format!("  at {}: {}", e.instance_path, e))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::SchemaValidation(errors.join("\n")))
    }
}
