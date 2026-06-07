//! End-to-end smoke test for the MCP server.
//!
//! Builds a tiny throwaway git repo with the system `git` (same pattern as `git_smoke.rs`),
//! scans it via the gitmind library, spawns `gitmind serve` as a subprocess, and exercises
//! a representative slice of MCP tools through the rmcp child-process transport. The goal
//! is to keep the entire MCP integration path green in normal `cargo test` runs without
//! waiting for the heavier real-OSS hardening harness (`tests/harden.rs`, `#[ignore]`'d).
//!
//! What this covers (and the gating harness goes deeper on):
//! * stdio JSON-RPC framing through `rmcp`
//! * tool dispatch + parameter deserialization
//! * `Repo::is_shallow()` plumbing → `truncated` flag on history-walking responses
//! * the in-process scan → on-disk `.gitmind/` → MCP server preload chain
//!
//! Runs in < 5 s on a warm-build machine.

use std::path::Path;
use std::process::Command;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::Command as AsyncCommand;

// ─── helpers (kept inline; shared module setup adds noise for one file) ─────

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

fn build_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(
        root.join("a.rs"),
        b"pub fn alpha() {}\npub struct Beta { x: i32 }\nimpl Beta {\n  pub fn doit(&self) {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.ts"),
        b"export const Greet = (name: string) => `hi ${name}`;\nexport function plain() { return 1; }\n",
    )
    .unwrap();
    git(root, &["add", "a.rs", "b.ts"]);
    git(root, &["commit", "-qm", "init"]);
    // Touch a.rs in a second commit so symbol_history has something to chew on.
    std::fs::write(
        root.join("a.rs"),
        b"pub fn alpha() { let _ = 1; }\npub struct Beta { x: i32 }\nimpl Beta {\n  pub fn doit(&self) {}\n}\n",
    )
    .unwrap();
    git(root, &["commit", "-aqm", "tweak alpha"]);
    dir
}

fn run_scan(root: &Path) {
    let cfg = gitmind::config::default_for_root(root);
    let _ = gitmind::lang::ensure_grammars().expect("grammar bootstrap");
    let mut store =
        gitmind::store::Store::open(root, gitmind::store::VIEW_WORKING).expect("open store");
    gitmind::scanner::scan(
        root,
        &mut store,
        &cfg,
        gitmind::scanner::ScanSource::WorkingTree,
    )
    .expect("scan");
}

fn decode_text(result: &CallToolResult) -> Value {
    use rmcp::model::RawContent;
    let raw = result
        .content
        .iter()
        .find_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or(Value::Null)
}

fn call_params(name: &'static str, args: Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name);
    if let Some(obj) = args.as_object() {
        params = params.with_arguments(obj.clone());
    }
    params
}

// ─── the test ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_server_exercises_representative_tools() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let bin = env!("CARGO_BIN_EXE_gitmind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root")
            .arg(root)
            .arg("serve")
            .arg("--view")
            .arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn gitmind serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    // status: file_count > 0, languages includes rust + typescript
    let body = decode_text(
        &service
            .call_tool(call_params("status", json!({})))
            .await
            .expect("status"),
    );
    let file_count = body.get("file_count").and_then(Value::as_u64).unwrap_or(0);
    assert!(file_count >= 2, "scan should have indexed at least 2 files");
    let langs = body
        .get("languages")
        .and_then(Value::as_object)
        .expect("languages object");
    assert!(
        langs.contains_key("rust"),
        "rust should be present: {langs:?}"
    );
    assert!(
        langs.contains_key("typescript"),
        "typescript should be present: {langs:?}"
    );

    // outline: a.rs surfaces alpha, Beta, doit (method); the new impl-kind symbol exists too
    let body = decode_text(
        &service
            .call_tool(call_params(
                "outline",
                json!({ "path": "a.rs", "l2": false }),
            ))
            .await
            .expect("outline"),
    );
    let symbols = body
        .get("symbols")
        .and_then(Value::as_array)
        .expect("symbols");
    let names: Vec<String> = symbols
        .iter()
        .filter_map(|s| s.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    assert!(names.contains(&"alpha".to_string()), "got {names:?}");
    assert!(names.contains(&"Beta".to_string()), "got {names:?}");
    let impl_kind = symbols
        .iter()
        .any(|s| s.get("kind").and_then(Value::as_str) == Some("impl"));
    assert!(
        impl_kind,
        "Stage 2 impl-kind symbol should be present: {names:?}"
    );

    // search_symbols: arrow-fn const is now kind=function (Stage 2)
    let body = decode_text(
        &service
            .call_tool(call_params(
                "search_symbols",
                json!({ "needle": "Greet", "limit": 10 }),
            ))
            .await
            .expect("search_symbols"),
    );
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .expect("results");
    assert_eq!(results.len(), 1, "one Greet hit: {results:?}");
    assert_eq!(
        results[0].get("kind").and_then(Value::as_str),
        Some("function"),
        "Stage 2 arrow-fn const should be kind=function"
    );

    // recent_changes: should return 2 commits; not shallow → no truncated flag
    let body = decode_text(
        &service
            .call_tool(call_params(
                "recent_changes",
                json!({ "limit": 5, "include_files": true }),
            ))
            .await
            .expect("recent_changes"),
    );
    let commits = body
        .get("commits")
        .and_then(Value::as_array)
        .expect("commits");
    assert_eq!(commits.len(), 2, "two commits expected");
    assert!(
        body.get("truncated").is_none() || body.get("truncated") == Some(&Value::Bool(false)),
        "non-shallow clone should not surface truncated=true"
    );

    // symbol_history on alpha: Stage 5's normalization keeps whitespace-only commits silent.
    // The 'tweak alpha' commit changes a literal so we expect ≥ 1 "modified" entry.
    let body = decode_text(
        &service
            .call_tool(call_params(
                "symbol_history",
                json!({ "path": "a.rs", "name": "alpha", "limit": 10 }),
            ))
            .await
            .expect("symbol_history"),
    );
    let history = body
        .get("history")
        .and_then(Value::as_array)
        .expect("history");
    let modifieds = history
        .iter()
        .filter(|e| e.get("change").and_then(Value::as_str) == Some("modified"))
        .count();
    assert!(
        modifieds >= 1,
        "expected ≥ 1 'modified' entry for alpha after the tweak commit: {history:?}"
    );

    // blame_file: should succeed on a non-shallow repo (the gix shallow path doesn't fire).
    let body = decode_text(
        &service
            .call_tool(call_params("blame_file", json!({ "path": "a.rs" })))
            .await
            .expect("blame_file"),
    );
    let hunks = body.get("hunks").and_then(Value::as_array).expect("hunks");
    assert!(
        !hunks.is_empty(),
        "blame should return hunks on a real file"
    );

    let _ = service.cancel().await;
}
