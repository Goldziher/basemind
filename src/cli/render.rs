//! Rendering for the in-process CLI.
//!
//! The CLI calls the exact MCP `#[tool]` methods and receives the same
//! [`CallToolResult`] an MCP client would. Tools serialize their response via
//! `Content::json`, so the JSON payload lives in the first text content block.
//! [`result_to_value`] extracts and parses it; [`render_human`] turns it into a
//! readable, generic table / key-value view that works for every tool without
//! per-tool code (with a few high-traffic special cases for nicer output).

use std::io::Write;

use anyhow::{Context, Result};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

/// Maximum characters of a string value rendered inline before truncation.
const MAX_INLINE_LEN: usize = 200;
/// Maximum number of array items rendered in human mode before a summary line.
const MAX_HUMAN_ITEMS: usize = 1000;

/// Render a tool result to the writer, honoring the `--json` switch.
///
/// `tool_name` selects the human special-case renderer. On a tool error the
/// `McpError` is surfaced as an `anyhow` error by the caller before this runs.
pub fn emit(tool_name: &str, result: &CallToolResult, json: bool, out: &mut impl Write) -> Result<()> {
    let value = result_to_value(result)?;
    if json {
        render_json(&value, out)
    } else {
        render_human(tool_name, &value, out)
    }
}

/// Extract the JSON payload from a tool result.
///
/// basemind tools always return a single `Content::json` block whose `text`
/// field is the serialized response. We parse that text back into a [`Value`].
pub fn result_to_value(result: &CallToolResult) -> Result<Value> {
    for content in &result.content {
        if let ContentBlock::Text(text) = content {
            return serde_json::from_str(&text.text).with_context(|| "parse tool JSON response");
        }
    }
    anyhow::bail!("tool returned no text content")
}

/// Print the JSON value as pretty JSON.
pub fn render_json(value: &Value, out: &mut impl Write) -> Result<()> {
    let s = serde_json::to_string_pretty(value).context("serialize JSON output")?;
    writeln!(out, "{s}").context("write JSON output")?;
    Ok(())
}

/// Truncate a string to [`MAX_INLINE_LEN`] characters, appending an ellipsis marker.
fn truncate(s: &str) -> String {
    let flat = s.replace('\n', " ");
    if flat.chars().count() <= MAX_INLINE_LEN {
        return flat;
    }
    let cut: String = flat.chars().take(MAX_INLINE_LEN).collect();
    format!("{cut}…")
}

/// Render a scalar JSON value to a compact string.
fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => truncate(s),
        other => truncate(&other.to_string()),
    }
}

/// Render a tool response for humans. Generic across all tools:
/// - An object whose dominant payload is an array of objects → an aligned table.
/// - An array of objects at the top level → an aligned table.
/// - A scalar/flat object → `key: value` lines.
///
/// `tool_name` enables a few nicer special-cases (outline, search, references).
pub fn render_human(tool_name: &str, value: &Value, out: &mut impl Write) -> Result<()> {
    match value {
        Value::Object(map) => {
            let object_arrays: Vec<(&str, &Vec<Value>)> = map
                .iter()
                .filter_map(|(k, v)| match v {
                    Value::Array(items) if items.first().is_some_and(|i| i.is_object()) => Some((k.as_str(), items)),
                    _ => None,
                })
                .collect();

            for (key, v) in map.iter() {
                if object_arrays.iter().any(|(k, _)| *k == key) {
                    continue;
                }
                match v {
                    Value::Array(items) if !items.is_empty() => {
                        let joined: Vec<String> = items.iter().map(scalar_to_string).collect();
                        writeln!(out, "{key}: {}", joined.join(", "))?;
                    }
                    Value::Array(_) => writeln!(out, "{key}: (empty)")?,
                    Value::Object(_) => writeln!(out, "{key}: {}", scalar_to_string(v))?,
                    _ => writeln!(out, "{key}: {}", scalar_to_string(v))?,
                }
            }

            for (key, items) in &object_arrays {
                writeln!(out, "\n{key} ({} items):", items.len())?;
                render_table(tool_name, items, out)?;
            }
        }
        Value::Array(items) if items.first().is_some_and(|i| i.is_object()) => {
            render_table(tool_name, items, out)?;
        }
        Value::Array(items) => {
            for item in items {
                writeln!(out, "{}", scalar_to_string(item))?;
            }
        }
        other => writeln!(out, "{}", scalar_to_string(other))?,
    }
    Ok(())
}

