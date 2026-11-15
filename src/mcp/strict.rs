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
//! The arguments are checked against `T`'s own schema before serde sees them, for two reasons.
//! Models routinely send a scalar as a string (`"limit":"10"`, `"regex":"true"`), which is
//! unambiguous for an integer or boolean field and is coerced. And serde's type error names the
//! value, not the field (`invalid type: string "many", expected u32`), so the check here reports
//! every unknown key and every mistyped field together, each by name.
//!
//! The [`schemars::JsonSchema`] impl delegates to `T`, so the wrapper is invisible in `tools/list`.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

/// The arguments of one verb tool: the wire name the error messages cite.
pub(crate) trait ToolArgs: DeserializeOwned + JsonSchema + 'static {
    /// The MCP tool name, e.g. `code_grep`.
    const TOOL: &'static str;
}

/// Arguments that either parsed exactly or carry the message explaining why not.
pub(crate) struct Strict<T>(pub Result<T, String>);

/// Maximum edit distance at which an unknown key is treated as a misspelling of an accepted field.
const NEAR_MISS_MAX_DISTANCE: usize = 2;

/// Shortest unknown key the edit-distance suggestion applies to. Below it almost any field is
/// within two edits (`to` -> `tag`, `id` -> `kind`), so the hint would be noise.
const NEAR_MISS_MIN_KEY_LEN: usize = 4;

/// Names agents reach for that mean the same thing. An unknown key is matched to the accepted
/// field sharing its group, which catches the renames edit distance cannot (`glob` for `path`).
///
/// A group holds only names whose VALUES are interchangeable: a hint to move a value into a field
/// that takes a different vocabulary (a 0-based `row` into the 1-based `line`, a `direction` into
/// `relation`, a list of `tags` into the single `tag`) produces a second wrong call.
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
    &["query", "pattern", "q", "text", "search", "term", "needle"],
    &["pattern", "query", "q", "text", "search"],
    &["line", "line_start", "start_line", "line_num"],
    &["end_line", "line_end"],
    &["column", "col"],
    &["rev", "sha", "commit", "revision", "ref"],
    &["from", "rev_old", "base", "old"],
    &["to", "rev_new", "head", "new"],
    &["limit", "top_k", "max_results", "count", "n"],
    &["max_tokens", "token_budget", "budget"],
    &["key", "id"],
    &["kind", "type"],
    &["language", "lang"],
    &["keywords", "keywords_contains", "keyword"],
    &["entity", "entity_category"],
    &["context", "include_context"],
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
///
/// A call with no `arguments` at all is the empty object. Stringified integers and booleans are
/// coerced; every unknown key, mistyped field and missing required field is reported in one message.
pub(crate) fn parse<T: ToolArgs>(value: serde_json::Value) -> Result<T, String> {
    use serde_json::Value;

    let fields = Fields::cached::<T>();
    let mut arguments = match value {
        Value::Object(arguments) => arguments,
        Value::Null => serde_json::Map::new(),
        other => {
            return Err(fields.message::<T>(&[format!("arguments must be a JSON object (got {})", shown(&other))]));
        }
    };

    let mut problems: Vec<String> = Vec::new();
    for (key, value) in arguments.iter_mut() {
        match fields.kinds.iter().find(|(name, _)| name == key) {
            Some((_, kind)) => {
                let required = fields.required.contains(key);
                if let Some(problem) = kind.check(key, value, required) {
                    problems.push(problem);
                }
            }
            None => problems.push(unknown_field(key, &fields.names())),
        }
    }
    for field in &fields.required {
        if !arguments.contains_key(field) {
            problems.push(format!("missing field `{field}`"));
        }
    }
    if !problems.is_empty() {
        return Err(fields.message::<T>(&problems));
    }
    // A string field may hold an object only if it is a path (`{"bytes": […]}`), which the schema
    // cannot tell from any other string. Kept aside so a refusal can still name the field.
    let objects: Vec<String> = arguments
        .iter()
        .filter(|(key, value)| value.is_object() && fields.is_text(key))
        .map(|(key, _)| key.clone())
        .collect();
    let aside = (!objects.is_empty()).then(|| arguments.clone());
    serde_json::from_value::<T>(Value::Object(arguments)).map_err(|error| {
        let misplaced = aside.map(|arguments| misplaced_objects::<T>(&arguments, &objects));
        match misplaced {
            Some(problems) if !problems.is_empty() => fields.message::<T>(&problems),
            _ => fields.message::<T>(&[error.to_string()]),
        }
    })
}

