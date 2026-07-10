//! Instructive deserialization wrapper for MCP tool parameters.
//!
//! # The rmcp message-passthrough trick
//!
//! `rmcp` deserializes a tool's `Parameters<T>` from the JSON-RPC `arguments`
//! object *before* the tool body runs. When that deserialization fails, rmcp
//! surfaces the serde error's `Display` text verbatim as the `-32602`
//! ("Invalid params") error message handed back to the agent. The stock serde
//! message for a missing required field is the famously terse
//! `` missing field `pattern` `` — which gives the agent no hint about what the
//! tool actually expects or what it likely meant to type.
//!
//! [`Lenient<T>`] exploits that passthrough. It is a transparent newtype whose
//! [`serde::Deserialize`] impl first parses the raw [`serde_json::Value`], then
//! attempts `from_value::<T>`. On failure it rebuilds a *helpful* message —
//! naming the tool's expected fields and (when a provided key is a near-miss for
//! an expected one) a `` did you mean `field`? `` suggestion — and emits it
//! through [`serde::de::Error::custom`]. Because rmcp passes that text straight
//! into the `-32602` message, the agent receives the instructive version.
//!
//! Crucially, the [`schemars::JsonSchema`] impl delegates *entirely* to `T`, so
//! the published input schema an agent sees is byte-for-byte identical to the
//! unwrapped type. The wrapper is invisible at the schema layer and only changes
//! the *error* surface.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

/// Transparent wrapper that turns a failed parameter deserialization into an
/// instructive `-32602` message. See the module docs for the mechanism.
///
/// `Lenient<T>` derefs to nothing special — callers destructure it
/// (`Parameters(Lenient(params)): Parameters<Lenient<T>>`) to recover the inner
/// `T`, then use `params` exactly as before.
pub(crate) struct Lenient<T>(pub T);

/// Maximum Levenshtein distance at which a provided key is treated as a probable
/// misspelling of an expected field (so we emit a `did you mean` suggestion).
/// Two edits covers single-character typos, transposition-like slips, and a
/// dropped/added character without matching genuinely unrelated keys.
const NEAR_MISS_MAX_DISTANCE: usize = 2;

impl<T: JsonSchema> JsonSchema for Lenient<T> {
    fn inline_schema() -> bool {
        T::inline_schema()
    }

    fn schema_name() -> Cow<'static, str> {
        T::schema_name()
    }

    fn schema_id() -> Cow<'static, str> {
        T::schema_id()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        T::json_schema(generator)
    }

    fn _schemars_private_non_optional_json_schema(generator: &mut SchemaGenerator) -> Schema {
        T::_schemars_private_non_optional_json_schema(generator)
    }

    fn _schemars_private_is_option() -> bool {
        T::_schemars_private_is_option()
    }
}

impl<'de, T> Deserialize<'de> for Lenient<T>
where
    T: DeserializeOwned + JsonSchema,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error as _;

        let value = serde_json::Value::deserialize(deserializer)?;
        match serde_json::from_value::<T>(value.clone()) {
            Ok(inner) => Ok(Lenient(inner)),
            Err(error) => Err(D::Error::custom(build_message::<T>(&error, &value))),
        }
    }
}

/// Build the instructive error message: the underlying serde error, the tool's
/// expected field names (derived at runtime from the type's JSON Schema), and an
/// optional `did you mean` suggestion when a provided key is a near-miss.
fn build_message<T: JsonSchema>(error: &serde_json::Error, provided: &serde_json::Value) -> String {
    let (expected, required) = expected_fields::<T>();

    let mut message = format!("{error}");

    if !expected.is_empty() {
        message.push_str(". expected fields: ");
        message.push_str(&join_fields(&expected, &required));
    }

    if let Some(suggestion) = did_you_mean(provided, &expected) {
        message.push_str(&format!(". did you mean `{suggestion}`?"));
    }

    message
}

