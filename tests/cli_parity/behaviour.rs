//! Layer 3: behavioural parity. The same read-only query is issued through the in-process MCP server
//! and through the spawned CLI (`--json`) against one scanned fixture repo; the payloads must be
//! equal once run-to-run noise (timings, lifecycle notices) is removed.

use std::process::Command;

use serde_json::{Value, json};

use crate::capabilities::{McpOp, Row};

/// A query: the MCP call plus the CLI operands that follow the command path in the capability table.
pub struct Case {
    pub op: McpOp,
    pub mcp: Value,
    pub cli_operands: &'static [&'static str],
    /// Compare list order-insensitively (a known ordering gap, see the case's comment).
    pub unordered: bool,
}

impl Case {
    fn new(tool: &'static str, mode: &'static str, mcp: Value, cli_operands: &'static [&'static str]) -> Self {
        Self {
            op: McpOp { tool, mode },
            mcp,
            cli_operands,
            unordered: false,
        }
    }
}

pub fn cases() -> Vec<Case> {
    vec![
        Case::new(
            "code",
            "outline",
            json!({"mode": "outline", "path": "src/alpha.rs"}),
            &["src/alpha.rs"],
        ),
        Case::new(
            "code",
            "symbols",
            json!({"mode": "symbols", "name": "alpha"}),
            &["alpha"],
        ),
        Case::new("code", "find", json!({"mode": "find", "query": "gamma"}), &["gamma"]),
        Case {
            // TODO parity: the listing order differs between the long-lived server and a fresh CLI
            // process (hash-ordered code-map cache); compared as a set until it is made stable.
            unordered: true,
            ..Case::new("code", "files", json!({"mode": "files"}), &[])
        },
        Case::new(
            "code",
            "grep",
            json!({"mode": "grep", "pattern": "alpha\\("}),
            &["alpha\\("],
        ),
        Case::new(
            "code",
            "references",
            json!({"mode": "references", "name": "alpha"}),
            &["alpha"],
        ),
        Case::new(
            "code",
            "callers",
            json!({"mode": "callers", "path": "src/alpha.rs", "name": "alpha"}),
            &["src/alpha.rs", "alpha"],
        ),
        Case::new(
            "code",
            "dependents",
            json!({"mode": "dependents", "module": "std::collections"}),
            &["std::collections"],
        ),
    ]
}

/// Drop fields that legitimately differ between two runs or between transports.
pub fn normalize(value: &mut Value, unordered: bool) {
    const NOISE: &[&str] = &["elapsed_us", "startup_us", "notice"];
    match value {
        Value::Object(map) => {
            map.retain(|k, _| !NOISE.contains(&k.as_str()));
            map.values_mut().for_each(|v| normalize(v, unordered));
        }
        Value::Array(items) => {
            items.iter_mut().for_each(|v| normalize(v, unordered));
            if unordered {
                items.sort_by_key(Value::to_string);
            }
        }
        _ => {}
    }
}

/// The CLI path the capability table pairs with `op`.
pub fn cli_path_for(rows: &[Row], op: McpOp) -> Option<&'static str> {
    rows.iter().find_map(|r| match r {
        Row::Pair { mcp, cli } if *mcp == op => Some(*cli),
        _ => None,
    })
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