/// Which of the string fields `objects` hold an object they cannot take, each as a problem naming
/// the field. serde's own error for this names the value (`invalid type: map, expected a string`),
/// not the field, so each candidate is tested by parsing again with the others set to a string.
fn misplaced_objects<T: ToolArgs>(
    arguments: &serde_json::Map<String, serde_json::Value>,
    objects: &[String],
) -> Vec<String> {
    use serde_json::Value;

    let parses_keeping = |kept: Option<&String>| {
        let mut probe = arguments.clone();
        for key in objects.iter().filter(|key| Some(*key) != kept) {
            probe.insert(key.clone(), Value::String(String::new()));
        }
        serde_json::from_value::<T>(Value::Object(probe)).is_ok()
    };
    if !parses_keeping(None) {
        return Vec::new();
    }
    objects
        .iter()
        .filter(|key| !parses_keeping(Some(key)))
        .map(|key| format!("`{key}` must be a string (got {})", shown(&arguments[key.as_str()])))
        .collect()
}

fn unknown_field(key: &str, accepted: &[&str]) -> String {
    let mut problem = format!("unknown field `{key}`");
    if key == "mode" {
        problem.push_str(" (mode-dispatched tools were replaced: each operation is its own tool)");
    } else if let Some(suggestion) = suggest(key, accepted) {
        problem.push_str(&format!(", did you mean `{suggestion}`?"));
    }
    problem
}

/// `value` as it is quoted back in an error, cut so a pasted document cannot fill the message.
fn shown(value: &serde_json::Value) -> String {
    const SHOWN_MAX: usize = 48;
    let mut text = value.to_string();
    if text.len() > SHOWN_MAX {
        let mut cut = SHOWN_MAX;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push('…');
    }
    text
}

/// What one argument's schema says its value must be.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// An unsigned integer no larger than `max`.
    Integer {
        max: u64,
    },
    Boolean,
    /// A string, optionally restricted to a fixed vocabulary.
    Text {
        one_of: Vec<String>,
    },
    /// An array whose elements are each of the one kind.
    Array(Box<Kind>),
    /// A shape this check does not model; serde decides.
    Other,
}

impl Kind {
    fn of(schema: &serde_json::Value) -> Self {
        use serde_json::Value;

        let is = |wanted: &str| match schema.get("type") {
            Some(Value::String(kind)) => kind == wanted,
            Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == wanted),
            _ => false,
        };
        if is("integer") {
            let max = match schema.get("format").and_then(Value::as_str) {
                Some("uint8") => u64::from(u8::MAX),
                Some("uint16") => u64::from(u16::MAX),
                Some("uint32") => u64::from(u32::MAX),
                _ => u64::MAX,
            };
            Self::Integer { max }
        } else if is("boolean") {
            Self::Boolean
        } else if is("string") {
            let one_of = schema
                .get("enum")
                .and_then(Value::as_array)
                .map(|variants| variants.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default();
            Self::Text { one_of }
        } else if is("array") {
            Self::Array(Box::new(schema.get("items").map_or(Self::Other, Self::of)))
        } else {
            Self::Other
        }
    }

