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
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\npub struct Beta { x: i32 }\n").unwrap();
    std::fs::write(root.join("c.rs"), b"pub fn caller() { alpha(); alpha(); }\n").unwrap();
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
    let output = Command::new(bin()).args(&full).output().expect("run basemind");
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
    let value: Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{args:?} not JSON: {e}\n{stdout}"));
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
fn code_outline_reports_symbols() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(
        root,
        &["code", "outline", "a.rs"],
        &["path", "language", "total", "symbols"],
    );
    let symbols = v["symbols"].as_array().expect("symbols array");
    assert_eq!(symbols.len(), 2, "expected alpha + Beta");
    let line_of = |name: &str| {
        symbols
            .iter()
            .find(|symbol| symbol["name"] == name)
            .unwrap_or_else(|| panic!("no symbol `{name}` in {v}"))["line"]
            .clone()
    };
    assert_eq!(line_of("alpha"), 1, "outline lines are 1-based");
    assert_eq!(line_of("Beta"), 2);
    assert_human_contains(root, &["code", "outline", "a.rs"], "alpha");
}

#[test]
fn code_symbols_finds_symbol() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(root, &["code", "symbols", "alpha"], &["results"]);
    assert_eq!(v["results"], serde_json::json!(["a.rs:1: pub fn alpha()"]), "{v}");
    assert!(v.get("truncated").is_none(), "a false `truncated` is left out: {v}");
    assert_human_contains(root, &["code", "symbols", "alpha"], "alpha");
}

#[test]
fn code_references_finds_call_sites() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(root, &["code", "references", "alpha"], &["total", "hits"]);
    assert_eq!(
        v["hits"]["c.rs"].as_array().map(Vec::len),
        Some(2),
        "hits listed under their file: {v}"
    );
    assert_eq!(v["total"], 2, "alpha is called twice in c.rs");
    assert_human_contains(root, &["code", "references", "alpha"], "c.rs");
}

#[test]
fn admin_status_reports_file_count() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(
        root,
        &["status"],
        &["file_count", "total_size_bytes", "languages", "schema_version"],
    );
    assert_eq!(v["file_count"], 2);
    assert_human_contains(root, &["status"], "file_count");
}

#[test]
fn code_files_enumerates() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(root, &["code", "files"], &["files"]);
    assert_eq!(v["files"].as_array().map(Vec::len), Some(2), "{v}");
    assert!(v.get("total").is_none(), "no `next_cursor`, so no count: {v}");
    assert_human_contains(root, &["code", "files"], "a.rs");
}

#[test]
fn git_status_is_clean() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(root, &["git", "status"], &["is_clean"]);
    assert_eq!(v["is_clean"], true, "repo should be clean after commit");
    for list in ["staged_added", "modified", "untracked"] {
        assert!(v.get(list).is_none(), "an empty `{list}` is left out: {v}");
    }
    assert_human_contains(root, &["git", "status"], "is_clean");
}

#[test]
fn git_search_finds_commit_by_author() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(root, &["git", "log", "--author", "t"], &["commits"]);
    let commits = v["commits"].as_array().expect("commits array");
    assert_eq!(commits.len(), 1, "author search should find the one commit");
    assert_human_contains(root, &["git", "search", "init"], "init");
}

#[test]
fn rescan_without_paths_reindexes_a_new_file() {
    let dir = build_and_scan();
    let root = dir.path();
    std::fs::write(root.join("d.rs"), b"pub fn delta() {}\n").unwrap();

    let (stdout, ok) = run(root, &["rescan", "--quiet"]);
    assert!(ok, "rescan exited non-zero; stdout: {stdout}");

    let search = assert_json_fields(root, &["code", "symbols", "delta"], &["results"]);
    assert_eq!(
        search["results"],
        serde_json::json!(["d.rs:1: pub fn delta()"]),
        "a rescan with no path must index the new symbol"
    );
    let status = assert_json_fields(root, &["status"], &["file_count"]);
    assert_eq!(status["file_count"], 3, "a rescan with no path must index the new file");
}

#[test]
fn rescan_scoped_path_reindexes_only_that_path() {
    let dir = build_and_scan();
    let root = dir.path();
    std::fs::write(root.join("d.rs"), b"pub fn delta() {}\n").unwrap();

    let (stdout, ok) = run(root, &["rescan", "d.rs", "--quiet"]);
    assert!(ok, "scoped rescan exited non-zero; stdout: {stdout}");

    let search = assert_json_fields(root, &["code", "symbols", "delta"], &["results"]);
    assert_eq!(
        search["results"],
        serde_json::json!(["d.rs:1: pub fn delta()"]),
        "scoped rescan must index the named path"
    );
}

#[test]
fn cache_stats_reports_blob_accounting() {
    let dir = build_and_scan();
    let root = dir.path();
    let v = assert_json_fields(
        root,
        &["cache", "stats"],
        &[
            "blobs_bytes",
            "blob_count",
            "orphan_blob_count",
            "git_history_bytes",
            "total_bytes",
            "other_bytes",
            "blob_accounting_ok",
        ],
    );
    assert!(v["blob_count"].as_u64().unwrap() >= 2, "at least one blob per file");
    let u = |k: &str| v[k].as_u64().unwrap_or_default();
    let component_sum = u("blobs_bytes")
        + u("views_bytes")
        + u("group_index_bytes")
        + u("lance_bytes")
        + u("git_cache_bytes")
        + u("telemetry_bytes")
        + u("git_history_bytes");
    assert_eq!(
        u("total_bytes"),
        component_sum + u("other_bytes"),
        "total_bytes must reconcile to components + other"
    );
    assert_human_contains(root, &["cache", "stats"], "blob_count");
}

/// A refused command prints its message, never a backtrace, even with `RUST_BACKTRACE=1` set (as CI
/// sets it): anyhow captures one then, and `{:?}` would print it after the error.
#[test]
fn a_refusal_prints_no_backtrace_under_rust_backtrace() {
    let dir = build_and_scan();
    let output = Command::new(bin())
        .args([
            "--root",
            dir.path().to_str().unwrap(),
            "git",
            "by-path",
            "a",
            "--limit",
            "0",
        ])
        .env("RUST_BACKTRACE", "1")
        .env("RUST_LIB_BACKTRACE", "1")
        .output()
        .expect("run basemind");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "limit 0 is refused: {stderr}");
    assert!(!stderr.contains("Stack backtrace"), "{stderr}");
    assert!(
        stderr.trim_end().ends_with(
            "Error: git_by_path: `git_by_path`: `limit` must be at least 1; omit it for the default page size."
        ),
        "{stderr}"
    );
}

/// A command run from a directory that was removed under it (a deleted worktree) fails with an
/// error naming the directory read, not a panic.
#[cfg(unix)]
#[test]
fn a_removed_working_directory_is_an_error_not_a_panic() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let gone = dir.path().join("gone");
    std::fs::create_dir(&gone).expect("create the doomed directory");
    let output = Command::new("sh")
        .args(["-c", r#"cd "$1" && rmdir "$1" && exec "$2" status"#, "sh"])
        .arg(&gone)
        .arg(bin())
        .output()
        .expect("run basemind from a removed directory");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!gone.exists(), "the working directory was not removed");
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(
        stderr.trim_end().ends_with(
            "Error: read the current directory (pass --root to name the repository)\n\nCaused by:\n    \
             No such file or directory (os error 2)"
        ),
        "{stderr}"
    );
}
