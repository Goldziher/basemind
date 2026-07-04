//! Native TOON (Token-Oriented Object Notation) encoder for high-volume list responses.
//!
//! TOON is a compact tabular encoding of a uniform array-of-objects: a single header line
//! naming the columns, then one comma-joined row per object. For the list responses basemind
//! returns — `search_symbols`, `find_references`, `workspace_grep`, `list_files`, `outline` —
//! this drops the per-row `"key":` repetition that dominates JSON's token cost.
//!
//! ## Why native (not `serde_toon`)
//!
//! The crate already pulls `serde_toon_format`, but only behind the `documents` feature
//! (`dep:serde_toon_format` lives in that feature list). The tools above are core tools present
//! in the default build, so their encoder must compile with no features enabled. This module is
//! therefore a small, dependency-free encoder over [`serde_json::Value`].
//!
//! ## Shape handled
//!
//! basemind responses are *envelopes*: a flat object of scalar metadata (`total`, `truncated`,
//! …) plus exactly one high-volume array field (`results` / `hits` / `files` / `symbols`). The
//! encoder emits the scalars as `key: value` lines and renders the array field as a TOON table
//! when — and only when — it is a uniform array of flat objects (every element an object with
//! the same key set, all values scalar). Anything that does not fit that mold (nested objects,
//! ragged key sets, arrays of scalars/arrays) falls back to compact JSON for that value, so the
//! output is always lossless and round-trippable.

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;
use serde_json::Value;

/// Wire format an agent can opt into per tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResponseFormat {
    /// Compact JSON via the existing `ContentBlock::json` path (the default, backward compatible).
    Json,
    /// Native TOON for the list array, scalars as `key: value` lines.
    Toon,
}

impl ResponseFormat {
    /// Parse the optional `format` (alias `encoding`) tool param. Unknown / absent values
    /// resolve to [`ResponseFormat::Json`] so the default stays backward compatible and a typo
    /// degrades to JSON rather than erroring the call.
    pub(super) fn parse(opt: Option<&str>) -> Self {
        match opt.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("toon") => Self::Toon,
            _ => Self::Json,
        }
    }
}

/// Serialize `value` into a `CallToolResult` honoring the requested wire format.
///
/// `Json` delegates to the canonical [`super::helpers::json_result`] (`ContentBlock::json`). `Toon`
/// renders [`encode`] output into a plain `ContentBlock::text` item so the agent receives the compact
/// table on the wire.
pub(super) fn format_result<T: Serialize>(value: &T, fmt: ResponseFormat) -> Result<CallToolResult, McpError> {
    match fmt {
        ResponseFormat::Json => super::helpers::json_result(value),
        ResponseFormat::Toon => {
            let json = serde_json::to_value(value)
                .map_err(|e| McpError::internal_error(format!("toon: serialize: {e}"), None))?;
            Ok(CallToolResult::success(vec![ContentBlock::text(encode(&json))]))
        }
    }
}

/// Encode a `serde_json::Value` as TOON, falling back to compact JSON for anything that is not a
/// uniform array of flat objects (directly, or as the array field of a flat envelope object).
pub(super) fn encode(value: &Value) -> String {
    match value {
        Value::Array(items) => match encode_table(items) {
            Some(table) => table,
            None => compact_json(value),
        },
        Value::Object(map) => encode_envelope(map),
        other => compact_json(other),
    }
}

/// Render a flat envelope: scalar fields as `key: value` lines, plus each array-of-flat-objects
/// field as a TOON table block. Nested objects and other non-tabular fields fall back to compact
/// JSON on their own line so the envelope stays lossless.
fn encode_envelope(map: &serde_json::Map<String, Value>) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(map.len());
    // Iterate keys in sorted order so the output is deterministic regardless of whether
    // `serde_json`'s `preserve_order` feature is active in the dependency graph (it is pulled
    // transitively by the lancedb/arrow stack). Sorting here keeps TOON byte-stable for agents.
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort_unstable();
    for key in keys {
        let val = &map[key];
        match val {
            Value::Array(items) => {
                if let Some(table) = encode_table_block(key, items) {
                    lines.push(table);
                } else {
                    lines.push(format!("{key}: {}", compact_json(val)));
                }
            }
            Value::Object(_) => lines.push(format!("{key}: {}", compact_json(val))),
            scalar => lines.push(format!("{key}: {}", scalar_to_toon(scalar))),
        }
    }
    lines.join("\n")
}

