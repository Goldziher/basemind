//! Focused end-to-end smoke test for the scope-resolved `find_callers` path.
//!
//! Mirrors `mcp_smoke.rs`: scan a tiny fixture in-process, spawn `basemind serve`, and drive the
//! `find_callers` tool over the rmcp child-process transport. This one exercises the *resolved*
//! mode — `find_callers` prefers the scope/import-resolved edges over the name scan, so a caller of
//! a same-named function in another file is never conflated.
//!
//! Gated on `code-intel-js`: only the oxc JS/TS engine resolves top-level function calls to their
//! definition today, so under default features the whole test compiles out (graceful skip).

#![cfg(feature = "code-intel-js")]

use std::path::Path;
use std::process::Command;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::Command as AsyncCommand;

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

/// Two TS files each defining a `target` function: `util.ts` has two callers that resolve to its
/// export; `other.ts` has one caller that resolves to *its own* same-named function.
fn build_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(
        root.join("util.ts"),
        b"export function target() { return 1; }\ntarget();\ntarget();\n",
    )
    .unwrap();
    std::fs::write(root.join("other.ts"), b"function target() { return 3; }\ntarget();\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    dir
}

fn run_scan(root: &Path) {
    // Embed off: this runs `scan` on a `#[tokio::test]` thread, and an embedding scan with the ONNX
    // model cached opens LanceDB (`block_on` inside the live runtime) → panic. The test resolves
    // callers from the index, not vectors; production wraps `scan` in `spawn_blocking`.
    let mut cfg = basemind::config::default_for_root(root);
    cfg.documents.embed = false;
    cfg.code_search.embed = false;
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
    basemind::scanner::scan(root, &mut store, &cfg, basemind::scanner::ScanSource::WorkingTree).expect("scan");
}

fn decode_text(result: &CallToolResult) -> Value {
    use rmcp::model::ContentBlock;
    let raw = result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn find_callers_resolves_scope_and_ignores_same_named_symbol() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let body = decode_text(
        &service
            .call_tool(call_params(
                "find_callers",
                json!({ "path": "util.ts", "name": "target", "kind": "function" }),
            ))
            .await
            .expect("find_callers"),
    );

    assert_eq!(
        body.get("definition")
            .and_then(|d| d.get("name"))
            .and_then(Value::as_str),
        Some("target"),
        "definition should resolve to util.ts target"
    );
    assert_eq!(
        body.get("resolved").and_then(Value::as_bool),
        Some(true),
        "hits must come from the scope-resolved edges, not the name scan: {body}"
    );
    let hits = body.get("hits").and_then(Value::as_array).expect("hits");
    assert_eq!(
        hits.len(),
        2,
        "exactly the two util.ts callers resolve to util.ts target"
    );
    assert!(
        hits.iter()
            .all(|h| h.get("path").and_then(Value::as_str) == Some("util.ts")),
        "other.ts target() (same name, different scope) must NOT be conflated: {body}"
    );

    service.cancel().await.expect("shutdown");
}
