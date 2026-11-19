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

/// Maximum characters of a string rendered in one table cell (or one outline signature) before it is
/// cut with an ellipsis. This is a deliberate column display, not a data limit: every cut is counted
/// and reported on stderr, and `--json` always carries the full value. Key/value fields are never
/// cut.
const MAX_CELL_LEN: usize = 200;
/// Maximum number of array items rendered in human mode before a summary line.
const MAX_HUMAN_ITEMS: usize = 1000;
/// Below this many microseconds a duration renders as `N µs`; at or above it, as `N.N ms`.
const MS_THRESHOLD_US: u64 = 1_000;

/// Output options for one CLI tool invocation, plus the startup cost the CLI can attribute
/// to itself.
///
/// The tool's own latency (`elapsed_us`) is reported by the tool body and arrives inside the
/// response. `startup_us` is the *other* half of what a shell `time basemind …` measures — and
/// reporting the two separately is the whole point: it tells you how much of a wrapped `time`
/// measurement was never the query.
pub struct Emit {
    /// The `--json` switch.
    pub json: bool,
    /// Microseconds from `main()` entry to the instant the tool body is invoked: clap parsing,
    /// tracing setup, repo-root discovery, grammar check, the tokio runtime build, the read-only
    /// store open, the config load, and the git-cache open.
    ///
    /// Excludes pre-`main` process cost (exec, dynamic linking, Rust runtime init), which a
    /// process cannot observe about itself — so `startup_us + elapsed_us` is a lower bound on,
    /// not an exact reproduction of, an external `time` measurement.
    ///
    /// A long-running `basemind serve` (and therefore every MCP call) pays this **once** at boot,
    /// not per query. It is a CLI-only cost.
    pub startup_us: u64,
    /// When the tool call began: the query time the footer reports for a tool whose response
    /// carries no `elapsed_us` of its own (the locating tools leave it out).
    pub called: std::time::Instant,
}

/// Render a duration compactly: `285 µs`, or `41.2 ms` once it reaches a millisecond.
fn format_us(us: u64) -> String {
    if us < MS_THRESHOLD_US {
        format!("{us} µs")
    } else {
        format!("{:.1} ms", us as f64 / 1_000.0)
    }
}

/// Render a tool result to the writer, honoring the `--json` switch. Diagnostics (the timing
/// footer, the truncated-cell notice) go to the process's stderr; see [`emit_to`].
pub fn emit(tool_name: &str, result: &CallToolResult, opts: &Emit, out: &mut impl Write) -> Result<()> {
    emit_to(tool_name, result, opts, out, &mut std::io::stderr().lock())
}

