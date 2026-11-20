use std::sync::OnceLock;

use jsonschema::error::ValidationErrorKind;
use jsonschema::paths::{Location, LocationSegment};
use jsonschema::{ValidationError, Validator};
use serde_json::Value;

use super::{CodeSearchConfig, ConfigError};

const SCHEMA_V1: &str = include_str!("../../schema/basemind-config-v1.schema.json");

/// Largest edit distance at which an unknown key is still offered a "did you mean" suggestion.
const MAX_SUGGESTION_DISTANCE: usize = 2;

static SCHEMA_JSON_V1: OnceLock<Value> = OnceLock::new();
static VALIDATOR_V1: OnceLock<Validator> = OnceLock::new();

fn schema_v1() -> &'static Value {
    SCHEMA_JSON_V1.get_or_init(|| serde_json::from_str(SCHEMA_V1).expect("bundled schema must be valid JSON"))
}

fn validator_v1() -> &'static Validator {
    VALIDATOR_V1.get_or_init(|| {
        jsonschema::draft202012::new(schema_v1()).expect("bundled schema must be a valid Draft 2020-12 schema")
    })
}

pub fn validate_v1(value: &Value) -> Result<(), ConfigError> {
    let validator = validator_v1();
    let errors: Vec<String> = validator.iter_errors(value).flat_map(|e| describe_error(&e)).collect();
    if !errors.is_empty() {
        return Err(ConfigError::SchemaValidation(errors.join("\n")));
    }
    validate_code_search(value)
}

fn describe_error(error: &ValidationError<'_>) -> Vec<String> {
    let ValidationErrorKind::AdditionalProperties { unexpected } = error.kind() else {
        return vec![format!("  at {}: {}", error.instance_path(), error)];
    };
    let table = table_name(error.instance_path());
    let known = known_keys(error.schema_path());
    unexpected
        .iter()
        .map(|key| format!("  {}", unknown_key_message(key, &table, &known)))
        .collect()
}

/// Render an instance JSON pointer as the TOML table a user wrote (`/documents/reranker` →
/// `[documents.reranker]`); the document root is the top-level table.
fn table_name(instance_path: &Location) -> String {
    let mut name = String::new();
    for segment in instance_path.segments() {
        match segment {
            LocationSegment::Property(key) => {
                if !name.is_empty() {
                    name.push('.');
                }
                name.push_str(&key);
            }
            LocationSegment::Index(index) => name.push_str(&format!("[{index}]")),
        }
    }
    if name.is_empty() {
        "the top-level table".to_string()
    } else {
        format!("[{name}]")
    }
}

/// The keys the schema object that raised an `additionalProperties` failure declares, in schema
/// order. `schema_path` is the definition-site keyword location (`/$defs/ScanConfig/additionalProperties`),
/// so its parent resolves directly in the bundled schema, which is derived from the config structs.
fn known_keys(schema_path: &Location) -> Vec<&'static str> {
    schema_path
        .as_str()
        .strip_suffix("/additionalProperties")
        .and_then(|parent| schema_v1().pointer(parent))
        .and_then(|node| node.get("properties"))
        .and_then(Value::as_object)
        .map(|properties| properties.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

fn unknown_key_message(key: &str, table: &str, known: &[&str]) -> String {
    let closest = known
        .iter()
        .map(|candidate| (strsim::levenshtein(key, candidate), *candidate))
        .filter(|(distance, _)| *distance <= MAX_SUGGESTION_DISTANCE)
        .min_by_key(|(distance, _)| *distance);
    match closest {
        Some((_, suggestion)) => format!("unknown key `{key}` in {table}; did you mean `{suggestion}`?"),
        None if known.is_empty() => format!("unknown key `{key}` in {table}"),
        None => {
            let allowed: Vec<String> = known.iter().map(|k| format!("`{k}`")).collect();
            format!("unknown key `{key}` in {table}; allowed keys: {}", allowed.join(", "))
        }
    }
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

    fn schema_error(value: &Value) -> String {
        match validate_v1(value) {
            Err(ConfigError::SchemaValidation(msg)) => msg,
            other => panic!("expected a schema validation error, got {other:?}"),
        }
    }

    #[test]
    fn should_suggest_closest_key_for_scan_typo() {
        let value = json!({ "$schema": "v1", "scan": { "folow_symlinks": true } });
        assert_eq!(
            schema_error(&value),
            "  unknown key `folow_symlinks` in [scan]; did you mean `follow_symlinks`?"
        );
    }

    #[test]
    fn should_suggest_closest_key_for_nested_table_typo() {
        let value = json!({ "$schema": "v1", "documents": { "reranker": { "enabeld": true } } });
        assert_eq!(
            schema_error(&value),
            "  unknown key `enabeld` in [documents.reranker]; did you mean `enabled`?"
        );
    }

    #[test]
    fn should_suggest_closest_key_for_top_level_typo() {
        let value = json!({ "$schema": "v1", "scna": {} });
        assert_eq!(
            schema_error(&value),
            "  unknown key `scna` in the top-level table; did you mean `scan`?"
        );
    }

    #[test]
    fn should_list_allowed_keys_when_no_key_is_close() {
        let value = json!({ "$schema": "v1", "documents": { "reranker": { "zzz": 1 } } });
        assert_eq!(
            schema_error(&value),
            "  unknown key `zzz` in [documents.reranker]; allowed keys: `enabled`, `preset`, `top_k`"
        );
    }

    #[test]
    fn should_report_each_unknown_key_in_a_table() {
        let value = json!({ "$schema": "v1", "documents": { "reranker": { "top_kk": 5, "zzz": 1 } } });
        assert_eq!(
            schema_error(&value),
            "  unknown key `top_kk` in [documents.reranker]; did you mean `top_k`?\n  \
             unknown key `zzz` in [documents.reranker]; allowed keys: `enabled`, `preset`, `top_k`"
        );
    }

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
