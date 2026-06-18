//! End-to-end smoke test for the in-process `basemind` CLI tool surface.
//!
//! Builds a tiny throwaway git repo, scans it with the built binary, then runs a
//! representative slice of tool subcommands across groups. For each it asserts:
//! - `--json` output parses as JSON and carries the same top-level fields the MCP
//!   Response struct produces (proving the CLI runs the identical tool code).
//! - human output (no `--json`) is non-empty and contains an expected substring.
//!
//! Tests are independent + idempotent: each builds its own tempdir and cleans up
//! on drop. Mirrors the fixture style of `tests/mcp_smoke.rs` / `tests/scan_smoke.rs`.

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

/// Path to the freshly built `basemind` binary (cargo sets `CARGO_BIN_EXE_<name>`).
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

/// Build a tiny repo with a couple of Rust files and an initial commit, then scan it.
fn build_and_scan() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(
        root.join("a.rs"),
        b"pub fn alpha() {}\npub struct Beta { x: i32 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("c.rs"),
        b"pub fn caller() { alpha(); alpha(); }\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);

    let status = Command::new(bin())
        .args(["--root", root.to_str().unwrap(), "scan", "--quiet"])
        .status()
        .expect("run basemind scan");
    assert!(status.success(), "basemind scan failed");
    dir
}

/// Run `basemind --root <root> [extra args...]` and return (stdout, success).
fn run(root: &Path, args: &[&str]) -> (String, bool) {
    let mut full = vec!["--root", root.to_str().unwrap()];
    full.extend_from_slice(args);
    let output = Command::new(bin())
        .args(&full)
        .output()
        .expect("run basemind");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.success(),
    )
}

/// Assert that a `--json` invocation parses and every named field is present.
fn assert_json_fields(root: &Path, args: &[&str], fields: &[&str]) -> Value {
    let mut json_args = vec!["--json"];
    json_args.extend_from_slice(args);
    let (stdout, ok) = run(root, &json_args);
    assert!(ok, "command {args:?} exited non-zero; stdout: {stdout}");
    let value: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("{args:?} not JSON: {e}\n{stdout}"));
    for field in fields {
        assert!(
            value.get(field).is_some(),
            "{args:?} JSON missing field `{field}`; got: {value}"
        );
    }
    value
}

/// Assert that a human (non-`--json`) invocation produces non-empty output
/// containing `needle`.
fn assert_human_contains(root: &Path, args: &[&str], needle: &str) {
    let (stdout, ok) = run(root, args);
    assert!(ok, "command {args:?} exited non-zero; stdout: {stdout}");
    assert!(!stdout.trim().is_empty(), "{args:?} produced empty output");
    assert!(
        stdout.contains(needle),
        "{args:?} output missing {needle:?}; got: {stdout}"
    );
}

#[test]
fn query_outline_reports_symbols() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(
        root,
        &["query", "outline", "a.rs"],
        &["path", "language", "symbols", "imports"],
    );
    let symbols = v["symbols"].as_array().expect("symbols array");
    assert_eq!(symbols.len(), 2, "expected alpha + Beta");
    assert_human_contains(root, &["query", "outline", "a.rs"], "alpha");
}

#[test]
fn query_search_finds_symbol() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(
        root,
        &["query", "search", "alpha"],
        &["total", "truncated", "results"],
    );
    assert_eq!(v["total"], 1);
    assert_human_contains(root, &["query", "search", "alpha"], "alpha");
}

#[test]
fn query_references_finds_call_sites() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(
        root,
        &["query", "references", "alpha"],
        &["name", "total", "hits"],
    );
    assert_eq!(v["total"], 2, "alpha is called twice in c.rs");
    assert_human_contains(root, &["query", "references", "alpha"], "c.rs");
}

#[test]
fn query_status_reports_file_count() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(
        root,
        &["query", "status"],
        &[
            "file_count",
            "total_size_bytes",
            "languages",
            "schema_version",
        ],
    );
    assert_eq!(v["file_count"], 2);
    assert_human_contains(root, &["query", "status"], "file_count");
}

#[test]
fn query_list_files_enumerates() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(
        root,
        &["query", "list-files"],
        &["total", "returned", "files"],
    );
    assert_eq!(v["total"], 2);
    assert_human_contains(root, &["query", "list-files"], "a.rs");
}

#[test]
fn git_working_tree_status_is_clean() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(
        root,
        &["git", "working-tree-status"],
        &["staged_added", "modified", "untracked", "is_clean"],
    );
    assert_eq!(v["is_clean"], true, "repo should be clean after commit");
    assert_human_contains(root, &["git", "working-tree-status"], "is_clean");
}

#[test]
fn cache_stats_reports_blob_accounting() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(
        root,
        &["cache", "stats"],
        &["blobs_bytes", "blob_count", "orphan_blob_count"],
    );
    assert!(
        v["blob_count"].as_u64().unwrap() >= 2,
        "at least one blob per file"
    );
    assert_human_contains(root, &["cache", "stats"], "blob_count");
}