/// [`emit`] with an explicit diagnostics writer.
///
/// `tool_name` selects the human special-case renderer. On a tool error the `McpError` is surfaced
/// as an `anyhow` error by the caller before this runs.
///
/// In `--json` mode the tool's own `elapsed_us` is passed through untouched and `startup_us` is
/// added alongside it. In human mode both are lifted out of the payload and reported as a compact
/// timing line on `err`, so stdout carries only the answer and stays pipe-clean.
///
/// Both modes read the structured content: a tool's text block is written for a model
/// ([`crate::wire`]), not for the terminal.
pub fn emit_to(
    tool_name: &str,
    result: &CallToolResult,
    opts: &Emit,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<()> {
    let mut value = payload(result)?;
    if opts.json {
        if let Value::Object(map) = &mut value {
            map.insert("startup_us".to_string(), Value::from(opts.startup_us));
        }
        return render_json(&value, out);
    }

    let reported = match &mut value {
        Value::Object(map) => map.remove("elapsed_us").and_then(|v| v.as_u64()),
        _ => None,
    };
    let elapsed_us = reported.or_else(|| u64::try_from(opts.called.elapsed().as_micros()).ok());
    let mut cut = Truncated::default();
    render_human_counting(tool_name, &value, out, &mut cut)?;
    if cut.cells > 0 {
        writeln!(
            err,
            "note: {} table cell(s) were cut to {MAX_CELL_LEN} characters for display; rerun with --json for the full values",
            cut.cells
        )?;
    }
    match elapsed_us {
        Some(us) => writeln!(
            err,
            "({} query · {} startup)",
            format_us(us),
            format_us(opts.startup_us)
        )?,
        None => writeln!(err, "({} startup)", format_us(opts.startup_us))?,
    }
    Ok(())
}

/// Count of table cells [`cell`] had to cut, so the caller can say so.
#[derive(Default)]
struct Truncated {
    cells: usize,
}

/// The tool's data: the structured content when present (every verb tool sets it), otherwise the
/// first text block read back with [`crate::wire::decode`].
fn payload(result: &CallToolResult) -> Result<Value> {
    if let Some(structured) = &result.structured_content {
        return Ok(structured.clone());
    }
    let text = result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .context("tool returned no text content")?;
    crate::wire::decode(text).map_err(|e| anyhow::anyhow!("parse tool response: {e}"))
}

/// Extract the JSON payload from a tool result: the structured content when present, otherwise the
/// first text block parsed as JSON.
pub fn result_to_value(result: &CallToolResult) -> Result<Value> {
    payload(result)
}

/// Print the JSON value as pretty JSON.
pub fn render_json(value: &Value, out: &mut impl Write) -> Result<()> {
    let s = serde_json::to_string_pretty(value).context("serialize JSON output")?;
    writeln!(out, "{s}").context("write JSON output")?;
    Ok(())
}

/// Flatten one value to a single table cell, cutting it to [`MAX_CELL_LEN`] characters and counting
/// the cut. Newlines become spaces so a cell never breaks the table.
fn cell(value: &Value, cut: &mut Truncated) -> String {
    let flat = match value {
        Value::String(s) => s.replace('\n', " "),
        other => scalar_to_string(other).replace('\n', " "),
    };
    clip(&flat, cut)
}

/// Cut `flat` to [`MAX_CELL_LEN`] characters, appending an ellipsis and counting the cut.
fn clip(flat: &str, cut: &mut Truncated) -> String {
    if flat.chars().count() <= MAX_CELL_LEN {
        return flat.to_string();
    }
    cut.cells += 1;
    let head: String = flat.chars().take(MAX_CELL_LEN).collect();
    format!("{head}…")
}

/// Render a value to a string in full: strings bare, everything else as compact JSON.
fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Render a tool response for humans. Generic across all tools:
/// - An object whose dominant payload is an array of objects → an aligned table.
/// - An array of objects at the top level → an aligned table.
/// - A scalar/flat object → `key: value` lines; a multi-line string (a source body, a diff, an
///   export) prints raw on the lines after `key:` so it can be piped or pasted as-is.
///
/// Key/value strings are never shortened; only table cells are cut (see [`MAX_CELL_LEN`]).
///
/// `tool_name` is the `domain:mode` key the CLI dispatched (e.g. `code:outline`) and enables a few
/// nicer special-cases.
pub fn render_human(tool_name: &str, value: &Value, out: &mut impl Write) -> Result<()> {
    render_human_counting(tool_name, value, out, &mut Truncated::default())
}

fn render_human_counting(tool_name: &str, value: &Value, out: &mut impl Write, cut: &mut Truncated) -> Result<()> {
    let flat;
    let value = match ungrouped(value) {
        Some(rows) => {
            flat = rows;
            &flat
        }
        None => value,
    };
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
                    // A row is a string (`"<path>:<line>: <signature>"`, `"<sha> <summary>"`): one per
                    // line, since a row's own text holds the `, ` a joined list would separate them by.
                    Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_string) => {
                        writeln!(out, "{key}:")?;
                        write_tree(v, 1, out)?;
                    }
                    Value::Array(items) if !items.is_empty() => {
                        let joined: Vec<String> = items.iter().map(scalar_to_string).collect();
                        writeln!(out, "{key}: {}", joined.join(", "))?;
                    }
                    Value::Array(_) => writeln!(out, "{key}: (empty)")?,
                    // `git_blame` `commits`: each sha's author, time and summary.
                    Value::Object(entries) if key == "commits" && is_record_map(entries) => {
                        writeln!(out, "{key}:")?;
                        render_records(entries, out, cut)?;
                    }
                    Value::Object(entries) if !entries.is_empty() => {
                        writeln!(out, "{key}:")?;
                        write_tree(v, 1, out)?;
                    }
                    Value::String(s) if s.contains('\n') => {
                        writeln!(out, "{key}:")?;
                        write_raw(out, s)?;
                    }
                    _ => writeln!(out, "{key}: {}", scalar_to_string(v))?,
                }
            }

            for (key, items) in &object_arrays {
                writeln!(out, "\n{key} ({} items):", items.len())?;
                render_table(tool_name, items, out, cut)?;
            }

            render_grep_truncation(tool_name, map, out)?;
        }
        Value::Array(items) if items.first().is_some_and(|i| i.is_object()) => {
            render_table(tool_name, items, out, cut)?;
        }
        Value::Array(items) => {
            for item in items {
                writeln!(out, "{}", scalar_to_string(item))?;
            }
        }
        Value::String(s) => write_raw(out, s)?,
        other => writeln!(out, "{}", scalar_to_string(other))?,
    }
    Ok(())
}

