//! The text a verb tool answers with (ADR-0053).
//!
//! A tool's answer is built as JSON (what the CLI's `--json` prints) and sent once over MCP, as the
//! text block a model reads. The text block is encoded per tool, by what measured fewest tokens on the eval corpora:
//!
//! - `code_grep` rows: the rest of the answer as [TOON](https://toonformat.dev) lines, then the
//!   rows under `# <path>` and `## <symbol>` headers, as `rg --heading` prints them. TOON alone
//!   quotes nearly every line of code (a `:` forces it) and escapes its quotes.
//! - `code_outline`, `git_diff` and a `find_files` list: TOON, whose tables drop the per-row keys.
//! - Every other answer, and a `code_grep` answer the row block cannot carry: compact JSON, which
//!   TOON did not shorten.
//!
//! [`decode`] reads any of them back to the JSON the tool built.

use serde_json::{Map, Value};

/// The key `code_grep` lists its rows under.
const HITS: &str = "hits";

/// The prefix of a file header in the `code_grep` block.
const FILE_HEADER: &str = "# ";

/// A symbol header in the `code_grep` block; bare, it names the rows outside every symbol.
const SYMBOL_HEADER: &str = "##";

/// The delimiter of TOON's inline arrays and table rows.
const TOON_DELIMITER: char = ',';

/// The text of `tool`'s answer `value`.
pub fn encode(tool: &str, value: &Value) -> String {
    match tool {
        "code_grep" => encode_grep(value).unwrap_or_else(|| value.to_string()),
        "code_outline" | "git_diff" => toon(value),
        "find_files" if value.get("files").is_some_and(Value::is_array) => toon(value),
        _ => value.to_string(),
    }
}

/// `value` as TOON. Never fails for a JSON value; an encoder error falls back to compact JSON, which
/// [`decode`] also reads. A debug build panics on that error instead, so a test sees it.
pub fn toon(value: &Value) -> String {
    let encoded = toon_format::encode_default(value);
    debug_assert!(
        encoded.is_ok(),
        "TOON could not encode a JSON value ({:?}): {value}",
        encoded.as_ref().err()
    );
    encoded.unwrap_or_else(|_| value.to_string())
}

/// The JSON a tool's text answer encodes: compact JSON, TOON, or TOON followed by a `code_grep` row
/// block.
///
/// # Errors
///
/// The text is none of these, or its row block is malformed; the message names the line.
pub fn decode(text: &str) -> Result<Value, String> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{')
        && let Ok(value) = serde_json::from_str(trimmed)
    {
        return Ok(value);
    }
    let block = text
        .split('\n')
        .scan(0usize, |offset, line| {
            let at = *offset;
            *offset += line.len() + 1;
            Some((at, line))
        })
        .find(|(_, line)| line.starts_with(FILE_HEADER))
        .map(|(at, _)| at);
    let (head, rows) = match block {
        Some(at) => (text[..at].trim_end_matches('\n'), Some(&text[at..])),
        None => (text, None),
    };
    let mut value = if head.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        toon_format::decode_default::<Value>(head).map_err(|e| format!("not TOON: {e}"))?
    };
    if let Some(rows) = rows {
        let Value::Object(map) = &mut value else {
            return Err("a row block follows a TOON value that is not an object".into());
        };
        if map.contains_key(HITS) {
            return Err("`hits` is both in the TOON lines and in the row block".into());
        }
        map.insert(HITS.into(), decode_grep_rows(rows)?);
    }
    Ok(value)
}

/// `code_grep`'s answer as TOON lines for everything but `hits`, then the rows under file and symbol
/// headers; an empty first group gets its bare `##` header, or it would read back as absent. `None`
/// when `hits` lists nothing or holds a row the block cannot carry: one not starting with its line
/// number, or holding a line break or carriage return.
fn encode_grep(value: &Value) -> Option<String> {
    let map = value.as_object()?;
    let hits = map.get(HITS)?.as_object()?;
    if hits.is_empty() {
        return None;
    }
    let mut block = String::new();
    for (path, groups) in hits {
        block.push_str(FILE_HEADER);
        block.push_str(&header(path));
        block.push('\n');
        for (i, (symbol, rows)) in groups.as_object()?.iter().enumerate() {
            let rows = rows.as_array()?;
            if i > 0 || !symbol.is_empty() || rows.is_empty() {
                block.push_str(SYMBOL_HEADER);
                if !symbol.is_empty() {
                    block.push(' ');
                    block.push_str(&header(symbol));
                }
                block.push('\n');
            }
            for row in rows {
                let row = row.as_str()?;
                if !row.starts_with(|c: char| c.is_ascii_digit()) || row.contains(['\n', '\r']) {
                    return None;
                }
                block.push_str(row);
                block.push('\n');
            }
        }
    }
    block.pop();
    let rest: Map<String, Value> = map
        .iter()
        .filter(|(key, _)| *key != HITS)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if rest.is_empty() {
        return Some(block);
    }
    Some(format!("{}\n{block}", toon(&Value::Object(rest))))
}