/// Render a named array field as a labeled TOON table block:
///
/// ```text
/// results[2]{path,name}:
///   a.rs,alpha
///   b.rs,beta
/// ```
///
/// Returns `None` when `items` is not a uniform array of flat objects (caller falls back).
fn encode_table_block(field: &str, items: &[Value]) -> Option<String> {
    let (columns, rows) = table_parts(items)?;
    let header = format!("{field}[{}]{{{}}}:", items.len(), columns.join(","));
    if rows.is_empty() {
        return Some(header);
    }
    let body = rows.iter().map(|r| format!("  {r}")).collect::<Vec<_>>().join("\n");
    Some(format!("{header}\n{body}"))
}

/// Render a bare top-level array as a TOON table (header line + one row per element).
/// Returns `None` when the array is not a uniform array of flat objects.
fn encode_table(items: &[Value]) -> Option<String> {
    let (columns, rows) = table_parts(items)?;
    let header = format!("[{}]{{{}}}:", items.len(), columns.join(","));
    if rows.is_empty() {
        return Some(header);
    }
    Some(format!("{header}\n{}", rows.join("\n")))
}

/// Validate uniformity and build `(columns, rows)` for a candidate table.
///
/// Requirements (any violation → `None`, caller falls back to JSON):
/// - the array is non-empty;
/// - every element is an object;
/// - every element has the exact same set of keys as the first;
/// - every value is a scalar (null / bool / number / string) — no nested objects or arrays.
///
/// Columns are emitted in sorted order so the header is deterministic regardless of the
/// `serde_json` `preserve_order` feature (pulled transitively by the lancedb/arrow stack).
fn table_parts(items: &[Value]) -> Option<(Vec<String>, Vec<String>)> {
    let first = items.first()?.as_object()?;
    if first.is_empty() {
        return None;
    }
    let mut columns: Vec<String> = first.keys().cloned().collect();
    columns.sort_unstable();
    let mut rows: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let obj = item.as_object()?;
        if obj.len() != columns.len() {
            return None;
        }
        let mut cells: Vec<String> = Vec::with_capacity(columns.len());
        for col in &columns {
            let cell = obj.get(col)?;
            if !is_scalar(cell) {
                return None;
            }
            cells.push(scalar_cell(cell));
        }
        rows.push(cells.join(","));
    }
    Some((columns, rows))
}

fn is_scalar(value: &Value) -> bool {
    !matches!(value, Value::Object(_) | Value::Array(_))
}

/// Render a scalar as a TOON `key: value` right-hand side. Strings are emitted bare unless they
/// need quoting (see [`needs_quote`]); other scalars use their JSON form.
fn scalar_to_toon(value: &Value) -> String {
    match value {
        Value::String(s) => maybe_quote(s),
        other => compact_json(other),
    }
}

/// Render a scalar as a single table cell. Identical to [`scalar_to_toon`] except a string
/// containing the column delimiter (`,`) or a newline is always quoted so the row stays parseable.
fn scalar_cell(value: &Value) -> String {
    match value {
        Value::String(s) => {
            if s.contains(',') || s.contains('\n') || needs_quote(s) {
                quote(s)
            } else {
                s.clone()
            }
        }
        other => compact_json(other),
    }
}

/// Quote a string only when leaving it bare would be ambiguous (empty, leading/trailing space,
/// or a leading character that would otherwise parse as a non-string scalar).
fn maybe_quote(s: &str) -> String {
    if needs_quote(s) { quote(s) } else { s.to_string() }
}