/// Render an array of objects as an aligned table. Columns are the union of keys
/// of the first item (stable order), with nested arrays/objects collapsed.
fn render_table(tool_name: &str, items: &[Value], out: &mut impl Write) -> Result<()> {
    if items.is_empty() {
        writeln!(out, "  (none)")?;
        return Ok(());
    }

    if let Some(rendered) = render_special(tool_name, items, out)? {
        return Ok(rendered);
    }

    let Some(first) = items.first().and_then(Value::as_object) else {
        for item in items {
            writeln!(out, "  {}", scalar_to_string(item))?;
        }
        return Ok(());
    };
    let columns: Vec<&str> = first.keys().map(String::as_str).collect();

    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    let display = items.len().min(MAX_HUMAN_ITEMS);
    let rows: Vec<Vec<String>> = items
        .iter()
        .take(display)
        .map(|item| {
            columns
                .iter()
                .map(|col| item.get(*col).map(scalar_to_string).unwrap_or_default())
                .collect()
        })
        .collect();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    let header: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{:<width$}", c, width = widths[i]))
        .collect();
    writeln!(out, "  {}", header.join("  "))?;
    for row in &rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:<width$}", c, width = widths[i]))
            .collect();
        writeln!(out, "  {}", cells.join("  "))?;
    }
    if items.len() > display {
        writeln!(out, "  … and {} more", items.len() - display)?;
    }
    Ok(())
}

/// Special-cased compact renderers for high-traffic tools. Returns `Some(())`
/// when it handled the items, `None` to fall through to the generic table.
fn render_special(tool_name: &str, items: &[Value], out: &mut impl Write) -> Result<Option<()>> {
    match tool_name {
        "outline" | "search_symbols" => {
            for item in items.iter().take(MAX_HUMAN_ITEMS) {
                let Some(obj) = item.as_object() else {
                    return Ok(None);
                };
                let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
                let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("");
                let row = obj.get("start_row").and_then(Value::as_u64).map(|r| r + 1).unwrap_or(0);
                let path = obj.get("path").and_then(Value::as_str);
                let sig = obj.get("signature").and_then(Value::as_str).unwrap_or("");
                match path {
                    Some(p) => writeln!(out, "  {p}:{row} {kind:<10} {name} {sig}", sig = truncate(sig))?,
                    None => writeln!(out, "  {row:>5} {kind:<10} {name} {sig}", sig = truncate(sig))?,
                }
            }
            if items.len() > MAX_HUMAN_ITEMS {
                writeln!(out, "  … and {} more", items.len() - MAX_HUMAN_ITEMS)?;
            }
            Ok(Some(()))
        }
        "find_references" | "find_callers" => {
            for item in items.iter().take(MAX_HUMAN_ITEMS) {
                let Some(obj) = item.as_object() else {
                    return Ok(None);
                };
                let path = obj.get("path").and_then(Value::as_str).unwrap_or("");
                let line = obj.get("line").and_then(Value::as_u64).unwrap_or(0);
                let col = obj.get("column").and_then(Value::as_u64).unwrap_or(0);
                let callee = obj.get("callee").and_then(Value::as_str).unwrap_or("");
                writeln!(out, "  {path}:{line}:{col} {callee}")?;
            }
            if items.len() > MAX_HUMAN_ITEMS {
                writeln!(out, "  … and {} more", items.len() - MAX_HUMAN_ITEMS)?;
            }
            Ok(Some(()))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::render_human;
    use serde_json::json;

    fn render(value: &serde_json::Value) -> String {
        let mut buf: Vec<u8> = Vec::new();
        render_human("diff_outline", value, &mut buf).expect("render");
        String::from_utf8(buf).expect("utf8")
    }

    #[test]
    fn renders_every_object_array_as_labeled_table() {
        let value = json!({
            "added": [{"name": "alpha"}],
            "removed": [{"name": "beta"}],
            "common": [{"name": "gamma"}],
        });
        let out = render(&value);
        assert!(out.contains("added (1 items):"), "missing added table: {out}");
        assert!(out.contains("removed (1 items):"), "missing removed table: {out}");
        assert!(out.contains("common (1 items):"), "missing common table: {out}");
        assert!(out.contains("alpha") && out.contains("beta") && out.contains("gamma"));
        assert!(!out.contains("items)\nremoved"), "removed was summarized: {out}");
    }
}
