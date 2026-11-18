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
    /// The tool reads the workspace's fjall index (the keyword lane, memory, and `code_refs` call
    /// sites and callers, which scan it), which the in-process writer locks for as long as it lives. Beside it, with no daemon, the CLI cannot read that index and must refuse busy
    /// (exit 3) naming the fix ([`refuses_beside_the_writer`]); the payloads are compared once the
    /// session has ended. Every other case answers beside the writer.
    pub after_session: bool,
}

const fn case(tool: &'static str, mcp: Value, cli: &'static str, cli_operands: &'static [&'static str]) -> Case {
    Case {
        tool,
        mcp,
        cli,
        cli_operands,
        volatile: &[],
        after_session: false,
    }
}

impl Case {
    const fn ignoring(mut self, volatile: &'static [&'static str]) -> Self {
        self.volatile = volatile;
        self
    }

    const fn after_session(mut self) -> Self {
        self.after_session = true;
        self
    }
}

/// Fields of `index_status` that describe the process answering, not the index: how long its map
/// took to warm, and the root as that process spelled it (`/var/…` against `/private/var/…`).
const STATUS_VOLATILE: &[&str] = &["warm_ms", "root"];

/// Whether a commit search was answered by the live walk: the server has the git-history index
/// warm, a one-shot CLI call may not.
const SEARCH_VOLATILE: &[&str] = &["truncated", "truncated_reason"];

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
        case("code_refs", json!({"name": "alpha"}), "code refs", &["alpha"]).after_session(),
        case(
            "code_refs",
            json!({"name": "alpha", "path": "src/alpha.rs"}),
            "code callers",
            &["src/alpha.rs", "alpha"],
        )
        .after_session(),
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
        )
        .after_session(),
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
        )
        .after_session(),
        case(
            "memory_save",
            json!({"key": "parity/note", "value": "written by the parity test"}),
            "memory put",
            &["parity/note", "written by the parity test"],
        )
        .ignoring(NOTE_VOLATILE)
        .after_session(),
        case(
            "memory_search",
            json!({"key": "parity/note"}),
            "memory get",
            &["parity/note"],
        )
        .ignoring(NOTE_VOLATILE)
        .after_session(),
        case(
            "memory_search",
            json!({"prefix": "parity/"}),
            "memory list",
            &["--prefix", "parity/"],
        )
        .ignoring(NOTE_VOLATILE)
        .after_session(),
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

/// Whether a CLI command run beside the writer left the git-history database alone: the writer
/// holds it, so a one-shot command live-walks at once instead of retrying the open (each attempt
/// logs the database path).
pub fn leaves_the_held_history_alone(root: &std::path::Path, cli_path: &str, operands: &[&str]) -> Result<(), String> {
    let output = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["--root", root.to_str().expect("utf8 root"), "--json"])
        .args(cli_path.split(' '))
        .args(operands)
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() || stderr.contains("git-history.fjall") {
        return Err(format!("exit {:?}: {stderr}", output.status.code()));
    }
    Ok(())
}

/// How [`run_cli`] reports a command that exited busy (`3`): the workspace's lock is held elsewhere.
const BUSY_EXIT: &str = "exit Some(3): ";

/// Whether the CLI refused a command beside the writer as the contract says: busy, naming the fix.
/// The daemon is offered as the fix only in a build that has one (`comms` on Unix or Windows).
pub fn refuses_beside_the_writer(cli: &Result<Value, String>) -> bool {
    let daemon_fix = cfg!(all(feature = "comms", any(unix, windows)));
    matches!(cli, Err(message) if message.starts_with(BUSY_EXIT)
        && message.contains("holds this workspace's index")
        && message.contains("stop the process that holds the lock")
        && message.contains("`basemind comms start`") == daemon_fix)
}

/// [`run_cli`] for a command issued right after the in-process server ended: its store lock is
/// released when the server's background work winds down, which can trail the client's disconnect
/// by a moment. Until then the command exits busy, the contract for a held lock, and is retried.
pub fn run_cli_once_unlocked(root: &std::path::Path, cli_path: &str, operands: &[&str]) -> Result<Value, String> {
    const ATTEMPTS: usize = 20;
    let mut last = run_cli(root, cli_path, operands);
    for _ in 1..ATTEMPTS {
        match &last {
            Err(message) if message.starts_with(BUSY_EXIT) => {
                std::thread::sleep(std::time::Duration::from_millis(250));
                last = run_cli(root, cli_path, operands);
            }
            _ => break,
        }
    }
    last
}