fn needs_quote(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    if s.starts_with(' ') || s.ends_with(' ') {
        return true;
    }
    // Leading char that would re-parse as bool / null / number / a quoted string.
    matches!(s, "true" | "false" | "null")
        || s.starts_with(['"', '-', '+'])
        || s.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// JSON-quote a string (handles escaping); falls back to a manual wrap on the impossible
/// serialize error so this never panics.
fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_recognizes_toon_case_insensitively() {
        assert_eq!(ResponseFormat::parse(Some("toon")), ResponseFormat::Toon);
        assert_eq!(ResponseFormat::parse(Some("TOON")), ResponseFormat::Toon);
        assert_eq!(ResponseFormat::parse(Some(" Toon ")), ResponseFormat::Toon);
        assert_eq!(ResponseFormat::parse(Some("json")), ResponseFormat::Json);
        assert_eq!(ResponseFormat::parse(None), ResponseFormat::Json);
        assert_eq!(ResponseFormat::parse(Some("garbage")), ResponseFormat::Json);
    }

    // NOTE: the encoder sorts envelope keys and table columns explicitly (see `encode_envelope`
    // / `table_parts`), so the output is alphabetically sorted and deterministic regardless of
    // whether `serde_json`'s `preserve_order` feature is active in the dependency graph (it is,
    // transitively, via the lancedb/arrow stack). That is valid TOON — the header names the
    // columns — so the exact-output tests below assert the sorted form.
    #[test]
    fn encodes_uniform_array_as_exact_table() {
        let value = json!([
            { "path": "a.rs", "line": 1 },
            { "path": "b.rs", "line": 2 },
        ]);
        let toon = encode(&value);
        assert_eq!(toon, "[2]{line,path}:\n1,a.rs\n2,b.rs");
    }

    #[test]
    fn encodes_envelope_scalars_then_table() {
        let value = json!({
            "total": 2,
            "truncated": false,
            "results": [
                { "path": "a.rs", "name": "alpha" },
                { "path": "b.rs", "name": "beta" },
            ],
        });
        let toon = encode(&value);
        assert_eq!(
            toon,
            "results[2]{name,path}:\n  alpha,a.rs\n  beta,b.rs\ntotal: 2\ntruncated: false"
        );
    }

    #[test]
    fn empty_array_field_renders_as_json_fallback() {
        let value = json!({ "total": 0, "results": [] });
        let toon = encode(&value);
        // An empty array is not a uniform array of objects, so it falls back to compact JSON.
        // Fields sort alphabetically: `results` before `total`.
        assert_eq!(toon, "results: []\ntotal: 0");
    }

    #[test]
    fn non_uniform_array_falls_back_to_json() {
        // Ragged key sets — second object is missing `line`.
        let value = json!([
            { "path": "a.rs", "line": 1 },
            { "path": "b.rs" },
        ]);
        let toon = encode(&value);
        assert_eq!(toon, compact_json(&value));
    }

    #[test]
    fn nested_object_value_falls_back_to_json() {
        let value = json!([{ "path": "a.rs", "loc": { "line": 1 } }]);
        let toon = encode(&value);
        assert_eq!(toon, compact_json(&value));
    }

    #[test]
    fn cells_with_delimiters_are_quoted() {
        let value = json!([{ "sig": "fn f(a, b)", "name": "f" }]);
        let toon = encode(&value);
        // Columns sort alphabetically: `name` before `sig`.
        assert_eq!(toon, "[1]{name,sig}:\nf,\"fn f(a, b)\"");
    }

    #[test]
    fn toon_is_smaller_than_json_for_list_payload() {
        // A representative `search_symbols`-style envelope with several hits.
        let value = json!({
            "total": 4,
            "truncated": false,
            "results": [
                { "path": "src/a.rs", "name": "alpha", "kind": "function", "start_row": 1 },
                { "path": "src/b.rs", "name": "beta", "kind": "function", "start_row": 2 },
                { "path": "src/c.rs", "name": "gamma", "kind": "struct", "start_row": 3 },
                { "path": "src/d.rs", "name": "delta", "kind": "method", "start_row": 4 },
            ],
        });
        let toon = encode(&value);
        let json = serde_json::to_string(&value).unwrap();
        assert!(
            toon.len() < json.len(),
            "TOON ({} bytes) should be smaller than JSON ({} bytes)\nTOON:\n{toon}",
            toon.len(),
            json.len(),
        );
    }
}
