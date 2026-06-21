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
fn rescan_full_reindexes_new_file() {
    let dir = build_and_scan();
    let root = dir.path();
    // A new file the initial scan never saw.
    std::fs::write(root.join("d.rs"), b"pub fn delta() {}\n").unwrap();

    let (stdout, ok) = run(root, &["rescan", "--full", "--quiet"]);
    assert!(ok, "rescan --full exited non-zero; stdout: {stdout}");

    // The full re-index must pick up the new symbol and the new file.
    let search = assert_json_fields(root, &["query", "search", "delta"], &["total", "results"]);
    assert_eq!(
        search["total"], 1,
        "rescan --full must index the new symbol"
    );
    let status = assert_json_fields(root, &["query", "status"], &["file_count"]);
    assert_eq!(
        status["file_count"], 3,
        "rescan --full must index the new file"
    );
}

#[test]
fn rescan_scoped_path_reindexes_only_that_path() {
    let dir = build_and_scan();
    let root = dir.path();
    std::fs::write(root.join("d.rs"), b"pub fn delta() {}\n").unwrap();

    // Incremental rescan scoped to the single new path (no --full).
    let (stdout, ok) = run(root, &["rescan", "d.rs", "--quiet"]);
    assert!(ok, "scoped rescan exited non-zero; stdout: {stdout}");

    let search = assert_json_fields(root, &["query", "search", "delta"], &["total", "results"]);
    assert_eq!(
        search["total"], 1,
        "scoped rescan must index the named path"
    );
}