/// Extract the `(all_property_names, required_property_names)` from `T`'s JSON
/// Schema. Walks the object schema's `properties` map for every field name and
/// the `required` array for the subset that must be present.
fn expected_fields<T: JsonSchema>() -> (Vec<String>, Vec<String>) {
    let schema = schemars::schema_for!(T);
    let Some(object) = schema.as_object() else {
        return (Vec::new(), Vec::new());
    };

    let properties = object
        .get("properties")
        .and_then(|v| v.as_object())
        .map(|map| map.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();

    let required = object
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    (properties, required)
}

/// Render the expected-field list, tagging each required field so the agent can
/// see at a glance which are mandatory: ``pattern (required), language, limit``.
fn join_fields(expected: &[String], required: &[String]) -> String {
    expected
        .iter()
        .map(|field| {
            if required.iter().any(|r| r == field) {
                format!("`{field}` (required)")
            } else {
                format!("`{field}`")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// If the provided JSON object has a key that is *not* an expected field but is a
/// near-miss (Levenshtein distance `<= NEAR_MISS_MAX_DISTANCE`) for one, return
/// the closest expected field name. Picks the smallest distance; ties resolve to
/// the first expected field encountered.
fn did_you_mean(provided: &serde_json::Value, expected: &[String]) -> Option<String> {
    let provided_keys = provided.as_object()?;

    let mut best: Option<(usize, &String)> = None;
    for key in provided_keys.keys() {
        if expected.iter().any(|field| field == key) {
            continue;
        }
        for field in expected {
            let distance = levenshtein(key, field);
            if distance <= NEAR_MISS_MAX_DISTANCE && best.is_none_or(|(best_dist, _)| distance < best_dist) {
                best = Some((distance, field));
            }
        }
    }

    best.map(|(_, field)| field.clone())
}

/// Classic dynamic-programming Levenshtein edit distance over Unicode scalar
/// values. Inlined to avoid pulling in a new crate dependency for ~15 lines of
/// logic; the inputs here are short field names so the O(n·m) cost is trivial.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut diagonal = previous[0];
        previous[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution_cost = usize::from(ca != cb);
            let next = (previous[j + 1] + 1)
                .min(previous[j] + 1)
                .min(diagonal + substitution_cost);
            diagonal = previous[j + 1];
            previous[j + 1] = next;
        }
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::types::WorkspaceGrepParams;

    #[test]
    fn levenshtein_matches_known_distances() {
        assert_eq!(levenshtein("pattern", "pattern"), 0);
        assert_eq!(levenshtein("patern", "pattern"), 1);
        assert_eq!(levenshtein("pattrn", "pattern"), 1);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    #[test]
    fn valid_params_deserialize_to_inner() {
        let value = serde_json::json!({ "pattern": "x" });
        let lenient: Lenient<WorkspaceGrepParams> =
            serde_json::from_value(value).expect("valid params should deserialize");
        assert_eq!(lenient.0.pattern, "x");
        assert!(lenient.0.include_context);
        assert_eq!(lenient.0.limit, None);
    }

    #[test]
    fn missing_required_field_names_expected_and_suggests() {
        let value = serde_json::json!({ "patern": "x" });
        let message = match serde_json::from_value::<Lenient<WorkspaceGrepParams>>(value) {
            Ok(_) => panic!("missing `pattern` must fail"),
            Err(error) => error.to_string(),
        };
        assert!(
            message.contains("pattern"),
            "message should name the expected `pattern` field, got: {message}"
        );
        assert!(
            message.contains("did you mean `pattern`?"),
            "message should suggest the near-miss field, got: {message}"
        );
    }

    #[test]
    fn aliased_param_name_deserializes_through_lenient() {
        let value = serde_json::json!({ "query": "needle" });
        let lenient: Lenient<WorkspaceGrepParams> =
            serde_json::from_value(value).expect("aliased `query` should deserialize");
        assert_eq!(lenient.0.pattern, "needle");
    }

    #[test]
    fn missing_required_field_without_near_miss_still_lists_fields() {
        let value = serde_json::json!({ "completely_unrelated_key": "x" });
        let message = match serde_json::from_value::<Lenient<WorkspaceGrepParams>>(value) {
            Ok(_) => panic!("missing `pattern` must fail"),
            Err(error) => error.to_string(),
        };
        assert!(
            message.contains("`pattern` (required)"),
            "message should list `pattern` as required, got: {message}"
        );
    }
}