    /// Coerce `value` in place where a string unambiguously stands for the scalar the field takes,
    /// and return what is wrong with it otherwise.
    fn check(&self, key: &str, value: &mut serde_json::Value, required: bool) -> Option<String> {
        use serde_json::Value;

        if value.is_null() {
            return required.then(|| format!("`{key}` is required and cannot be null"));
        }
        match self {
            Self::Integer { max } => {
                let parsed = match &*value {
                    Value::Number(number) => number.as_u64().or_else(|| integral(number.as_f64()?)),
                    Value::String(text) => text.trim().parse::<u64>().ok(),
                    _ => None,
                };
                match parsed {
                    Some(number) if number <= *max => {
                        *value = Value::from(number);
                        None
                    }
                    Some(_) => Some(format!("`{key}` is too large (got {})", shown(value))),
                    None => Some(format!(
                        "`{key}` must be an integer, zero or more (got {})",
                        shown(value)
                    )),
                }
            }
            Self::Boolean => {
                let parsed = match &*value {
                    Value::Bool(flag) => Some(*flag),
                    Value::String(text) if text.trim().eq_ignore_ascii_case("true") => Some(true),
                    Value::String(text) if text.trim().eq_ignore_ascii_case("false") => Some(false),
                    _ => None,
                };
                match parsed {
                    Some(flag) => {
                        *value = Value::Bool(flag);
                        None
                    }
                    None => Some(format!(
                        "`{key}` must be a boolean, true or false (got {})",
                        shown(value)
                    )),
                }
            }
            Self::Text { one_of } => match &*value {
                Value::String(text) if one_of.is_empty() || one_of.iter().any(|variant| variant == text) => None,
                // A path may also arrive as `{"bytes": […]}`; serde decides. A path has no vocabulary.
                Value::Object(_) if one_of.is_empty() => None,
                Value::String(_) | Value::Object(_) => Some(format!(
                    "`{key}` must be one of {} (got {})",
                    one_of
                        .iter()
                        .map(|variant| format!("`{variant}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    shown(value)
                )),
                _ => Some(format!("`{key}` must be a string (got {})", shown(value))),
            },
            Self::Array(item) => {
                let Value::Array(elements) = value else {
                    return Some(format!("`{key}` must be an array (got {})", shown(value)));
                };
                elements.iter_mut().enumerate().find_map(|(index, element)| {
                    let name = format!("{key}[{index}]");
                    // An element is never a path, so an object in a list of strings is refused here.
                    if matches!(**item, Self::Text { .. }) && element.is_object() {
                        return Some(format!("`{name}` must be a string (got {})", shown(element)));
                    }
                    item.check(&name, element, true)
                })
            }
            Self::Other => None,
        }
    }
}

/// `number` as an unsigned integer when it is one written as a float (`10.0`), which some clients
/// send for every JSON number. From 2^53 a float no longer names one integer: 2^53 + 1 written as
/// a float reads back as 2^53.
fn integral(number: f64) -> Option<u64> {
    const EXACT_MAX: f64 = 9_007_199_254_740_992.0;
    (number.fract() == 0.0 && (0.0..EXACT_MAX).contains(&number)).then_some(number as u64)
}

/// The accepted arguments of one tool, read off its schema.
struct Fields {
    /// Every property with what its value must be, in declaration order.
    kinds: Vec<(String, Kind)>,
    required: Vec<String>,
}

impl Fields {
    /// The table for `T`, built from its schema once per process: generating a schema allocates a
    /// few kilobytes of JSON, and every tool call needs the same answer.
    fn cached<T: ToolArgs>() -> std::sync::Arc<Self> {
        use std::any::TypeId;
        use std::sync::{Arc, LazyLock, RwLock};

        static TABLES: LazyLock<RwLock<ahash::AHashMap<TypeId, Arc<Fields>>>> = LazyLock::new(RwLock::default);
        let id = TypeId::of::<T>();
        if let Some(fields) = TABLES.read().ok().and_then(|tables| tables.get(&id).cloned()) {
            return fields;
        }
        let fields = Arc::new(Self::of::<T>());
        match TABLES.write() {
            Ok(mut tables) => Arc::clone(tables.entry(id).or_insert(fields)),
            Err(_) => fields,
        }
    }

    /// Whether `key` is a field that takes a string.
    fn is_text(&self, key: &str) -> bool {
        self.kinds
            .iter()
            .any(|(name, kind)| name == key && matches!(kind, Kind::Text { .. }))
    }

    fn of<T: JsonSchema>() -> Self {
        let schema = schemars::schema_for!(T);
        let object = schema.as_object();
        let kinds = match object.and_then(|object| object.get("properties")) {
            Some(serde_json::Value::Object(properties)) => properties
                .iter()
                .map(|(name, property)| (name.clone(), Kind::of(property)))
                .collect(),
            _ => Vec::new(),
        };
        let required = match object.and_then(|object| object.get("required")) {
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        };
        Self { kinds, required }
    }

    fn names(&self) -> Vec<&str> {
        self.kinds.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// `problems` as one message, followed by the accepted set.
    fn message<T: ToolArgs>(&self, problems: &[String]) -> String {
        format!(
            "`{}`: {}. Accepted fields: {}",
            T::TOOL,
            problems.join("; "),
            render_fields(&self.names(), &self.required)
        )
    }
}

fn render_fields(accepted: &[&str], required: &[String]) -> String {
    if accepted.is_empty() {
        return "none".to_string();
    }
    accepted
        .iter()
        .map(|field| {
            if required.iter().any(|name| name == field) {
                format!("`{field}` (required)")
            } else {
                format!("`{field}`")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The accepted field an unknown `key` most likely meant: a synonym first, then, for a key long
/// enough to tell apart, a near-miss spelling.
fn suggest<'a>(key: &str, accepted: &[&'a str]) -> Option<&'a str> {
    for group in SYNONYMS {
        if !group.contains(&key) {
            continue;
        }
        if let Some(field) = accepted.iter().find(|field| group[0] == **field) {
            return Some(field);
        }
    }
    for group in SYNONYMS {
        if !group.contains(&key) {
            continue;
        }
        if let Some(field) = accepted.iter().find(|field| group.contains(field)) {
            return Some(field);
        }
    }
    if key.chars().count() < NEAR_MISS_MIN_KEY_LEN {
        return None;
    }
    accepted
        .iter()
        .map(|field| (levenshtein(key, field), *field))
        .filter(|(distance, _)| *distance <= NEAR_MISS_MAX_DISTANCE)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, field)| field)
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
    fn stringified_scalars_are_coerced_for_integer_and_boolean_fields() {
        let params =
            parse::<CodeGrepParams>(json!({ "pattern": "x", "limit": " 10 ", "regex": "TRUE", "context": "false" }))
                .expect("coerced");
        assert_eq!(params.limit, Some(10));
        assert_eq!(params.regex, Some(true));
        assert_eq!(params.context, Some(false));
        let literal = parse::<CodeGrepParams>(json!({ "pattern": "10" })).expect("a string field keeps its string");
        assert_eq!(literal.pattern, "10");
    }

    #[test]
    fn a_type_error_names_its_field() {
        let message = error(json!({ "pattern": "x", "limit": "many" }));
        assert!(message.contains("`limit` must be an integer"), "{message}");
        assert!(message.contains("\"many\""), "{message}");
        let negative = error(json!({ "pattern": "x", "limit": -1 }));
        assert!(
            negative.contains("`limit` must be an integer, zero or more"),
            "{negative}"
        );
        let huge = error(json!({ "pattern": "x", "limit": 99_999_999_999u64 }));
        assert!(huge.contains("`limit` is too large"), "{huge}");
        let flag = error(json!({ "pattern": "x", "regex": 1 }));
        assert!(flag.contains("`regex` must be a boolean"), "{flag}");
        let text = error(json!({ "pattern": 7 }));
        assert!(text.contains("`pattern` must be a string"), "{text}");
    }

    #[test]
    fn unknown_keys_type_errors_and_missing_fields_are_reported_together() {
        let message = error(json!({ "glob": "*.rs", "limit": "many" }));
        assert!(message.contains("unknown field `glob`"), "{message}");
        assert!(message.contains("`limit` must be an integer"), "{message}");
        assert!(message.contains("missing field `pattern`"), "{message}");
    }

    #[test]
    fn null_is_absent_for_an_optional_field_and_an_error_for_a_required_one() {
        let params = parse::<CodeGrepParams>(json!({ "pattern": "x", "limit": null, "path": null })).expect("valid");
        assert_eq!(params.limit, None);
        let message = error(json!({ "pattern": null }));
        assert!(
            message.contains("`pattern` is required and cannot be null"),
            "{message}"
        );
    }

    #[test]
    fn absent_arguments_are_the_empty_object() {
        use crate::mcp::types_tools::IndexStatusParams;
        parse::<IndexStatusParams>(serde_json::Value::Null).expect("no arguments");
        parse::<IndexStatusParams>(json!({})).expect("empty arguments");
        let message = match parse::<IndexStatusParams>(json!([])) {
            Ok(_) => panic!("an array is not arguments"),
            Err(message) => message,
        };
        assert!(message.contains("arguments must be a JSON object"), "{message}");
        assert!(message.contains("Accepted fields: none"), "{message}");
    }

    #[test]
    fn a_fixed_vocabulary_is_checked_by_field_name() {
        use crate::mcp::types_tools::CodeRefsParams;
        let message = match parse::<CodeRefsParams>(json!({ "name": "x", "relation": "users" })) {
            Ok(_) => panic!("expected a rejection"),
            Err(message) => message,
        };
        assert!(
            message.contains("`relation` must be one of `callers`, `implementations`, `importers`, `definition`"),
            "{message}"
        );
    }

    #[test]
    fn a_short_unknown_key_gets_no_spelling_suggestion() {
        let short = error(json!({ "pattern": "x", "pat": "src/" }));
        assert!(short.contains("unknown field `pat`"), "{short}");
        assert!(!short.contains("did you mean"), "{short}");
        let long = error(json!({ "pattern": "x", "patth": "src/" }));
        assert!(long.contains("unknown field `patth`, did you mean `path`?"), "{long}");
    }

    #[test]
    fn a_synonym_with_a_different_vocabulary_is_not_suggested() {
        use crate::mcp::types_tools::{CodeRefsParams, GitBlameParams};
        let row = match parse::<GitBlameParams>(json!({ "path": "a.rs", "row": 3 })) {
            Ok(_) => panic!("expected a rejection"),
            Err(message) => message,
        };
        assert!(row.contains("unknown field `row`"), "{row}");
        assert!(
            !row.contains("did you mean"),
            "a 0-based row is not a 1-based line: {row}"
        );
        let direction = match parse::<CodeRefsParams>(json!({ "name": "x", "direction": "callees" })) {
            Ok(_) => panic!("expected a rejection"),
            Err(message) => message,
        };
        assert!(!direction.contains("did you mean `relation`"), "{direction}");
    }

    #[test]
    fn levenshtein_matches_known_distances() {
        assert_eq!(levenshtein("pattern", "pattern"), 0);
        assert_eq!(levenshtein("patern", "pattern"), 1);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    #[test]
    fn an_object_in_a_string_field_names_the_field() {
        use crate::mcp::types_tools::{CodeReadParams, CodeRefsParams};

        let message = error(json!({ "pattern": { "bytes": [120] } }));
        assert!(message.contains("`pattern` must be a string"), "{message}");
        assert!(!message.contains("invalid type"), "{message}");

        let both = error(json!({ "pattern": { "a": 1 }, "language": {}, "path": "src/" }));
        assert!(both.contains("`pattern` must be a string"), "{both}");
        assert!(both.contains("`language` must be a string"), "{both}");

        let path = parse::<CodeReadParams>(json!({ "path": { "bytes": [97, 46, 114, 115] }, "line": 1 }))
            .expect("a path may arrive as raw bytes");
        assert_eq!(path.path.as_str(), Some("a.rs"));

        let mixed = match parse::<CodeRefsParams>(json!({ "name": {}, "path": { "bytes": [97] } })) {
            Ok(_) => panic!("expected a rejection"),
            Err(message) => message,
        };
        assert!(mixed.contains("`name` must be a string"), "{mixed}");
        assert!(!mixed.contains("`path` must be a string"), "{mixed}");
    }

    #[test]
    fn a_wrong_typed_array_element_names_the_field_and_its_index() {
        use crate::mcp::types_tools::MemorySaveParams;

        let save = |tags: serde_json::Value| match parse::<MemorySaveParams>(
            json!({ "key": "k", "value": "v", "tags": tags }),
        ) {
            Ok(_) => panic!("expected a rejection"),
            Err(message) => message,
        };
        let number = save(json!(["a", 7]));
        assert!(number.contains("`tags[1]` must be a string (got 7)"), "{number}");
        let object = save(json!([{ "bytes": [97] }]));
        assert!(object.contains("`tags[0]` must be a string"), "{object}");
        let null = save(json!(["a", null]));
        assert!(null.contains("`tags[1]`"), "{null}");
        assert!(save(json!("a")).contains("`tags` must be an array"));

        let fine = parse::<MemorySaveParams>(json!({ "key": "k", "value": "v", "tags": ["a", "b"] })).expect("valid");
        assert_eq!(fine.tags.as_deref(), Some(&["a".to_string(), "b".to_string()][..]));
    }

    #[test]
    fn an_integral_float_is_an_integer() {
        let params = parse::<CodeGrepParams>(json!({ "pattern": "x", "limit": 10.0 })).expect("10.0 is 10");
        assert_eq!(params.limit, Some(10));
        let fraction = error(json!({ "pattern": "x", "limit": 10.5 }));
        assert!(fraction.contains("`limit` must be an integer"), "{fraction}");
        let negative = error(json!({ "pattern": "x", "limit": -1.0 }));
        assert!(negative.contains("`limit` must be an integer"), "{negative}");
        let huge = error(json!({ "pattern": "x", "limit": 1e12 }));
        assert!(huge.contains("`limit` is too large"), "{huge}");
    }

    #[test]
    fn a_float_is_an_integer_only_below_two_to_the_53() {
        assert_eq!(integral(9_007_199_254_740_991.0), Some(9_007_199_254_740_991));
        assert_eq!(integral(9_007_199_254_740_992.0), None);
        let rounded: serde_json::Value = serde_json::from_str("9007199254740993.0").expect("json");
        assert_eq!(rounded.as_f64().and_then(integral), None, "2^53 + 1 is not 2^53");
    }

    #[test]
    fn an_object_in_a_fixed_vocabulary_field_names_the_field() {
        use crate::mcp::types_tools::CodeRefsParams;
        let message = match parse::<CodeRefsParams>(json!({ "name": "x", "relation": {} })) {
            Ok(_) => panic!("expected a rejection"),
            Err(message) => message,
        };
        assert!(message.contains("`relation` must be one of `callers`"), "{message}");
        assert!(message.contains("(got {})"), "{message}");
        assert!(!message.contains("invalid type"), "{message}");
    }

    #[test]
    fn the_field_table_of_a_tool_is_built_once() {
        let first = Fields::cached::<CodeGrepParams>();
        let again = Fields::cached::<CodeGrepParams>();
        assert!(std::sync::Arc::ptr_eq(&first, &again));
        assert!(first.is_text("pattern") && !first.is_text("limit"));
        let other = Fields::cached::<crate::mcp::types_tools::CodeOutlineParams>();
        assert!(!std::sync::Arc::ptr_eq(&first, &other));
    }
}
