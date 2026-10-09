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
}

/// Wire encoding of a tool response, for the `--format` flag on commands whose MCP tool takes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum WireFormat {
    /// Structured JSON (the default).
    Json,
    /// Compact tabular TOON, rendered as-is in human mode; `--json` still prints JSON.
    Toon,
}

/// The `format` string the MCP params carry for an optional `--format` choice.
pub fn wire(format: Option<WireFormat>) -> Option<String> {
    format.map(|f| match f {
        WireFormat::Json => "json".to_string(),
        WireFormat::Toon => "toon".to_string(),
    })
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
/// A tool asked for `format: "toon"` answers with TOON text (and the same data as structured
/// content). Human mode renders that response as TOON; `--json` always wins and prints the JSON.
pub fn emit_to(
    tool_name: &str,
    result: &CallToolResult,
    opts: &Emit,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<()> {
    let (mut value, toon) = payload(result)?;
    if opts.json {
        if let Value::Object(map) = &mut value {
            map.insert("startup_us".to_string(), Value::from(opts.startup_us));
        }
        return render_json(&value, out);
    }

    let elapsed_us = match &mut value {
        Value::Object(map) => map.remove("elapsed_us").and_then(|v| v.as_u64()),
        _ => None,
    };
    let mut cut = Truncated::default();
    if toon {
        writeln!(out, "{}", crate::mcp::toon::encode(&value))?;
    } else {
        render_human_counting(tool_name, &value, out, &mut cut)?;
    }
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

/// The tool's data plus whether the tool answered in TOON.
///
/// Structured content is authoritative (every tool sets it); the text block is the fallback for a
/// tool that only mirrors JSON as text. A text block that is not JSON next to structured content is
/// the TOON mirror.
fn payload(result: &CallToolResult) -> Result<(Value, bool)> {
    let text = result.content.iter().find_map(|c| match c {
        ContentBlock::Text(t) => Some(t.text.as_str()),
        _ => None,
    });
    if let Some(structured) = &result.structured_content {
        let toon = text.is_some_and(|t| serde_json::from_str::<Value>(t).is_err());
        return Ok((structured.clone(), toon));
    }
    let text = text.context("tool returned no text content")?;
    let value = serde_json::from_str(text).with_context(|| "parse tool JSON response")?;
    Ok((value, false))
}

/// Extract the JSON payload from a tool result: the structured content when present, otherwise the
/// first text block parsed as JSON.
pub fn result_to_value(result: &CallToolResult) -> Result<Value> {
    payload(result).map(|(value, _)| value)
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
/// A bare `truncated: true` row in the generic key/value dump is a signal nobody reads — and for
/// grep, a partial result is indistinguishable from a complete one at a glance, which is how a
/// truncated grep gets mistaken for "no such symbol in the repo". So the bound gets its own line,
/// naming the count that was withheld and the way to get it.
fn render_grep_truncation(tool_name: &str, map: &serde_json::Map<String, Value>, out: &mut impl Write) -> Result<()> {
    if tool_name != "code:grep" || map.get("truncated").and_then(Value::as_bool) != Some(true) {
        return Ok(());
    }
    let shown = map.get("hits").and_then(Value::as_array).map_or(0, Vec::len);
    let total = map.get("total_matches").and_then(Value::as_u64).unwrap_or(0);
    match map.get("truncation_reason").and_then(Value::as_str) {
        Some("byte_budget") => writeln!(
            out,
            "\nwarning: TRUNCATED — the corpus exceeds what one grep may read, so files were left \
             unscanned. Narrow with --path-contains / --language."
        )?,
        _ => writeln!(
            out,
            "\nwarning: TRUNCATED — showing {shown} of {total} matches. Raise --limit, or narrow \
             with --path-contains / --language."
        )?,
    }
    Ok(())
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
fn render_special(tool_name: &str, items: &[Value], out: &mut impl Write, cut: &mut Truncated) -> Result<Option<()>> {
    match tool_name {
        "code:outline" | "code:symbols" => {
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
        "git:diff" => {
            // Hunk bodies are multi-line diff text: print them raw under a unified-diff style
            // header instead of flattening them into a table cell.
            for item in items {
                let Some(obj) = item.as_object() else {
                    return Ok(None);
                };
                let num = |k: &str| obj.get(k).and_then(Value::as_u64).unwrap_or(0);
                let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("");
                let text = obj.get("text").and_then(Value::as_str).unwrap_or("");
                writeln!(
                    out,
                    "  @@ -{},{} +{},{} @@ {kind}",
                    num("old_line_start"),
                    num("old_line_count"),
                    num("new_line_start"),
                    num("new_line_count")
                )?;
                write_raw(out, text)?;
            }
            Ok(Some(()))
        }
        "code:references" | "code:callers" => {
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

    fn render_grep(value: &serde_json::Value) -> String {
        let mut buf: Vec<u8> = Vec::new();
        render_human("code:grep", value, &mut buf).expect("render");
        String::from_utf8(buf).expect("utf8")
    }

    #[test]
    fn warns_in_prose_when_a_grep_result_is_truncated_by_the_limit() {
        let out = render_grep(&json!({
            "pattern": "OptimizationStatus",
            "total_matches": 101,
            "truncated": true,
            "truncation_reason": "limit",
            "hits": [{"path": "a.rs", "line_num": 1, "column": 0, "matched_text": "OptimizationStatus"}],
        }));
        assert!(
            out.contains("TRUNCATED"),
            "truncation must be shouted, not buried: {out}"
        );
        assert!(
            out.contains("showing 1 of 101 matches"),
            "must name the withheld count: {out}"
        );
    }

    #[test]
    fn a_complete_grep_result_carries_no_warning() {
        let out = render_grep(&json!({
            "pattern": "OptimizationStatus",
            "total_matches": 1,
            "truncated": false,
            "hits": [{"path": "a.rs", "line_num": 1, "column": 0, "matched_text": "OptimizationStatus"}],
        }));
        assert!(!out.contains("TRUNCATED"), "a complete result must not cry wolf: {out}");
    }

    fn emit_capture(tool: &str, result: &rmcp::model::CallToolResult, json: bool) -> (String, String) {
        let opts = super::Emit {
            json,
            startup_us: 1_500,
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
            "kind": "modified", "old_line_start": 2, "old_line_count": 1,
            "new_line_start": 2, "new_line_count": 1,
            "text": format!("-old\n+{}\n", "x".repeat(400)),
        }]});
        render_human("git:diff", &value, &mut buf).expect("render");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(out.contains("@@ -2,1 +2,1 @@ modified\n-old\n+"), "{out}");
        assert!(out.contains(&"x".repeat(400)), "{out}");
    }

    #[test]
    fn timing_footer_goes_to_stderr_not_stdout() {
        let result = rmcp::model::CallToolResult::structured(json!({"total": 1, "elapsed_us": 250}));
        let (out, err) = emit_capture("code:files", &result, false);
        assert!(
            !out.contains("startup") && !out.contains("query"),
            "stdout must be clean: {out}"
        );
        assert!(err.contains("250 µs query") && err.contains("1.5 ms startup"), "{err}");
    }

    #[test]
    fn toon_responses_render_end_to_end() {
        let data = json!({"total": 1, "results": [{"path": "a.rs", "name": "alpha"}], "elapsed_us": 9});
        let mut result = rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
            crate::mcp::toon::encode(&data),
        )]);
        result.structured_content = Some(data);
        let (out, err) = emit_capture("code:symbols", &result, false);
        assert!(
            out.contains("results[1]{name,path}:"),
            "human mode renders the TOON table: {out}"
        );
        assert!(
            !out.contains("elapsed_us"),
            "timing is lifted out of the payload: {out}"
        );
        assert!(err.contains("query"), "{err}");
        let (json_out, _) = emit_capture("code:symbols", &result, true);
        let parsed: serde_json::Value = serde_json::from_str(&json_out).expect("--json stays JSON with toon");
        assert_eq!(parsed["results"][0]["name"], "alpha");
        assert_eq!(parsed["startup_us"], 1_500);
    }

    #[test]
    fn json_text_only_results_still_parse() {
        let result =
            rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(r#"{"total": 2}"#.to_string())]);
        let (out, _) = emit_capture("code:files", &result, false);
        assert!(out.contains("total: 2"), "{out}");
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