/// A file or symbol name as its header writes it: bare, or JSON-quoted when it is empty, starts with
/// `"`, has leading or trailing whitespace, or holds a control character.
pub(crate) fn header(name: &str) -> String {
    let quoted = name.is_empty()
        || name.starts_with('"')
        || name.starts_with(char::is_whitespace)
        || name.ends_with(char::is_whitespace)
        || name.chars().any(char::is_control);
    if quoted {
        serde_json::to_string(name).unwrap_or_default()
    } else {
        name.to_string()
    }
}

fn unheader(raw: &str) -> Result<String, String> {
    if raw.starts_with('"') {
        serde_json::from_str(raw).map_err(|e| format!("bad quoted header {raw:?}: {e}"))
    } else {
        Ok(raw.to_string())
    }
}

/// The `hits` map of a `code_grep` row block.
fn decode_grep_rows(block: &str) -> Result<Value, String> {
    let mut files: Map<String, Value> = Map::new();
    let mut file: Option<(String, Map<String, Value>)> = None;
    let mut group = String::new();
    for line in block.split('\n') {
        if let Some(path) = line.strip_prefix(FILE_HEADER) {
            if let Some((path, groups)) = file.take() {
                files.insert(path, Value::Object(groups));
            }
            file = Some((unheader(path)?, Map::new()));
            group.clear();
            continue;
        }
        let Some((_, groups)) = file.as_mut() else {
            return Err(format!("row before any file header: {line:?}"));
        };
        if line == SYMBOL_HEADER {
            group.clear();
            groups.entry(String::new()).or_insert_with(|| Value::Array(Vec::new()));
        } else if let Some(symbol) = line.strip_prefix("## ") {
            group = unheader(symbol)?;
            groups.entry(group.clone()).or_insert_with(|| Value::Array(Vec::new()));
        } else if line.starts_with(|c: char| c.is_ascii_digit()) {
            match groups.entry(group.clone()).or_insert_with(|| Value::Array(Vec::new())) {
                Value::Array(rows) => rows.push(Value::String(line.to_string())),
                _ => return Err(format!("group {group:?} is not a row list")),
            }
        } else {
            return Err(format!("not a header or a numbered row: {line:?}"));
        }
    }
    if let Some((path, groups)) = file.take() {
        files.insert(path, Value::Object(groups));
    }
    Ok(Value::Object(files))
}

/// Bytes a string costs as a TOON cell: bare, or quoted and escaped where TOON requires it.
pub(crate) fn toon_cell_bytes(s: &str) -> u64 {
    if toon_format::needs_quoting(s, TOON_DELIMITER) {
        toon_format::escape_string(s).len() as u64 + 2
    } else {
        s.len() as u64
    }
}

/// Bytes one element of a TOON array costs as written: a scalar is a cell and its delimiter, a flat
/// object a table row (its cells, their delimiters, the row's indent and line break), anything else
/// its own TOON lines under a list marker.
pub(crate) fn toon_element_bytes(value: &Value) -> u64 {
    match value {
        Value::Object(map) if map.values().all(|v| !v.is_object() && !v.is_array()) => {
            let cells: u64 = map.values().map(scalar_cell_bytes).sum();
            cells + map.len().saturating_sub(1) as u64 + 3
        }
        Value::Object(_) | Value::Array(_) => toon(value).len() as u64 + 5,
        scalar => scalar_cell_bytes(scalar) + 1,
    }
}

fn scalar_cell_bytes(value: &Value) -> u64 {
    match value {
        Value::String(s) => toon_cell_bytes(s),
        other => other.to_string().len() as u64,
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
