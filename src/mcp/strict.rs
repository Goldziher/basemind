//! Exact argument validation for the verb tools (ADR-0018).
//!
//! `rmcp` deserializes a tool's `Parameters<T>` before the tool body runs and turns a failure into
//! a JSON-RPC `-32602`. Two things are wrong with that for an agent: a protocol error is not a tool
//! result it can read and correct in the same turn, and serde's default is to DROP a key it does
//! not know, so a call carrying an unsupported `glob` ran unscoped and reported success.
//!
//! [`Strict<T>`] fixes both. Its `Deserialize` never fails: it captures either the parsed `T` or an
//! instructive message, and the shim returns that message as an `isError` tool result. `T` must be
//! `#[serde(deny_unknown_fields)]`, so an unknown key is an error, and the message names the
//! closest accepted field and lists every accepted one.
//!
//! The [`schemars::JsonSchema`] impl delegates to `T`, so the wrapper is invisible in `tools/list`.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

/// The arguments of one verb tool: the wire name the error messages cite.
pub(crate) trait ToolArgs: DeserializeOwned + JsonSchema {
    /// The MCP tool name, e.g. `code_grep`.
    const TOOL: &'static str;
}

/// Arguments that either parsed exactly or carry the message explaining why not.
pub(crate) struct Strict<T>(pub Result<T, String>);

/// Maximum edit distance at which an unknown key is treated as a misspelling of an accepted field.
const NEAR_MISS_MAX_DISTANCE: usize = 2;

/// Names agents reach for that mean the same thing. An unknown key is matched to the accepted
/// field sharing its group, which catches the renames edit distance cannot (`glob` for `path`).
const SYNONYMS: &[&[&str]] = &[
    &[
        "path",
        "glob",
        "file",
        "filename",
        "path_contains",
        "path_prefix",
        "include",
    ],
    &[
        "name",
        "symbol",
        "needle",
        "identifier",
        "trait_name",
        "module",
        "callee",
    ],
    &["query", "pattern", "q", "text", "search", "term", "needle", "name"],
    &["pattern", "query", "regex", "q", "text", "search"],
    &["line", "row", "line_start", "start_line", "start_row", "line_num"],
    &["end_line", "line_end", "end_row"],
    &["column", "col", "start_col"],
    &["rev", "sha", "commit", "revision", "ref"],
    &["from", "rev_old", "base", "old"],
    &["to", "rev_new", "head", "new"],
    &["limit", "top_k", "max_results", "count", "n"],
    &["max_tokens", "token_budget", "budget"],
    &["key", "id"],
    &["tags", "tag"],
    &["tag", "tags"],
    &["kind", "type"],
    &["language", "lang"],
    &["keywords", "keywords_contains", "keyword"],
    &["entity", "entity_category"],
    &["context", "include_context"],
    &["structural", "outline", "symbols"],
    &["relation", "direction"],
];

impl<T: JsonSchema> JsonSchema for Strict<T> {
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

impl<'de, T: ToolArgs> Deserialize<'de> for Strict<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(Strict(parse::<T>(value)))
    }
}

/// Parse `value` as `T`, or explain the failure in terms of the tool's accepted fields.
pub(crate) fn parse<T: ToolArgs>(value: serde_json::Value) -> Result<T, String> {
    let keys: Vec<String> = value
        .as_object()
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default();
    serde_json::from_value::<T>(value).map_err(|error| explain::<T>(&error.to_string(), &keys))
}

fn explain<T: ToolArgs>(error: &str, provided: &[String]) -> String {
    let (accepted, required) = accepted_fields::<T>();
    let unknown: Vec<&String> = provided.iter().filter(|key| !accepted.contains(key)).collect();

    let mut message = format!("`{}`: ", T::TOOL);
    if unknown.is_empty() {
        message.push_str(error);
    } else {
        for (index, key) in unknown.iter().enumerate() {
            if index > 0 {
                message.push_str("; ");
            }
            message.push_str(&format!("unknown field `{key}`"));
            if key.as_str() == "mode" {
                message.push_str(" (mode-dispatched tools were replaced: each operation is its own tool)");
            } else if let Some(suggestion) = suggest(key, &accepted) {
                message.push_str(&format!(", did you mean `{suggestion}`?"));
            }
        }
    }
    message.push_str(". Accepted fields: ");
    message.push_str(&render_fields(&accepted, &required));
    message
}

