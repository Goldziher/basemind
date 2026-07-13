//! Focused end-to-end smoke test for the scope-resolved `find_callers` path.
//!
//! Mirrors `mcp_smoke.rs`: scan a tiny fixture in-process, spawn `basemind serve`, and drive the
//! `find_callers` tool over the rmcp child-process transport. This one exercises the *resolution*
//! layer — which ANNOTATES the name scan rather than replacing it.
//!
//! It used to assert that resolution *replaced* the name scan (returning only the resolved edges and
//! setting `resolved: true`). That was the P0: resolution silently dropped every caller it could not
//! bind — module-object imports, unresolvable path aliases — and reported the remainder as the
//! complete answer, with no truncation flag. Precision now travels as a per-hit `resolved` flag, so a
//! same-named symbol in another scope is still *distinguishable* without being *dropped*.
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
    basemind::store::init_isolated_cache();
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
    let mut cfg = basemind::config::default_for_root(root);
    cfg.documents.embed = false;
    cfg.code_search.embed = false;
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
    basemind::scanner::scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .expect("scan");
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
async fn find_callers_flags_scope_resolved_hits_without_dropping_same_named_ones() {
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
        body.get("resolved_total").and_then(Value::as_u64),
        Some(2),
        "exactly the two util.ts callers are PROVEN to bind to util.ts target: {body}"
    );
    assert_eq!(
        body.get("total").and_then(Value::as_u64),
        Some(3),
        "the name scan is the floor: the other.ts site is reported too, never silently dropped: {body}"
    );
    let hits = body.get("hits").and_then(Value::as_array).expect("hits");
    let proven: Vec<&str> = hits
        .iter()
        .filter(|h| h.get("resolved").and_then(Value::as_bool) == Some(true))
        .filter_map(|h| h.get("path").and_then(Value::as_str))
        .collect();
    assert_eq!(
        proven,
        vec!["util.ts", "util.ts"],
        "other.ts target() (same name, different scope) must NOT be conflated INTO the proven set: {body}"
    );
    let unproven: Vec<&str> = hits
        .iter()
        .filter(|h| h.get("resolved").and_then(Value::as_bool) == Some(false))
        .filter_map(|h| h.get("path").and_then(Value::as_str))
        .collect();
    assert_eq!(
        unproven,
        vec!["other.ts"],
        "the same-name site is surfaced as unproven so a caller can filter it, not omitted: {body}"
    );

    service.cancel().await.expect("shutdown");
}