/// `value` with every list grouped by file (`{"<path>": [row, ...]}`, as `code_grep` and
/// `code_refs` list their hits) turned back into one row per hit carrying its `path`, so it renders
/// as a table; `None` when there is no such list.
fn ungrouped(value: &Value) -> Option<Value> {
    let map = value.as_object()?;
    let grouped = |v: &Value| {
        v.as_object().is_some_and(|files| {
            !files.is_empty()
                && files
                    .values()
                    .all(|rows| rows.as_array().is_some_and(|rows| rows.iter().all(Value::is_object)))
        })
    };
    if !map.values().any(grouped) {
        return None;
    }
    let mut out = serde_json::Map::new();
    for (key, v) in map {
        let Some(files) = v.as_object().filter(|_| grouped(v)) else {
            out.insert(key.clone(), v.clone());
            continue;
        };
        let mut rows = Vec::new();
        for (path, hits) in files {
            for hit in hits.as_array().into_iter().flatten() {
                let mut row = serde_json::Map::new();
                row.insert("path".to_string(), Value::String(path.clone()));
                row.extend(
                    hit.as_object()
                        .into_iter()
                        .flatten()
                        .map(|(k, v)| (k.clone(), v.clone())),
                );
                rows.push(Value::Object(row));
            }
        }
        out.insert(key.clone(), Value::Array(rows));
    }
    Some(Value::Object(out))
}

/// Write a nested answer one entry per line, indented two spaces a level: a list of rows one row a
/// line; a map of scalars as aligned `key  value` lines (`find_files --min-lines`, a commit's files);
/// any other map as each key with its value indented under it (`code_grep` hits by file and symbol,
/// `git_log` files by commit, importers by depth). The `""` key a grep row outside every symbol is
/// grouped under is shown as `(outside any symbol)`.
fn write_tree(value: &Value, depth: usize, out: &mut impl Write) -> Result<()> {
    let pad = "  ".repeat(depth);
    match value {
        Value::Array(items) => {
            for item in items.iter().take(MAX_HUMAN_ITEMS) {
                match item {
                    Value::Array(_) | Value::Object(_) => write_tree(item, depth, out)?,
                    _ => writeln!(out, "{pad}{}", scalar_to_string(item).replace('\n', " "))?,
                }
            }
            if items.len() > MAX_HUMAN_ITEMS {
                writeln!(out, "{pad}… and {} more", items.len() - MAX_HUMAN_ITEMS)?;
            }
        }
        Value::Object(map) if map.values().all(|v| !v.is_array() && !v.is_object()) => {
            let width = map.keys().map(|k| k.chars().count()).max().unwrap_or(0);
            for (key, v) in map {
                writeln!(out, "{pad}{key:<width$}  {}", scalar_to_string(v))?;
            }
        }
        Value::Object(map) => {
            for (key, v) in map {
                let label = if key.is_empty() { "(outside any symbol)" } else { key };
                match v {
                    Value::Array(_) | Value::Object(_) => {
                        writeln!(out, "{pad}{label}")?;
                        write_tree(v, depth + 1, out)?;
                    }
                    _ => writeln!(out, "{pad}{label}: {}", scalar_to_string(v))?,
                }
            }
        }
        other => writeln!(out, "{pad}{}", scalar_to_string(other))?,
    }
    Ok(())
}