/// `(every property, the required subset)` of `T`'s schema, in declaration order.
fn accepted_fields<T: JsonSchema>() -> (Vec<String>, Vec<String>) {
    let schema = schemars::schema_for!(T);
    let Some(object) = schema.as_object() else {
        return (Vec::new(), Vec::new());
    };
    let names = |key: &str| -> Vec<String> {
        match object.get(key) {
            Some(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        }
    };
    (names("properties"), names("required"))
}

fn render_fields(accepted: &[String], required: &[String]) -> String {
    if accepted.is_empty() {
        return "none".to_string();
    }
    accepted
        .iter()
        .map(|field| {
            if required.contains(field) {
                format!("`{field}` (required)")
            } else {
                format!("`{field}`")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The accepted field an unknown `key` most likely meant: a synonym first, then a near-miss spelling.
fn suggest<'a>(key: &str, accepted: &'a [String]) -> Option<&'a str> {
    for group in SYNONYMS {
        if !group.contains(&key) {
            continue;
        }
        if let Some(field) = accepted.iter().find(|field| group[0] == field.as_str()) {
            return Some(field);
        }
    }
    for group in SYNONYMS {
        if !group.contains(&key) {
            continue;
        }
        if let Some(field) = accepted.iter().find(|field| group.contains(&field.as_str())) {
            return Some(field);
        }
    }
    accepted
        .iter()
        .map(|field| (levenshtein(key, field), field))
        .filter(|(distance, _)| *distance <= NEAR_MISS_MAX_DISTANCE)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, field)| field.as_str())
}

/// Levenshtein edit distance over Unicode scalar values; the inputs are short field names.
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
    use crate::mcp::types_tools::CodeGrepParams;
    use serde_json::json;

    fn error(value: serde_json::Value) -> String {
        match parse::<CodeGrepParams>(value) {
            Ok(_) => panic!("expected a rejection"),
            Err(message) => message,
        }
    }

    #[test]
    fn exact_arguments_parse() {
        let params = parse::<CodeGrepParams>(json!({ "pattern": "x", "path": "src/" })).expect("valid");
        assert_eq!(params.pattern, "x");
        assert_eq!(params.path.as_deref(), Some("src/"));
    }

    #[test]
    fn unknown_field_is_rejected_with_a_synonym_suggestion() {
        let message = error(json!({ "pattern": "x", "glob": "*.rs" }));
        assert!(
            message.contains("`code_grep`: unknown field `glob`, did you mean `path`?"),
            "{message}"
        );
        assert!(message.contains("Accepted fields: `pattern` (required)"), "{message}");
    }

    #[test]
    fn misspelled_field_is_rejected_with_a_near_miss_suggestion() {
        let message = error(json!({ "pattern": "x", "langauge": "rust" }));
        assert!(
            message.contains("unknown field `langauge`, did you mean `language`?"),
            "{message}"
        );
    }

    #[test]
    fn missing_required_field_names_it_and_lists_the_accepted_set() {
        let message = error(json!({ "language": "rust" }));
        assert!(message.contains("missing field `pattern`"), "{message}");
        assert!(message.contains("`pattern` (required)"), "{message}");
    }

    #[test]
    fn a_leftover_mode_key_explains_the_new_surface() {
        let message = error(json!({ "mode": "grep", "pattern": "x" }));
        assert!(message.contains("unknown field `mode`"), "{message}");
        assert!(message.contains("each operation is its own tool"), "{message}");
    }

    #[test]
    fn a_cross_tool_alias_is_not_silently_accepted() {
        let message = error(json!({ "q": "x" }));
        assert!(
            message.contains("unknown field `q`, did you mean `pattern`?"),
            "{message}"
        );
    }

    #[test]
    fn deserialization_never_fails_so_the_shim_can_answer_with_a_tool_result() {
        let strict: Strict<CodeGrepParams> = serde_json::from_value(json!({ "glob": 1 })).expect("never a serde error");
        assert!(strict.0.is_err());
    }

    #[test]
    fn levenshtein_matches_known_distances() {
        assert_eq!(levenshtein("pattern", "pattern"), 0);
        assert_eq!(levenshtein("patern", "pattern"), 1);
        assert_eq!(levenshtein("", "abc"), 3);
    }
}
