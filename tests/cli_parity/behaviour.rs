//! Layer 3: behavioural parity. The same call is issued through the in-process MCP server and through
//! the spawned CLI (`--json`) against one scanned fixture repo; the payloads must be equal once
//! run-to-run noise (timings, lifecycle notices) is removed.
//!
//! A tool whose retrieval stack is not compiled in (`code_search`, the memory tools) refuses on both
//! sides; such a case passes only when the CLI prints the very message the MCP tool returned.

use std::process::Command;

use serde_json::{Value, json};

use crate::capabilities::Row;

/// A query: the MCP tool call, and the CLI command (a `pair` or `shorthand` of that tool in the
/// capability table) with the operands that follow it.
pub struct Case {
    pub tool: &'static str,
    pub mcp: Value,
    pub cli: &'static str,
    pub cli_operands: &'static [&'static str],
    /// Fields of this tool's payload that differ between two calls by design (a write timestamp,
    /// the answering process).
    pub volatile: &'static [&'static str],
}

const fn case(tool: &'static str, mcp: Value, cli: &'static str, cli_operands: &'static [&'static str]) -> Case {
    Case {
        tool,
        mcp,
        cli,
        cli_operands,
        volatile: &[],
    }
}

impl Case {
    const fn ignoring(mut self, volatile: &'static [&'static str]) -> Self {
        self.volatile = volatile;
        self
    }
}

/// Fields of `index_status` that describe the process answering, not the index: how long its map
/// took to warm, and the root as that process spelled it (`/var/…` against `/private/var/…`).
const STATUS_VOLATILE: &[&str] = &["warm_ms", "root"];

/// Whether a commit search was answered by the live walk: the server has the git-history index
/// warm, a one-shot CLI call may not.
const SEARCH_VOLATILE: &[&str] = &["partial"];

/// Write timestamps of a memory note: the two transports write it one after the other.
const NOTE_VOLATILE: &[&str] = &["created_at", "updated_at"];

pub fn cases() -> Vec<Case> {
    vec![
        case(
            "code_outline",
            json!({"path": "src/alpha.rs"}),
            "code outline",
            &["src/alpha.rs"],
        ),
        case("code_symbols", json!({"name": "alpha"}), "code symbols", &["alpha"]),
        case(
            "code_read",
            json!({"path": "src/alpha.rs", "symbol": "alpha"}),
            "code read",
            &["src/alpha.rs", "--symbol", "alpha"],
        ),
        case(
            "code_read",
            json!({"path": "src/gamma.rs", "line": 3, "end_line": 5}),
            "code read",
            &["src/gamma.rs", "--line", "3", "--end-line", "5"],
        ),
        case("code_grep", json!({"pattern": "alpha("}), "code grep", &["alpha("]),
        case(
            "code_grep",
            json!({"pattern": "alpha\\(\\d", "regex": true, "path": "gamma", "context": true}),
            "code grep",
            &["alpha\\(\\d", "--regex", "--path", "gamma", "--context"],
        ),
        case("code_refs", json!({"name": "alpha"}), "code refs", &["alpha"]),
        case(
            "code_refs",
            json!({"name": "alpha", "path": "src/alpha.rs"}),
            "code callers",
            &["src/alpha.rs", "alpha"],
        ),
        case(
            "code_refs",
            json!({"name": "std::collections", "relation": "importers"}),
            "code dependents",
            &["std::collections"],
        ),
        case("find_files", json!({"query": "gamma"}), "code find", &["gamma"]),
        case("find_files", json!({}), "code files", &[]),
        case("git_log", json!({}), "git log", &[]),
        case(
            "git_log",
            json!({"path": "src/alpha.rs"}),
            "git touching",
            &["src/alpha.rs"],
        ),
        case(
            "git_blame",
            json!({"path": "src/alpha.rs", "line": 4, "end_line": 6}),
            "git blame",
            &["src/alpha.rs", "--line", "4", "--end-line", "6"],
        ),
        case("git_diff", json!({}), "git status", &[]),
        case(
            "git_diff",
            json!({"path": "src/alpha.rs", "structural": true}),
            "git diff-outline",
            &["src/alpha.rs"],
        ),
        case("git_log", json!({"author": "t"}), "git log", &["--author", "t"]).ignoring(SEARCH_VOLATILE),
        case(
            "git_log",
            json!({"query": "init", "author": "t"}),
            "git search",
            &["init", "--author", "t"],
        )
        .ignoring(SEARCH_VOLATILE),
        case(
            "git_log",
            json!({"query": "init", "author": "nobody"}),
            "git search",
            &["init", "--author", "nobody"],
        )
        .ignoring(SEARCH_VOLATILE),
        case(
            "code_refs",
            json!({"name": "alpha", "relation": "definition", "path": "src/gamma.rs", "line": 4}),
            "code definition",
            &["src/gamma.rs", "4", "alpha"],
        ),
        case(
            "code_refs",
            json!({"name": "alpha", "path": "src/alpha.rs", "line": 5, "match": "substring"}),
            "code callers",
            &["src/alpha.rs", "alpha", "--line", "5", "--match", "substring"],
        ),
        case(
            "code_refs",
            json!({"name": "std::collections", "relation": "importers", "max_tokens": 50}),
            "code dependents",
            &["std::collections", "--max-tokens", "50"],
        ),
        case(
            "code_search",
            json!({"query": "adds one", "lane": "keyword"}),
            "code search",
            &["adds one", "--lane", "keyword"],
        ),
        case(
            "memory_save",
            json!({"key": "parity/note", "value": "written by the parity test"}),
            "memory put",
            &["parity/note", "written by the parity test"],
        )
        .ignoring(NOTE_VOLATILE),
        case(
            "memory_search",
            json!({"key": "parity/note"}),
            "memory get",
            &["parity/note"],
        )
        .ignoring(NOTE_VOLATILE),
        case(
            "memory_search",
            json!({"prefix": "parity/"}),
            "memory list",
            &["--prefix", "parity/"],
        )
        .ignoring(NOTE_VOLATILE),
        case("index_status", json!({}), "status", &[]).ignoring(STATUS_VOLATILE),
    ]
}

/// Drop fields that legitimately differ between two runs or between transports, plus the case's
/// own `volatile` ones.
pub fn normalize(value: &mut Value, volatile: &[&str]) {
    const NOISE: &[&str] = &["elapsed_us", "startup_us", "notice"];
    match value {
        Value::Object(map) => {
            map.retain(|k, _| !NOISE.contains(&k.as_str()) && !volatile.contains(&k.as_str()));
            map.values_mut().for_each(|child| normalize(child, volatile));
        }
        Value::Array(items) => items.iter_mut().for_each(|child| normalize(child, volatile)),
        _ => {}
    }
}

/// Whether the capability table says `cli` runs the body of `tool`.
pub fn table_pairs(rows: &[Row], tool: &str, cli: &str) -> bool {
    rows.iter()
        .filter_map(Row::runs)
        .any(|(mcp, path)| mcp.tool == tool && mcp.mode.is_empty() && path == cli)
}

pub fn run_cli(root: &std::path::Path, cli_path: &str, operands: &[&str]) -> Result<Value, String> {
    let output = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["--root", root.to_str().expect("utf8 root"), "--json"])
        .args(cli_path.split(' '))
        .args(operands)
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "exit {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("non-JSON stdout ({e}): {}", String::from_utf8_lossy(&output.stdout)))
}