/// Build a fixture repo where `a.rs` and `c.rs` co-change across several commits,
/// giving the mining algorithm genuine signal. Returns the tempdir (kept alive by caller).
///
/// Commit layout (each commit touches the listed files):
///   1. init: a.rs + c.rs (co-change)
///   2. feat: a.rs + c.rs (co-change)
///   3. extra: a.rs + c.rs (co-change)
///   4. solo: a.rs only (solo change — makes freq[a.rs] > cochange count)
///
/// With `--min-support 1 --min-confidence 0.1`, the pair (a.rs, c.rs) qualifies:
///   support = 3, freq[a.rs] = 4, confidence = 3/4 = 0.75 >= 0.1.
#[cfg(feature = "memory")]
fn build_cochange_fixture() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    // Commit 1 — a.rs + c.rs co-change.
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();
    std::fs::write(root.join("c.rs"), b"pub fn caller() { alpha(); }\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init: a and c"]);

    // Commit 2 — a.rs + c.rs co-change.
    std::fs::write(root.join("a.rs"), b"pub fn alpha() -> u32 { 1 }\n").unwrap();
    std::fs::write(root.join("c.rs"), b"pub fn caller() -> u32 { alpha() }\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "feat: typed alpha"]);

    // Commit 3 — a.rs + c.rs co-change.
    std::fs::write(root.join("a.rs"), b"pub fn alpha() -> u32 { 2 }\n").unwrap();
    std::fs::write(
        root.join("c.rs"),
        b"pub fn caller() -> u32 { alpha() + 1 }\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "feat: bump alpha return"]);

    // Commit 4 — a.rs solo change (raises freq[a.rs] without adding co-change).
    std::fs::write(root.join("a.rs"), b"pub fn alpha() -> u32 { 42 }\n").unwrap();
    git(root, &["add", "a.rs"]);
    git(root, &["commit", "-qm", "fix: solo alpha tweak"]);

    let status = Command::new(bin())
        .args(["--root", root.to_str().unwrap(), "scan", "--quiet"])
        .status()
        .expect("run basemind scan");
    assert!(status.success(), "basemind scan failed on cochange fixture");
    dir
}

/// Run `basemind --root <root> [extra args...]` and return (stdout, stderr, success).
fn run_full(root: &Path, args: &[&str]) -> (String, String, bool) {
    let mut full = vec!["--root", root.to_str().unwrap()];
    full.extend_from_slice(args);
    let output = Command::new(bin())
        .args(&full)
        .output()
        .expect("run basemind");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}

// ─── Governance tests ─────────────────────────────────────────────────────────

/// End-to-end governance workflow under `--features memory`:
/// mine → proposals (list) → accept → memory get → reject.
///
/// Requires genuine co-change history; uses `build_cochange_fixture()`.
#[cfg(feature = "memory")]
#[test]
fn governance_mine_proposals_accept_get_reject_end_to_end() {
    let dir = build_cochange_fixture();
    let root = dir.path();

    // ── Step 1: mine with low thresholds so the (a.rs, c.rs) pair is captured ──
    let mine_v = assert_json_fields(
        root,
        &[
            "governance",
            "mine",
            "--min-support",
            "1",
            "--min-confidence",
            "0.1",
        ],
        &["mined", "window_inspected", "skipped_bulk"],
    );
    assert!(
        mine_v["mined"].as_u64().unwrap() >= 1,
        "governance mine must emit at least one co-change proposal; got: {mine_v}"
    );

    // ── Step 2: proposals list — must return the mined candidate ──────────────
    let list_v = assert_json_fields(
        root,
        &["governance", "proposals"],
        &["total", "truncated", "proposals"],
    );
    let proposals = list_v["proposals"]
        .as_array()
        .expect("proposals field must be an array");
    assert!(
        !proposals.is_empty(),
        "governance proposals must list at least one proposal; got: {list_v}"
    );

    let first = &proposals[0];
    let id = first["id"]
        .as_str()
        .expect("each proposal must have an 'id' string field");
    assert!(!id.is_empty(), "proposal id must not be empty");

    // ── Step 3: accept — must write a memory, return accepted=true, exit 0 ────
    //
    // `proposal_accept` materialises the cached `LanceStore`, which owns a tokio
    // runtime. That store is dropped at the end of the CLI's outer `block_on`, i.e.
    // inside an async context — which used to panic the process on teardown ("Cannot
    // drop a runtime in a context where blocking is not allowed") and exit 101 even
    // though the data write succeeded. `LanceStoreInner`'s `Drop` now calls
    // `Runtime::shutdown_background`, so the command exits cleanly. We assert on that.
    let (accept_stdout, accept_stderr, accept_ok) =
        run_full(root, &["--json", "governance", "accept", id]);
    assert!(
        accept_ok,
        "governance accept must exit 0 (no runtime-drop panic); stderr: {accept_stderr}"
    );
    let accept_v: Value = serde_json::from_str(accept_stdout.trim()).unwrap_or_else(|e| {
        panic!("governance accept did not emit JSON: {e}\nstdout: {accept_stdout}")
    });
    assert_eq!(
        accept_v["accepted"],
        serde_json::Value::Bool(true),
        "governance accept must return accepted=true in stdout; got: {accept_v}"
    );
    let memory_key = accept_v["memory_key"]
        .as_str()
        .expect("governance accept must return a memory_key field in stdout");
    assert!(
        memory_key.starts_with("skill/cochange-"),
        "accepted memory key must start with 'skill/cochange-'; got: {memory_key:?}"
    );

    // ── Step 4: memory get — the accepted skill must be retrievable ───────────
    let get_v = assert_json_fields(
        root,
        &["memory", "get", memory_key],
        &["key", "value", "tags"],
    );
    assert_eq!(
        get_v["key"].as_str().unwrap(),
        memory_key,
        "memory get must return the exact key that was stored"
    );
    let tags = get_v["tags"]
        .as_array()
        .expect("memory get response must include a tags array");
    let tag_strs: Vec<&str> = tags.iter().filter_map(|t| t.as_str()).collect();
    assert!(
        tag_strs.contains(&"skill"),
        "accepted memory must carry the 'skill' tag; got tags: {tags:?}"
    );
    assert!(
        tag_strs.contains(&"cochange"),
        "accepted memory must carry the 'cochange' tag; got tags: {tags:?}"
    );

    // ── Step 5: mine again so there is a fresh candidate to reject ────────────
    // Accept DELETES the proposal but does not tombstone it (only reject tombstones),
    // and git history is immutable, so re-mining deterministically regenerates the
    // same content-addressed cluster — `proposals` must be non-empty again.
    let remine_v = assert_json_fields(
        root,
        &[
            "governance",
            "mine",
            "--min-support",
            "1",
            "--min-confidence",
            "0.1",
        ],
        &["mined"],
    );
    assert!(
        remine_v["mined"].as_u64().unwrap() >= 1,
        "re-mine after accept must regenerate the cluster (accept does not tombstone); got: {remine_v}"
    );

    // ── Step 6: reject — must consume the regenerated candidate ───────────────
    let list2_v = assert_json_fields(root, &["governance", "proposals"], &["total", "proposals"]);
    let proposals2 = list2_v["proposals"]
        .as_array()
        .expect("proposals field must be an array after re-mine");
    let reject_id = proposals2
        .first()
        .and_then(|p| p["id"].as_str())
        .expect("re-mine must leave at least one proposal to reject");

    let reject_v = assert_json_fields(
        root,
        &[
            "governance",
            "reject",
            reject_id,
            "--reason",
            "test rejection",
        ],
        &["rejected"],
    );
    assert_eq!(
        reject_v["rejected"],
        serde_json::Value::Bool(true),
        "governance reject must return rejected=true; got: {reject_v}"
    );

    // The tombstone must now suppress that cluster on a fresh mine.
    let post_reject = assert_json_fields(
        root,
        &[
            "governance",
            "mine",
            "--min-support",
            "1",
            "--min-confidence",
            "0.1",
        ],
        &["mined"],
    );
    let still_listed = assert_json_fields(root, &["governance", "proposals"], &["proposals"]);
    let remaining = still_listed["proposals"].as_array().expect("array");
    assert!(
        !remaining
            .iter()
            .any(|p| p["id"].as_str() == Some(reject_id)),
        "rejected proposal id must not reappear after re-mine (tombstone); post_reject={post_reject}, listed={still_listed}"
    );
}

/// Verify that governance subcommands return a graceful failure (non-zero exit,
/// no panic) when the `memory` feature is not compiled in.
///
/// This test runs under both default-features and `--features memory` builds.
/// Under `--features memory` the subcommand succeeds (the test is tolerant of
/// that). Under default features it must NOT succeed and must NOT crash.
#[test]
fn governance_mine_without_memory_feature_does_not_panic() {
    // Use build_and_scan() — the single-commit repo is enough; we never reach the
    // mining step when the feature gate fires.
    let dir = build_and_scan();
    let root = dir.path();

    let (stdout, stderr, ok) = run_full(
        root,
        &[
            "governance",
            "mine",
            "--min-support",
            "1",
            "--min-confidence",
            "0.1",
        ],
    );

    // Under --features memory the command succeeds; under default features it must
    // fail gracefully — non-zero exit, no panic string in stderr.
    if !ok {
        // Non-zero exit is expected when memory feature is off.
        // The error must mention "memory" (the feature name) or "not enabled".
        let combined = format!("{stdout}{stderr}");
        assert!(
            combined.to_lowercase().contains("memory")
                || combined.to_lowercase().contains("not enabled"),
            "governance mine failure must mention 'memory' or 'not enabled'; got stdout={stdout:?} stderr={stderr:?}"
        );
        // Must NOT be a Rust panic.
        assert!(
            !stderr.contains("thread 'main' panicked"),
            "governance mine must not panic when memory feature is off; stderr={stderr:?}"
        );
    }
    // If ok==true (memory feature is on), the command succeeded — nothing to assert.
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