/// A map of records as a table, the map key first.
fn render_records(map: &serde_json::Map<String, Value>, out: &mut impl Write, cut: &mut Truncated) -> Result<()> {
    let rows: Vec<Value> = map
        .iter()
        .map(|(key, record)| {
            let mut row = serde_json::Map::new();
            row.insert(String::new(), Value::String(key.clone()));
            row.extend(
                record
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
            Value::Object(row)
        })
        .collect();
    render_table("", &rows, out, cut)
}

/// A non-empty map whose values are objects of scalars all with the same fields: rows of a table.
fn is_record_map(map: &serde_json::Map<String, Value>) -> bool {
    let mut records = map.values().map(Value::as_object);
    let Some(Some(first)) = records.next() else {
        return false;
    };
    let scalars = |record: &serde_json::Map<String, Value>| record.values().all(|v| !v.is_array() && !v.is_object());
    !first.is_empty()
        && scalars(first)
        && records.all(|record| {
            record.is_some_and(|record| {
                scalars(record) && record.len() == first.len() && record.keys().all(|k| first.contains_key(k))
            })
        })
}

/// Write `text` verbatim, ensuring it ends with exactly one newline.
fn write_raw(out: &mut impl Write, text: &str) -> Result<()> {
    out.write_all(text.as_bytes())?;
    if !text.ends_with('\n') {
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// Warn, in prose, when a grep result is not the whole truth.
///
/// A `next_cursor` or `truncated: true` row in the generic key/value dump is a signal nobody
/// reads — and for grep, a partial result is indistinguishable from a complete one at a glance,
/// which is how a cut grep gets mistaken for "no such symbol in the repo". So a page that is not
/// the whole answer gets its own line, naming the count that was withheld and the way to get it.
fn render_grep_truncation(tool_name: &str, map: &serde_json::Map<String, Value>, out: &mut impl Write) -> Result<()> {
    if tool_name != "code_grep" {
        return Ok(());
    }
    let shown = map.get("hits").map_or(0, grep_matches);
    let total = map.get("total").and_then(Value::as_u64).unwrap_or(0);
    if map.get("truncated").and_then(Value::as_bool) == Some(true) {
        writeln!(
            out,
            "\nwarning: TRUNCATED — the corpus exceeds what one grep may read, so files were left \
             unscanned. Narrow with --path / --language."
        )?;
    } else if map.get("next_cursor").is_some_and(|cursor| !cursor.is_null()) {
        writeln!(
            out,
            "\nwarning: MORE MATCHES — showing {shown} of {total}. Raise --limit, or narrow with --path / \
             --language."
        )?;
    }
    Ok(())
}

/// The matches a `code_grep` `hits` map lists: a row `"<line>: <text>"` is one, `"<line>:<col>,<col>:
/// <text>"` one per column, a context row `"<line>- <text>"` none.
fn grep_matches(hits: &Value) -> u64 {
    match hits {
        Value::Object(groups) => groups.values().map(grep_matches).sum(),
        Value::Array(rows) => rows.iter().map(grep_matches).sum(),
        Value::String(row) => {
            let head = row.split_once(' ').map_or(row.as_str(), |(head, _)| head);
            match head.strip_suffix(':') {
                Some(position) => position
                    .split_once(':')
                    .map_or(1, |(_, columns)| columns.split(',').count() as u64),
                None => 0,
            }
        }
        _ => 0,
    }
}

/// Render an array of objects as an aligned table. Columns are the union of keys
/// of the first item (stable order), with nested arrays/objects collapsed.
fn render_table(tool_name: &str, items: &[Value], out: &mut impl Write, cut: &mut Truncated) -> Result<()> {
    if items.is_empty() {
        writeln!(out, "  (none)")?;
        return Ok(());
    }

    if let Some(rendered) = render_special(tool_name, items, out, cut)? {
        return Ok(rendered);
    }

    let Some(first) = items.first().and_then(Value::as_object) else {
        for item in items {
            writeln!(out, "  {}", cell(item, cut))?;
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
                .map(|col| item.get(*col).map(|v| cell(v, cut)).unwrap_or_default())
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
    writeln!(out, "  {}", header.join("  ").trim_end())?;
    for row in &rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:<width$}", c, width = widths[i]))
            .collect();
        writeln!(out, "  {}", cells.join("  ").trim_end())?;
    }
    if items.len() > display {
        writeln!(out, "  … and {} more", items.len() - display)?;
    }
    Ok(())
}

/// Special-cased compact renderers for high-traffic tools. Returns `Some(())`
/// when it handled the items, `None` to fall through to the generic table.
fn render_special(tool_name: &str, items: &[Value], out: &mut impl Write, cut: &mut Truncated) -> Result<Option<()>> {
    match tool_name {
        "code_outline" => {
            for item in items.iter().take(MAX_HUMAN_ITEMS) {
                let Some(obj) = item.as_object() else {
                    return Ok(None);
                };
                let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
                let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("");
                let row = obj
                    .get("line")
                    .and_then(Value::as_u64)
                    .or_else(|| obj.get("start_row").and_then(Value::as_u64).map(|r| r + 1))
                    .unwrap_or(0);
                let path = obj.get("path").and_then(Value::as_str);
                let sig = obj.get("signature").and_then(Value::as_str).unwrap_or("");
                match path {
                    Some(p) => writeln!(
                        out,
                        "  {p}:{row} {kind:<10} {name} {sig}",
                        sig = clip(&sig.replace('\n', " "), cut)
                    )?,
                    None => writeln!(
                        out,
                        "  {row:>5} {kind:<10} {name} {sig}",
                        sig = clip(&sig.replace('\n', " "), cut)
                    )?,
                }
            }
            if items.len() > MAX_HUMAN_ITEMS {
                writeln!(out, "  … and {} more", items.len() - MAX_HUMAN_ITEMS)?;
            }
            Ok(Some(()))
        }
        "git_diff" => {
            // Hunk bodies are multi-line diff text: print them raw under a unified-diff style
            // header instead of flattening them into a table cell.
            for item in items {
                let Some(obj) = item.as_object() else {
                    return Ok(None);
                };
                let num = |k: &str| obj.get(k).and_then(Value::as_u64).unwrap_or(0);
                let text = obj.get("text").and_then(Value::as_str).unwrap_or("");
                writeln!(
                    out,
                    "  @@ -{},{} +{},{} @@",
                    num("old_line_start"),
                    num("old_line_count"),
                    num("new_line_start"),
                    num("new_line_count")
                )?;
                write_raw(out, text)?;
            }
            Ok(Some(()))
        }
        "code_refs" => {
            for item in items.iter().take(MAX_HUMAN_ITEMS) {
                let Some(obj) = item.as_object() else {
                    return Ok(None);
                };
                let path = obj.get("path").and_then(Value::as_str).unwrap_or("");
                let line = obj.get("line").and_then(Value::as_u64).unwrap_or(0);
                let what = ["callee", "impl_type", "trait_name"]
                    .iter()
                    .filter_map(|key| obj.get(*key).and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" ");
                writeln!(out, "  {path}:{line} {what}")?;
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

    fn render_grep(value: &serde_json::Value) -> String {
        let mut buf: Vec<u8> = Vec::new();
        render_human("code_grep", value, &mut buf).expect("render");
        String::from_utf8(buf).expect("utf8")
    }

    #[test]
    fn warns_in_prose_when_a_grep_page_is_not_every_match() {
        let out = render_grep(&json!({
            "total": 101,
            "next_cursor": "b3BhcXVl",
            "hits": {"a.rs": {"run": ["1: OptimizationStatus", "2- context", "3:5,9: a, OptimizationStatus"]}},
        }));
        assert!(
            out.contains("MORE MATCHES — showing 3 of 101"),
            "must count the matches listed, not the files or context rows: {out}"
        );
        assert!(
            out.contains("a.rs") && out.contains("OptimizationStatus"),
            "a hit listed under its file renders as a row with its path: {out}"
        );
        assert!(!out.contains("TRUNCATED"), "a `limit` cut is paging: {out}");

        let unread = render_grep(&json!({
            "total": 1,
            "truncated": true,
            "truncated_reason": "byte_budget",
            "next_cursor": "b3BhcXVl",
            "hits": {"a.rs": {"": ["1: OptimizationStatus"]}},
        }));
        assert!(unread.contains("TRUNCATED"), "unread files must be shouted: {unread}");
    }

    #[test]
    fn a_complete_grep_result_carries_no_warning() {
        let out = render_grep(&json!({
            "hits": {"a.rs": {"": ["1: OptimizationStatus"]}},
        }));
        assert!(
            !out.contains("TRUNCATED") && !out.contains("MORE MATCHES"),
            "a complete result must not cry wolf: {out}"
        );
    }

    fn emit_capture(tool: &str, result: &rmcp::model::CallToolResult, json: bool) -> (String, String) {
        let opts = super::Emit {
            json,
            startup_us: 1_500,
            called: std::time::Instant::now(),
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        super::emit_to(tool, result, &opts, &mut out, &mut err).expect("emit");
        (
            String::from_utf8(out).expect("utf8"),
            String::from_utf8(err).expect("utf8"),
        )
    }

    #[test]
    fn never_shortens_a_key_value_string() {
        let body = format!("fn main() {{\n    let s = \"{}\";\n}}\n", "z".repeat(500));
        let out = render(&json!({"path": "a.rs", "body": body, "note": "n".repeat(500)}));
        assert!(out.contains(&"z".repeat(500)), "multi-line body was cut: {out}");
        assert!(
            out.contains("body:\nfn main() {\n"),
            "body must print raw after its key: {out}"
        );
        assert!(out.contains(&"n".repeat(500)), "single-line value was cut: {out}");
        assert!(!out.contains('…'), "no ellipsis expected: {out}");
    }

    #[test]
    fn cuts_only_table_cells_and_counts_the_cuts() {
        let value = json!({"rows": [{"name": "a", "text": "t".repeat(500)}]});
        let mut buf = Vec::new();
        let mut cut = super::Truncated::default();
        super::render_human_counting("diff_outline", &value, &mut buf, &mut cut).expect("render");
        let out = String::from_utf8(buf).expect("utf8");
        assert_eq!(cut.cells, 1);
        assert!(out.contains(&format!("{}…", "t".repeat(200))), "{out}");
    }

    #[test]
    fn git_diff_hunks_print_raw_under_a_unified_header() {
        let mut buf = Vec::new();
        let value = json!({"hunks": [{
            "old_line_start": 2, "old_line_count": 1,
            "new_line_start": 2, "new_line_count": 1,
            "text": format!("-old\n+{}\n", "x".repeat(400)),
        }]});
        render_human("git_diff", &value, &mut buf).expect("render");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(out.contains("@@ -2,1 +2,1 @@\n-old\n+"), "{out}");
        assert!(out.contains(&"x".repeat(400)), "{out}");
    }

    #[test]
    fn timing_footer_goes_to_stderr_not_stdout() {
        let result = rmcp::model::CallToolResult::structured(json!({"total": 1, "elapsed_us": 250}));
        let (out, err) = emit_capture("find_files", &result, false);
        assert!(
            !out.contains("startup") && !out.contains("query"),
            "stdout must be clean: {out}"
        );
        assert!(err.contains("250 µs query") && err.contains("1.5 ms startup"), "{err}");
    }

    #[test]
    fn a_response_without_its_own_timing_is_timed_by_the_cli() {
        let result = rmcp::model::CallToolResult::structured(json!({"total": 1, "files": ["a.rs", "b/c.md"]}));
        let (out, err) = emit_capture("find_files", &result, false);
        assert!(out.contains("files:\n  a.rs\n  b/c.md\n"), "one path per line: {out}");
        assert!(err.contains("query") && err.contains("1.5 ms startup"), "{err}");
    }

    #[test]
    fn json_text_only_results_still_parse() {
        let result =
            rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(r#"{"total": 2}"#.to_string())]);
        let (out, _) = emit_capture("find_files", &result, false);
        assert!(out.contains("total: 2"), "{out}");
    }

    fn render_as(tool: &str, value: &serde_json::Value) -> String {
        let mut buf: Vec<u8> = Vec::new();
        render_human(tool, value, &mut buf).expect("render");
        String::from_utf8(buf).expect("utf8")
    }

    #[test]
    fn symbol_rows_print_one_per_line() {
        let out = render_as(
            "code_symbols",
            &json!({"results": ["src/a.rs:3: fn alpha(a: u8, b: u8)", "src/b.rs:9: struct Alpha"]}),
        );
        assert_eq!(
            out, "results:\n  src/a.rs:3: fn alpha(a: u8, b: u8)\n  src/b.rs:9: struct Alpha\n",
            "{out}"
        );
    }

    #[test]
    fn grep_hits_print_under_their_file_and_symbol() {
        let out = render_grep(&json!({
            "hits": {
                "src/a.rs": {"alpha": ["3: let x = 1;", "4- y"], "": ["9: x"]},
                "src/b.rs": {"beta": ["1:2,7: x, x"]},
            },
        }));
        assert_eq!(
            out,
            "hits:\n  src/a.rs\n    alpha\n      3: let x = 1;\n      4- y\n    (outside any symbol)\n      9: x\n  \
             src/b.rs\n    beta\n      1:2,7: x, x\n",
            "{out}"
        );
    }

    #[test]
    fn a_recent_log_prints_a_commit_a_line_and_its_files_under_its_sha() {
        let out = render_as(
            "git_log",
            &json!({
                "commits": ["c48e32cbf7dd add gamma, delta", "0c2fe19c38a2 tune alpha"],
                "files": {"c48e32cbf7dd": {"src/gamma.rs": "added"}, "0c2fe19c38a2": {"src/lib.rs": "modified"}},
            }),
        );
        assert_eq!(
            out,
            "commits:\n  c48e32cbf7dd add gamma, delta\n  0c2fe19c38a2 tune alpha\nfiles:\n  c48e32cbf7dd\n    \
             src/gamma.rs  added\n  0c2fe19c38a2\n    src/lib.rs  modified\n",
            "{out}"
        );
    }

    #[test]
    fn blame_prints_a_hunk_a_line_and_its_commits_as_a_table() {
        let out = render_as(
            "git_blame",
            &json!({
                "hunks": ["1-3 aaaaaaaaaaaa", "4 bbbbbbbbbbbb old.rs"],
                "commits": {
                    "aaaaaaaaaaaa": {"author": "ann", "time": 1, "summary": "add it"},
                    "bbbbbbbbbbbb": {"author": "bob", "time": 2, "summary": "move it"},
                },
            }),
        );
        assert_eq!(
            out,
            "hunks:\n  1-3 aaaaaaaaaaaa\n  4 bbbbbbbbbbbb old.rs\ncommits:\n                author  time  summary\n  \
             aaaaaaaaaaaa  ann     1     add it\n  bbbbbbbbbbbb  bob     2     move it\n",
            "{out}"
        );
    }

    #[test]
    fn a_path_to_count_map_prints_aligned_and_importers_print_by_depth() {
        let out = render_as("find_files", &json!({"files": {"src/big.rs": 4000, "a.rs": 900}}));
        assert_eq!(out, "files:\n  src/big.rs  4000\n  a.rs        900\n", "{out}");
        let out = render_as(
            "code_refs",
            &json!({"by_depth": {"1": ["b.rs", "c.rs"]}, "deeper": true}),
        );
        assert_eq!(out, "by_depth:\n  1\n    b.rs\n    c.rs\ndeeper: true\n", "{out}");
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
