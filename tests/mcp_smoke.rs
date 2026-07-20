//! End-to-end smoke test for the MCP server.
//!
//! Builds a tiny throwaway git repo with the system `git` (same pattern as `git_smoke.rs`),
//! scans it via the basemind library, spawns `basemind serve` as a subprocess, and exercises
//! a representative slice of MCP tools through the rmcp child-process transport. The goal
//! is to keep the entire MCP integration path green in normal `cargo test` runs without
//! waiting for the heavier real-OSS hardening harness (`tests/harden.rs`, `#[ignore]`'d).
//!
//! What this covers (and the gating harness goes deeper on):
//! * stdio JSON-RPC framing through `rmcp`
//! * tool dispatch + parameter deserialization
//! * `Repo::is_shallow()` plumbing → `truncated` flag on history-walking responses
//! * the in-process scan → on-disk `.basemind/` → MCP server preload chain
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
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(
        root.join("a.rs"),
        b"pub fn alpha() {}\n\
          pub struct Beta { x: i32 }\n\
          impl Beta {\n  pub fn doit(&self) {}\n}\n\
          pub trait Drawable { fn draw(&self); }\n\
          impl Drawable for Beta { fn draw(&self) {} }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.ts"),
        b"export const Greet = (name: string) => `hi ${name}`;\n\
          export function plain() { return 1; }\n\
          interface Drawable { draw(): void; }\n\
          class Rectangle implements Drawable { draw() {} }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("c.rs"),
        b"pub fn zed() {}\n\
          pub fn caller() { alpha(); alpha(); other(); alpha(); zed(); zed(); zed(); zed(); }\n",
    )
    .unwrap();
    std::fs::write(root.join("d.py"), b"class Foo: pass\nclass Bar(Foo): pass\n").unwrap();
    std::fs::write(
        root.join("e.rs"),
        b"pub fn inner() {}\n\
          pub fn middle() { inner(); }\n\
          pub fn outer() { middle(); }\n\
          pub fn zed() {}\n",
    )
    .unwrap();
    std::fs::write(root.join("cyc1.rs"), b"pub fn ping() { pong(); }\n").unwrap();
    std::fs::write(root.join("cyc2.rs"), b"pub fn pong() { ping(); }\n").unwrap();
    git(
        root,
        &["add", "a.rs", "b.ts", "c.rs", "d.py", "e.rs", "cyc1.rs", "cyc2.rs"],
    );
    git(root, &["commit", "-qm", "init"]);
    std::fs::write(
        root.join("a.rs"),
        b"pub fn alpha() { let _ = 1; }\n\
          pub struct Beta { x: i32 }\n\
          impl Beta {\n  pub fn doit(&self) {}\n}\n\
          pub trait Drawable { fn draw(&self); }\n\
          impl Drawable for Beta { fn draw(&self) {} }\n",
    )
    .unwrap();
    git(root, &["commit", "-aqm", "tweak alpha"]);
    dir
}

fn run_scan(root: &Path) {
    let cfg = basemind::config::default_for_root(root);
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    // `#[tokio::test]`, so run the scan on a dedicated std thread to mirror the production context.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
            basemind::scanner::scan(
                root,
                &mut store,
                &cfg,
                basemind::scanner::ScanSource::WorkingTree,
                basemind::scanner::EmbedMode::Inline,
            )
            .expect("scan");
        });
    });
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

/// Return the first text content item verbatim (no JSON parse) — used to inspect the
/// raw TOON payload a tool emits when `format="toon"`.
fn raw_text(result: &CallToolResult) -> String {
    use rmcp::model::ContentBlock;
    result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn call_params(name: &'static str, args: Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name);
    if let Some(obj) = args.as_object() {
        params = params.with_arguments(obj.clone());
    }
    params
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_server_exercises_representative_tools() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let instructions = service
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .unwrap_or_default();
    assert!(
        instructions.contains("Context economy"),
        "server instructions should state the context-economy discipline: {instructions}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("status", json!({})))
            .await
            .expect("status"),
    );
    let file_count = body.get("file_count").and_then(Value::as_u64).unwrap_or(0);
    assert!(file_count >= 2, "scan should have indexed at least 2 files");
    assert!(
        body.get("rebuild_in_progress").is_none(),
        "rebuild_in_progress must be absent when no writer holds the lock: {body:?}"
    );
    let langs = body
        .get("languages")
        .and_then(Value::as_object)
        .expect("languages object");
    assert!(langs.contains_key("rust"), "rust should be present: {langs:?}");
    assert!(
        langs.contains_key("typescript"),
        "typescript should be present: {langs:?}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("outline", json!({ "path": "a.rs", "l2": false })))
            .await
            .expect("outline"),
    );
    let symbols = body.get("symbols").and_then(Value::as_array).expect("symbols");
    let names: Vec<String> = symbols
        .iter()
        .filter_map(|s| s.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    assert!(names.contains(&"alpha".to_string()), "got {names:?}");
    assert!(names.contains(&"Beta".to_string()), "got {names:?}");
    let impl_kind = symbols
        .iter()
        .any(|s| s.get("kind").and_then(Value::as_str) == Some("impl"));
    assert!(impl_kind, "Stage 2 impl-kind symbol should be present: {names:?}");

    let body = decode_text(
        &service
            .call_tool(call_params("search_symbols", json!({ "needle": "Greet", "limit": 10 })))
            .await
            .expect("search_symbols"),
    );
    let results = body.get("results").and_then(Value::as_array).expect("results");
    assert_eq!(results.len(), 1, "one Greet hit: {results:?}");
    assert_eq!(
        results[0].get("kind").and_then(Value::as_str),
        Some("function"),
        "Stage 2 arrow-fn const should be kind=function"
    );

    let json_result = service
        .call_tool(call_params("search_symbols", json!({ "needle": "draw", "limit": 50 })))
        .await
        .expect("search_symbols json");
    let json_body = decode_text(&json_result);
    let json_raw = raw_text(&json_result);
    let json_results = json_body
        .get("results")
        .and_then(Value::as_array)
        .expect("json results")
        .clone();
    assert!(!json_results.is_empty(), "expected draw hits: {json_body:?}");

    let toon_result = service
        .call_tool(call_params(
            "search_symbols",
            json!({ "needle": "draw", "limit": 50, "format": "toon" }),
        ))
        .await
        .expect("search_symbols toon");
    let toon_raw = raw_text(&toon_result);
    assert!(
        toon_raw.len() < json_raw.len(),
        "TOON payload ({} bytes) should be smaller than JSON ({} bytes)\nTOON:\n{toon_raw}",
        toon_raw.len(),
        json_raw.len(),
    );
    assert!(
        toon_raw.contains("results[") && toon_raw.contains("name") && toon_raw.contains("path"),
        "TOON should carry a labeled results table with name + path columns:\n{toon_raw}"
    );
    for hit in &json_results {
        let name = hit.get("name").and_then(Value::as_str).expect("hit name");
        let path = hit.get("path").and_then(Value::as_str).expect("hit path");
        assert!(
            toon_raw.contains(name) && toon_raw.contains(path),
            "TOON body should round-trip hit ({path}, {name}):\n{toon_raw}"
        );
    }

    let body = decode_text(
        &service
            .call_tool(call_params(
                "recent_changes",
                json!({ "limit": 5, "include_files": true }),
            ))
            .await
            .expect("recent_changes"),
    );
    let commits = body.get("commits").and_then(Value::as_array).expect("commits");
    assert_eq!(commits.len(), 2, "two commits expected");
    assert!(
        body.get("truncated").is_none() || body.get("truncated") == Some(&Value::Bool(false)),
        "non-shallow clone should not surface truncated=true"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "search_git_history",
                json!({ "pattern": "tweak", "field": "message", "limit": 10 }),
            ))
            .await
            .expect("search_git_history"),
    );
    let hits = body.get("commits").and_then(Value::as_array).expect("commits");
    assert_eq!(hits.len(), 1, "one commit summary contains 'tweak', got {hits:?}");
    assert!(
        hits[0]
            .get("summary")
            .and_then(Value::as_str)
            .is_some_and(|s| s.contains("tweak")),
        "hit summary should contain the query token"
    );
    let body = decode_text(
        &service
            .call_tool(call_params(
                "search_git_history",
                json!({ "pattern": "tweak", "field": "author" }),
            ))
            .await
            .expect("search_git_history author scope"),
    );
    assert_eq!(
        body.get("commits").and_then(Value::as_array).map(Vec::len),
        Some(0),
        "'tweak' is a message token, not an author token"
    );
    let body = decode_text(
        &service
            .call_tool(call_params(
                "search_git_history",
                json!({ "pattern": "tweak alpha", "field": "all" }),
            ))
            .await
            .expect("search_git_history AND same-commit"),
    );
    assert_eq!(
        body.get("commits").and_then(Value::as_array).map(Vec::len),
        Some(1),
        "'tweak' AND 'alpha' both in the 'tweak alpha' commit"
    );
    let body = decode_text(
        &service
            .call_tool(call_params(
                "search_git_history",
                json!({ "pattern": "init tweak", "field": "all" }),
            ))
            .await
            .expect("search_git_history AND cross-commit"),
    );
    assert_eq!(
        body.get("commits").and_then(Value::as_array).map(Vec::len),
        Some(0),
        "'init' (c1) AND 'tweak' (c2) share no commit"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "symbol_history",
                json!({ "path": "a.rs", "name": "alpha", "limit": 10 }),
            ))
            .await
            .expect("symbol_history"),
    );
    let history = body.get("history").and_then(Value::as_array).expect("history");
    let modifieds = history
        .iter()
        .filter(|e| e.get("change").and_then(Value::as_str) == Some("modified"))
        .count();
    assert!(
        modifieds >= 1,
        "expected ≥ 1 'modified' entry for alpha after the tweak commit: {history:?}"
    );
    assert_eq!(
        body.get("hash_mode").and_then(Value::as_str),
        Some("normalized"),
        "default hash_mode echo should be normalized"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "symbol_history",
                json!({ "path": "a.rs", "name": "alpha", "limit": 10, "hash_mode": "structural" }),
            ))
            .await
            .expect("symbol_history(structural)"),
    );
    assert_eq!(
        body.get("hash_mode").and_then(Value::as_str),
        Some("structural"),
        "structural hash_mode should be echoed back to the caller"
    );
    let history = body.get("history").and_then(Value::as_array).expect("history");
    let modifieds = history
        .iter()
        .filter(|e| e.get("change").and_then(Value::as_str) == Some("modified"))
        .count();
    assert!(
        modifieds >= 1,
        "structural mode should also see the 'tweak alpha' literal change: {history:?}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("find_references", json!({ "name": "alpha", "limit": 100 })))
            .await
            .expect("find_references"),
    );
    let hits = body.get("hits").and_then(Value::as_array).expect("hits");
    assert_eq!(hits.len(), 3, "expected 3 alpha() call sites: {body}");
    assert!(
        hits.iter()
            .all(|h| h.get("callee").and_then(Value::as_str) == Some("alpha")),
        "every hit should carry callee=\"alpha\""
    );
    assert!(
        hits.iter()
            .all(|h| h.get("line").and_then(Value::as_u64).unwrap_or(0) >= 1),
        "every hit should carry a 1-based line number"
    );
    assert!(
        hits.iter()
            .all(|h| h.get("path").and_then(Value::as_str) == Some("c.rs")),
        "every alpha() call site lives in c.rs in this fixture"
    );

    let page1 = decode_text(
        &service
            .call_tool(call_params("find_references", json!({ "name": "alpha", "limit": 2 })))
            .await
            .expect("find_references page1"),
    );
    let page1_hits = page1.get("hits").and_then(Value::as_array).expect("page1 hits");
    assert_eq!(page1_hits.len(), 2, "limit=2 → 2 hits on first page");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("first page must carry a next_cursor when more remain")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "find_references",
                json!({ "name": "alpha", "limit": 2, "cursor": cursor1 }),
            ))
            .await
            .expect("find_references page2"),
    );
    let page2_hits = page2.get("hits").and_then(Value::as_array).expect("page2 hits");
    assert_eq!(page2_hits.len(), 1, "remaining single hit on second page");
    assert!(
        page2.get("next_cursor").is_none(),
        "second page must NOT carry a next_cursor: {page2}"
    );
    let pos = |h: &Value| -> (u64, u64) {
        (
            h.get("line").and_then(Value::as_u64).unwrap_or(0),
            h.get("column").and_then(Value::as_u64).unwrap_or(0),
        )
    };
    let p1_pos: Vec<(u64, u64)> = page1_hits.iter().map(pos).collect();
    let p2_pos: Vec<(u64, u64)> = page2_hits.iter().map(pos).collect();
    assert!(
        p2_pos.iter().all(|p| !p1_pos.contains(p)),
        "page2 must not overlap page1: {p1_pos:?} vs {p2_pos:?}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("find_callers", json!({ "path": "a.rs", "name": "alpha" })))
            .await
            .expect("find_callers"),
    );
    let def = body.get("definition").expect("definition echoed");
    assert_eq!(
        def.get("name").and_then(Value::as_str),
        Some("alpha"),
        "definition should resolve to alpha"
    );
    let hits = body.get("hits").and_then(Value::as_array).expect("hits");
    assert_eq!(hits.len(), 3, "find_callers should see the same 3 sites");

    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "find_callers",
                json!({ "path": "a.rs", "name": "alpha", "limit": 2 }),
            ))
            .await
            .expect("find_callers page1"),
    );
    let page1_hits = page1.get("hits").and_then(Value::as_array).expect("page1 hits");
    assert_eq!(page1_hits.len(), 2, "find_callers limit=2 → 2 hits");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("find_callers first page must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "find_callers",
                json!({
                    "path": "a.rs",
                    "name": "alpha",
                    "limit": 2,
                    "cursor": cursor1,
                }),
            ))
            .await
            .expect("find_callers page2"),
    );
    let page2_hits = page2.get("hits").and_then(Value::as_array).expect("page2 hits");
    assert_eq!(page2_hits.len(), 1, "find_callers tail page → 1 hit");
    assert!(
        page2.get("next_cursor").is_none(),
        "find_callers second page must NOT have next_cursor: {page2}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "goto_definition",
                json!({ "path": "a.rs", "line": 1, "column": 0 }),
            ))
            .await
            .expect("goto_definition"),
    );
    assert_eq!(
        body.get("path").and_then(Value::as_str),
        Some("a.rs"),
        "goto_definition must echo the queried path: {body}"
    );
    assert_eq!(
        body.get("line").and_then(Value::as_u64),
        Some(1),
        "goto_definition must echo the normalized 1-based line: {body}"
    );
    if let Some(def) = body.get("definition").filter(|d| !d.is_null()) {
        assert!(
            def.get("path").and_then(Value::as_str).is_some(),
            "resolved definition must carry a path: {body}"
        );
        assert!(
            def.get("line").and_then(Value::as_u64).unwrap_or(0) >= 1,
            "resolved definition must carry a 1-based line: {body}"
        );
    }

    let body = decode_text(
        &service
            .call_tool(call_params(
                "call_graph",
                json!({ "name": "inner", "direction": "callers", "max_depth": 2 }),
            ))
            .await
            .expect("call_graph callers"),
    );
    let nodes = body.get("nodes").and_then(Value::as_array).expect("nodes");
    let names: Vec<String> = nodes
        .iter()
        .filter_map(|n| n.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    assert!(
        names.contains(&"inner".to_string()),
        "call_graph callers must surface root `inner`: {names:?}"
    );
    assert!(
        names.contains(&"middle".to_string()),
        "call_graph callers must surface depth-1 `middle`: {names:?}"
    );
    assert!(
        names.contains(&"outer".to_string()),
        "call_graph callers must surface depth-2 `outer`: {names:?}"
    );
    assert_eq!(
        nodes[0].get("name").and_then(Value::as_str),
        Some("inner"),
        "nodes[0] is the root"
    );
    let middle_idx = nodes
        .iter()
        .position(|n| n.get("name").and_then(Value::as_str) == Some("middle"))
        .expect("middle node present");
    let middle_edges: Vec<u64> = nodes[middle_idx]
        .get("edges_to")
        .and_then(Value::as_array)
        .expect("middle.edges_to")
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    assert!(
        middle_edges.contains(&0),
        "middle.edges_to should reference the root inner (index 0): got {middle_edges:?}"
    );
    let outer_idx = nodes
        .iter()
        .position(|n| n.get("name").and_then(Value::as_str) == Some("outer"))
        .expect("outer node present");
    let outer_edges: Vec<u64> = nodes[outer_idx]
        .get("edges_to")
        .and_then(Value::as_array)
        .expect("outer.edges_to")
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    assert!(
        outer_edges.contains(&(middle_idx as u64)),
        "outer.edges_to should reference middle (index {middle_idx}): got {outer_edges:?}"
    );

    let sym = decode_text(
        &service
            .call_tool(call_params(
                "architecture_map",
                json!({ "granularity": "symbol", "include_churn": false }),
            ))
            .await
            .expect("architecture_map symbol"),
    );
    let sym_nodes = sym.get("nodes").and_then(Value::as_array).expect("symbol nodes");
    assert!(!sym_nodes.is_empty(), "symbol tier returns hub functions: {sym:?}");
    assert_eq!(
        sym_nodes[0].get("name").and_then(Value::as_str),
        Some("alpha"),
        "alpha (hub 3) must outrank the higher-fan-in but multiply-defined `zed` (hub 2): {sym:?}"
    );
    assert_eq!(
        sym_nodes[0].get("fan_in").and_then(Value::as_u64),
        Some(3),
        "the raw fan_in count is still reported verbatim on the node: {sym:?}"
    );
    assert!(
        sym_nodes
            .iter()
            .any(|n| n.get("name").and_then(Value::as_str) == Some("zed"))
            && sym_nodes
                .iter()
                .find(|n| n.get("name").and_then(Value::as_str) == Some("zed"))
                .and_then(|n| n.get("fan_in").and_then(Value::as_u64))
                == Some(4),
        "the `zed` decoy is present with its honest raw fan_in=4, just not ranked first: {sym:?}"
    );
    let sym_scores: Vec<f64> = sym_nodes
        .iter()
        .filter_map(|n| n.get("score").and_then(Value::as_f64))
        .collect();
    assert!(
        sym_scores.windows(2).all(|w| w[0] >= w[1]),
        "symbol-tier nodes must be emitted in non-increasing score order: {sym_scores:?}"
    );

    let filem = decode_text(
        &service
            .call_tool(call_params(
                "architecture_map",
                json!({ "granularity": "file", "include_churn": false }),
            ))
            .await
            .expect("architecture_map file"),
    );
    let file_nodes = filem.get("nodes").and_then(Value::as_array).expect("file nodes");
    let cycles = filem.get("cycles").and_then(Value::as_array).expect("cycles");
    assert!(
        !cycles.is_empty(),
        "file tier must surface the cyc1<->cyc2 cycle: {filem:?}"
    );
    let label_by_id = |id: u64| -> String {
        file_nodes
            .iter()
            .find(|n| n.get("id").and_then(Value::as_u64) == Some(id))
            .and_then(|n| n.get("label").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string()
    };
    let cycle_labels: Vec<String> = cycles
        .iter()
        .flat_map(|c| c.get("members").and_then(Value::as_array).expect("members").iter())
        .filter_map(Value::as_u64)
        .map(label_by_id)
        .collect();
    assert!(
        cycle_labels.contains(&"cyc1.rs".to_string()) && cycle_labels.contains(&"cyc2.rs".to_string()),
        "cycle members must include cyc1.rs + cyc2.rs: {cycle_labels:?}"
    );

    let filem2 = decode_text(
        &service
            .call_tool(call_params(
                "architecture_map",
                json!({ "granularity": "file", "include_churn": false }),
            ))
            .await
            .expect("architecture_map file (repeat)"),
    );
    assert_eq!(
        filem.get("nodes"),
        filem2.get("nodes"),
        "architecture_map file-tier node order must be deterministic across calls"
    );

    let page1 = decode_text(
        &service
            .call_tool(call_params("search_symbols", json!({ "needle": "a", "limit": 1 })))
            .await
            .expect("search_symbols page1"),
    );
    let page1_results = page1.get("results").and_then(Value::as_array).expect("page1 results");
    assert_eq!(page1_results.len(), 1, "search_symbols limit=1 → 1 result");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("first page must carry next_cursor when more remain")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "search_symbols",
                json!({ "needle": "a", "limit": 1, "cursor": cursor1 }),
            ))
            .await
            .expect("search_symbols page2"),
    );
    let page2_results = page2.get("results").and_then(Value::as_array).expect("page2 results");
    assert_eq!(page2_results.len(), 1, "page2 must also have 1 result");
    let key1 = (
        page1_results[0].get("path").and_then(Value::as_str).unwrap_or(""),
        page1_results[0].get("name").and_then(Value::as_str).unwrap_or(""),
    );
    let key2 = (
        page2_results[0].get("path").and_then(Value::as_str).unwrap_or(""),
        page2_results[0].get("name").and_then(Value::as_str).unwrap_or(""),
    );
    assert_ne!(key1, key2, "page2 must not repeat page1's entry");

    let unbudgeted = decode_text(
        &service
            .call_tool(call_params("search_symbols", json!({ "needle": "a", "limit": 100 })))
            .await
            .expect("search_symbols unbudgeted"),
    );
    let unbudgeted_len = unbudgeted
        .get("results")
        .and_then(Value::as_array)
        .expect("unbudgeted results")
        .len();
    assert!(
        unbudgeted_len >= 2,
        "fixture must have ≥2 'a' symbols to exercise budgeting, got {unbudgeted_len}"
    );
    let budgeted = decode_text(
        &service
            .call_tool(call_params(
                "search_symbols",
                json!({ "needle": "a", "limit": 100, "max_tokens": 1 }),
            ))
            .await
            .expect("search_symbols budgeted"),
    );
    let budgeted_results = budgeted
        .get("results")
        .and_then(Value::as_array)
        .expect("budgeted results");
    assert_eq!(
        budgeted_results.len(),
        1,
        "max_tokens=1 keeps exactly the first hit: {budgeted}"
    );
    assert!(
        budgeted_results.len() < unbudgeted_len,
        "budgeted page must be smaller than the unbudgeted page ({} < {unbudgeted_len})",
        budgeted_results.len()
    );
    assert_eq!(
        budgeted.get("budgeted").and_then(Value::as_bool),
        Some(true),
        "budgeted response must set budgeted=true: {budgeted}"
    );
    assert!(
        budgeted.get("next_cursor").and_then(Value::as_str).is_some(),
        "budgeted response must carry a non-null next_cursor: {budgeted}"
    );

    let page1 = decode_text(
        &service
            .call_tool(call_params("list_files", json!({ "limit": 4 })))
            .await
            .expect("list_files page1"),
    );
    let page1_files = page1.get("files").and_then(Value::as_array).expect("page1 files");
    assert_eq!(page1_files.len(), 4, "list_files limit=4 → 4 files");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("list_files first page must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params("list_files", json!({ "limit": 4, "cursor": cursor1 })))
            .await
            .expect("list_files page2"),
    );
    let page2_files = page2.get("files").and_then(Value::as_array).expect("page2 files");
    assert_eq!(page2_files.len(), 3, "list_files page2 → 3 remaining files: {page2}");
    assert!(
        page2.get("next_cursor").is_none(),
        "list_files page2 must NOT carry next_cursor"
    );
    let p1_paths: Vec<&str> = page1_files
        .iter()
        .filter_map(|f| f.get("path").and_then(Value::as_str))
        .collect();
    let p2_paths: Vec<&str> = page2_files
        .iter()
        .filter_map(|f| f.get("path").and_then(Value::as_str))
        .collect();
    assert!(
        p2_paths.iter().all(|p| !p1_paths.contains(p)),
        "list_files pages must not overlap: {p1_paths:?} vs {p2_paths:?}"
    );

    let page1 = decode_text(
        &service
            .call_tool(call_params("search_symbols", json!({ "needle": "a", "limit": 1 })))
            .await
            .expect("search_symbols pre-rescan"),
    );
    let stale_cursor = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("pre-rescan cursor")
        .to_string();
    let _ = service
        .call_tool(call_params("rescan", json!({})))
        .await
        .expect("rescan");
    let stale_response = decode_text(
        &service
            .call_tool(call_params(
                "search_symbols",
                json!({ "needle": "a", "limit": 1, "cursor": stale_cursor }),
            ))
            .await
            .expect("search_symbols with stale cursor"),
    );
    assert_eq!(
        stale_response.get("cursor_invalidated"),
        Some(&Value::Bool(true)),
        "rescan must invalidate in-memory search_symbols cursors: {stale_response}"
    );

    let page1 = decode_text(
        &service
            .call_tool(call_params("list_files", json!({ "limit": 1 })))
            .await
            .expect("list_files pre-rescan"),
    );
    let stale_cursor = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("list_files pre-rescan cursor")
        .to_string();
    let _ = service
        .call_tool(call_params("rescan", json!({})))
        .await
        .expect("rescan");
    let stale_response = decode_text(
        &service
            .call_tool(call_params("list_files", json!({ "limit": 1, "cursor": stale_cursor })))
            .await
            .expect("list_files with stale cursor"),
    );
    assert_eq!(
        stale_response.get("cursor_invalidated"),
        Some(&Value::Bool(true)),
        "rescan must invalidate in-memory list_files cursors: {stale_response}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("find_files", json!({ "query": "cy1" })))
            .await
            .expect("find_files(cy1)"),
    );
    let files = body.get("files").and_then(Value::as_array).expect("find_files files");
    assert_eq!(files.len(), 1, "'cy1' is a subsequence of only cyc1.rs: {body}");
    assert_eq!(
        files[0].get("path").and_then(Value::as_str),
        Some("cyc1.rs"),
        "find_files('cy1') must rank cyc1.rs: {body}"
    );
    assert!(
        files[0].get("score").and_then(Value::as_u64).is_some(),
        "find_files entries must carry a numeric score: {body}"
    );
    assert_eq!(
        body.get("total").and_then(Value::as_u64),
        Some(1),
        "find_files('cy1') total must be exactly 1: {body}"
    );
    assert_eq!(
        body.get("returned").and_then(Value::as_u64),
        Some(1),
        "find_files('cy1') returned must be exactly 1: {body}"
    );
    assert_eq!(
        body.get("truncated").and_then(Value::as_bool),
        Some(false),
        "find_files('cy1') must not be truncated: {body}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("find_files", json!({ "query": "zzzznonexistentqueryxyz" })))
            .await
            .expect("find_files(no match)"),
    );
    let files = body.get("files").and_then(Value::as_array).expect("find_files files");
    assert!(
        files.is_empty(),
        "a query with no subsequence match should return no files: {body}"
    );
    assert_eq!(
        body.get("total").and_then(Value::as_u64),
        Some(0),
        "find_files(no match) total must be 0: {body}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "find_files",
                json!({ "query": "rs", "language": "python" }),
            ))
            .await
            .expect("find_files(language filter)"),
    );
    let files = body.get("files").and_then(Value::as_array).expect("find_files files");
    assert!(
        files.is_empty(),
        "language=python filter must exclude every .rs match: {body}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("find_files", json!({ "query": "d" })))
            .await
            .expect("find_files(d)"),
    );
    let files = body.get("files").and_then(Value::as_array).expect("find_files files");
    assert_eq!(
        files.first().and_then(|f| f.get("path")).and_then(Value::as_str),
        Some("d.py"),
        "find_files('d') should rank d.py first (only path starting with d): {body}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "find_references",
                json!({ "name": "no_such_callee_anywhere" }),
            ))
            .await
            .expect("find_references(missing)"),
    );
    let hits = body.get("hits").and_then(Value::as_array).expect("hits");
    assert!(hits.is_empty(), "unknown callee should yield no hits");

    let body = decode_text(
        &service
            .call_tool(call_params("blame_file", json!({ "path": "a.rs" })))
            .await
            .expect("blame_file"),
    );
    let hunks = body.get("hunks").and_then(Value::as_array).expect("hunks");
    assert!(!hunks.is_empty(), "blame should return hunks on a real file");

    let body = decode_text(
        &service
            .call_tool(call_params(
                "workspace_grep",
                json!({ "pattern": "pub fn", "include_context": false }),
            ))
            .await
            .expect("workspace_grep"),
    );
    let grep_hits = body.get("hits").and_then(Value::as_array).expect("hits");
    assert!(
        !grep_hits.is_empty(),
        "workspace_grep for 'pub fn' should find hits in the fixture"
    );
    assert!(
        grep_hits
            .iter()
            .all(|h| h.get("line_num").and_then(Value::as_u64).unwrap_or(0) >= 1),
        "every grep hit must carry a 1-based line_num"
    );
    let total_matches = body
        .get("total_matches")
        .and_then(Value::as_u64)
        .expect("total_matches");
    assert!(
        total_matches >= 3,
        "fixture has alpha + doit + caller = 3+ 'pub fn' occurrences, got {total_matches}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "workspace_grep",
                json!({ "pattern": "pub fn", "limit": 1, "include_context": false }),
            ))
            .await
            .expect("workspace_grep(limit=1)"),
    );
    let truncated = body.get("truncated").and_then(Value::as_bool).unwrap_or(false);
    let hits_with_limit = body.get("hits").and_then(Value::as_array).expect("hits");
    assert_eq!(hits_with_limit.len(), 1, "limit=1 should return exactly 1 hit");
    assert!(truncated, "limit=1 with multiple matches should set truncated=true");
    assert_eq!(
        body.get("truncation_reason").and_then(Value::as_str),
        Some("limit"),
        "a truncated grep must name the bound that cut it: {body}"
    );
    let capped_total = body
        .get("total_matches")
        .and_then(Value::as_u64)
        .expect("total_matches");
    assert_eq!(
        capped_total, total_matches,
        "`limit` caps hits, never the scan: the match count is the same as the unlimited call"
    );

    let invalid_result = service
        .call_tool(call_params("workspace_grep", json!({ "pattern": "[invalid_regex(" })))
        .await;
    assert!(
        invalid_result.is_err(),
        "invalid regex should produce a protocol-level MCP error"
    );

    let _ = service
        .call_tool(call_params(
            "memory_put",
            json!({ "key": "smoke_key", "value": "hello", "embed": false }),
        ))
        .await;
    let _ = service
        .call_tool(call_params("memory_get", json!({ "key": "smoke_key" })))
        .await;
    let _ = service.call_tool(call_params("memory_list", json!({}))).await;
    let _ = service
        .call_tool(call_params("memory_delete", json!({ "key": "smoke_key" })))
        .await;
    let _ = service
        .call_tool(call_params("search_documents", json!({ "query": "hello" })))
        .await;

    #[cfg(not(feature = "code-search"))]
    {
        let sc = service
            .call_tool(call_params("search_code", json!({ "query": "hello" })))
            .await;
        assert!(
            sc.is_err(),
            "search_code without the code-search feature must return an MCP error"
        );
        let gc = service
            .call_tool(call_params("get_chunk", json!({ "path": "src/lib.rs" })))
            .await;
        assert!(
            gc.is_err(),
            "get_chunk without the code-search feature must return an MCP error"
        );
    }
    #[cfg(feature = "code-search")]
    {
        let sc = service
            .call_tool(call_params("search_code", json!({ "query": "hello" })))
            .await;
        if let Ok(result) = &sc {
            let body = decode_text(result);
            assert_eq!(
                body.get("query").and_then(Value::as_str),
                Some("hello"),
                "search_code must echo the input query field: {body}"
            );
            assert!(
                body.get("hits").and_then(Value::as_array).is_some(),
                "search_code response must carry a hits array (may be empty): {body}"
            );
        }

        let gc = service
            .call_tool(call_params("get_chunk", json!({ "path": "a.rs" })))
            .await;
        if let Ok(result) = &gc {
            let body = decode_text(result);
            assert!(
                body.get("path").is_some() && body.get("text").is_some(),
                "get_chunk success response must carry path + text: {body}"
            );
        }

        let kw = service
            .call_tool(call_params(
                "search_code",
                json!({ "query": "hello", "mode": "keyword" }),
            ))
            .await;
        if let Ok(result) = &kw {
            let body = decode_text(result);
            assert_eq!(
                body.get("query").and_then(Value::as_str),
                Some("hello"),
                "keyword search_code must echo the query field: {body}"
            );
            assert!(
                body.get("hits").and_then(Value::as_array).is_some(),
                "keyword search_code response must carry a hits array: {body}"
            );
        }

        let bad_mode = service
            .call_tool(call_params("search_code", json!({ "query": "hello", "mode": "bogus" })))
            .await;
        assert!(
            bad_mode.is_err(),
            "search_code must reject an unknown mode with an MCP error"
        );

        let hy = service
            .call_tool(call_params(
                "search_code",
                json!({ "query": "hello", "mode": "hybrid", "rerank": false, "rerank_preset": "bge-reranker-base" }),
            ))
            .await;
        if let Ok(result) = &hy {
            let body = decode_text(result);
            assert_eq!(
                body.get("query").and_then(Value::as_str),
                Some("hello"),
                "hybrid search_code must echo the query field: {body}"
            );
            assert!(
                body.get("hits").and_then(Value::as_array).is_some(),
                "hybrid search_code response must carry a hits array: {body}"
            );
        }
    }

    let override_result = service
        .call_tool(call_params(
            "search_documents",
            json!({ "query": "hello", "reranker_preset": "bge-reranker-base" }),
        ))
        .await;
    if let Ok(r) = &override_result {
        let _ = r;
    }

    #[cfg(feature = "documents")]
    {
        let json_result = service
            .call_tool(call_params("search_documents", json!({ "query": "hello" })))
            .await;
        let toon_result = service
            .call_tool(call_params(
                "search_documents",
                json!({ "query": "hello", "output_format": "toon" }),
            ))
            .await;
        if let (Ok(json_resp), Ok(toon_resp)) = (&json_result, &toon_result) {
            let json_body = decode_text(json_resp);
            if json_body != Value::Null {
                let toon_raw = toon_resp
                    .content
                    .iter()
                    .find_map(|c| match c {
                        rmcp::model::ContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let toon_body: Value = serde_toon::from_str(&toon_raw).expect("toon body deserializes to JSON value");
                assert_eq!(
                    json_body.get("query"),
                    toon_body.get("query"),
                    "TOON and JSON responses must echo the same query field"
                );
                let json_hits = json_body
                    .get("hits")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                let toon_hits = toon_body
                    .get("hits")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                assert_eq!(
                    json_hits, toon_hits,
                    "TOON and JSON responses must carry the same hit count"
                );
            }
        }
    }
    #[cfg(not(feature = "documents"))]
    {
        let _ = service
            .call_tool(call_params(
                "search_documents",
                json!({ "query": "hello", "output_format": "toon" }),
            ))
            .await;
    }

    #[cfg(feature = "memory")]
    {
        for i in 0..3 {
            let _ = service
                .call_tool(call_params(
                    "memory_put",
                    json!({
                        "key": format!("paging_key_{i}"),
                        "value": format!("v{i}"),
                        "embed": false,
                    }),
                ))
                .await
                .expect("memory_put");
        }
        let page1 = decode_text(
            &service
                .call_tool(call_params(
                    "memory_list",
                    json!({ "prefix": "paging_key_", "limit": 2 }),
                ))
                .await
                .expect("memory_list page1"),
        );
        let page1_entries = page1.get("entries").and_then(Value::as_array).expect("page1 entries");
        assert_eq!(page1_entries.len(), 2, "memory_list limit=2 → 2 entries");
        let cursor1 = page1
            .get("next_cursor")
            .and_then(Value::as_str)
            .expect("memory_list first page must carry next_cursor")
            .to_string();
        let page2 = decode_text(
            &service
                .call_tool(call_params(
                    "memory_list",
                    json!({
                        "prefix": "paging_key_",
                        "limit": 2,
                        "cursor": cursor1,
                    }),
                ))
                .await
                .expect("memory_list page2"),
        );
        let page2_entries = page2.get("entries").and_then(Value::as_array).expect("page2 entries");
        assert_eq!(page2_entries.len(), 1, "memory_list page2 → 1 remaining");
        assert!(
            page2.get("next_cursor").is_none(),
            "memory_list page2 must NOT carry next_cursor: {page2}"
        );
    }

    #[cfg(feature = "memory")]
    {
        let _ = service
            .call_tool(call_params(
                "memory_put",
                json!({
                    "key": "audit_probe",
                    "value": "a memory note with no code refs",
                    "embed": false,
                }),
            ))
            .await
            .expect("memory_put audit_probe");

        let body = decode_text(
            &service
                .call_tool(call_params("memory_audit", json!({ "key": "audit_probe" })))
                .await
                .expect("memory_audit single-key"),
        );
        assert_eq!(
            body.get("audited").and_then(Value::as_u64),
            Some(1),
            "memory_audit single-key must report audited=1: {body}"
        );
        let results = body.get("results").and_then(Value::as_array).expect("results");
        assert_eq!(results.len(), 1, "single-key audit must return one result");
        assert_eq!(
            results[0].get("state").and_then(Value::as_str),
            Some("unverified"),
            "empty-provenance memory must audit as unverified: {results:?}"
        );

        let dry_body = decode_text(
            &service
                .call_tool(call_params(
                    "memory_audit",
                    json!({ "key": "audit_probe", "dry_run": true }),
                ))
                .await
                .expect("memory_audit dry_run"),
        );
        assert_eq!(
            dry_body.get("audited").and_then(Value::as_u64),
            Some(1),
            "dry_run audit must still report audited=1: {dry_body}"
        );

        let range_body = decode_text(
            &service
                .call_tool(call_params("memory_audit", json!({ "limit": 50 })))
                .await
                .expect("memory_audit range"),
        );
        let range_audited = range_body.get("audited").and_then(Value::as_u64).expect("audited");
        assert!(
            range_audited >= 1,
            "range audit must cover at least the audit_probe key: {range_body}"
        );

        let _ = service
            .call_tool(call_params("memory_delete", json!({ "key": "audit_probe" })))
            .await
            .expect("memory_delete audit_probe");
    }

    #[cfg(feature = "memory")]
    {
        let mine_body = decode_text(
            &service
                .call_tool(call_params("proposals_mine", json!({})))
                .await
                .expect("proposals_mine default"),
        );
        assert!(
            mine_body.get("mined").and_then(Value::as_u64).is_some(),
            "proposals_mine must return `mined` field: {mine_body}"
        );
        assert_eq!(
            mine_body.get("window_inspected").and_then(Value::as_u64),
            Some(200),
            "proposals_mine must echo window_inspected=200 (default): {mine_body}"
        );
        assert!(
            mine_body.get("skipped_bulk").and_then(Value::as_u64).is_some(),
            "proposals_mine must return `skipped_bulk` field: {mine_body}"
        );

        let list_body = decode_text(
            &service
                .call_tool(call_params("proposals_list", json!({ "kind": "skill", "limit": 50 })))
                .await
                .expect("proposals_list after default mine"),
        );
        assert_eq!(
            list_body.get("total").and_then(Value::as_u64),
            Some(0),
            "proposals_list must return total=0 after a no-candidate mine: {list_body}"
        );
        assert_eq!(
            list_body.get("truncated").and_then(Value::as_bool),
            Some(false),
            "proposals_list must return truncated=false for an empty list: {list_body}"
        );
        assert!(
            list_body.get("proposals").and_then(Value::as_array).map(Vec::is_empty) == Some(true),
            "proposals array must be empty: {list_body}"
        );

        let mine_low = decode_text(
            &service
                .call_tool(call_params(
                    "proposals_mine",
                    json!({
                        "min_support": 1,
                        "min_confidence": 0.1,
                        "max_files_per_commit": 10,
                        "window": 50,
                    }),
                ))
                .await
                .expect("proposals_mine min_support=1"),
        );
        let mined_low = mine_low.get("mined").and_then(Value::as_u64).unwrap_or(0);
        assert!(
            mined_low >= 1,
            "proposals_mine(min_support=1) must mine the fixture's co-change cluster: {mine_low}"
        );

        let list2 = decode_text(
            &service
                .call_tool(call_params("proposals_list", json!({ "limit": 10 })))
                .await
                .expect("proposals_list after low-threshold mine"),
        );
        let proposals = list2
            .get("proposals")
            .and_then(Value::as_array)
            .expect("proposals array");
        assert_eq!(
            proposals.len() as u64,
            mined_low,
            "proposals_list count must match mined count: {list2}"
        );
        let accept_id = proposals[0]
            .get("id")
            .and_then(Value::as_str)
            .expect("accept id")
            .to_string();
        let accept_files: Vec<String> = proposals[0]
            .get("files")
            .and_then(Value::as_array)
            .expect("proposal files")
            .iter()
            .filter_map(|f| f.as_str().map(String::from))
            .collect();
        assert!(
            !accept_files.is_empty(),
            "a co-change proposal must carry at least one file: {list2}"
        );

        let accept_body = decode_text(
            &service
                .call_tool(call_params("proposal_accept", json!({ "id": accept_id })))
                .await
                .expect("proposal_accept"),
        );
        assert_eq!(
            accept_body.get("accepted").and_then(Value::as_bool),
            Some(true),
            "proposal_accept must return accepted=true: {accept_body}"
        );
        let memory_key = accept_body
            .get("memory_key")
            .and_then(Value::as_str)
            .expect("memory_key from proposal_accept")
            .to_string();
        assert!(
            memory_key.starts_with("skill/cochange-"),
            "auto-derived key must start with skill/cochange-: {memory_key}"
        );

        let audit_live = decode_text(
            &service
                .call_tool(call_params("memory_audit", json!({ "key": &memory_key })))
                .await
                .expect("memory_audit after accept"),
        );
        let live_results = audit_live
            .get("results")
            .and_then(Value::as_array)
            .expect("live audit results");
        assert_eq!(
            live_results.len(),
            1,
            "memory_audit must return one result for the accepted key: {audit_live}"
        );
        assert_eq!(
            live_results[0].get("state").and_then(Value::as_str),
            Some("verified"),
            "freshly accepted skill (all files present) must audit as verified: {audit_live}"
        );

        let probe_file = accept_files[0].clone();
        let probe_abs = root.join(&probe_file);
        let saved = std::fs::read(&probe_abs).expect("read probe file before delete");
        std::fs::remove_file(&probe_abs).expect("remove probe file");
        let _ = service
            .call_tool(call_params("rescan", json!({})))
            .await
            .expect("rescan after file deletion");
        let stale_audit = decode_text(
            &service
                .call_tool(call_params(
                    "memory_audit",
                    json!({ "key": &memory_key, "dry_run": true }),
                ))
                .await
                .expect("memory_audit stale wedge"),
        );
        let stale_results = stale_audit
            .get("results")
            .and_then(Value::as_array)
            .expect("stale audit results");
        assert_eq!(stale_results.len(), 1, "stale audit must have one result");
        assert_eq!(
            stale_results[0].get("state").and_then(Value::as_str),
            Some("stale"),
            "memory_audit must return state=stale after a referenced file is deleted: \
             {stale_results:?} (file: {probe_file})"
        );

        std::fs::write(&probe_abs, &saved).expect("restore probe file");
        let _ = service
            .call_tool(call_params("rescan", json!({})))
            .await
            .expect("rescan after restore");
        let _ = service
            .call_tool(call_params("memory_delete", json!({ "key": &memory_key })))
            .await;

        let mine_e = decode_text(
            &service
                .call_tool(call_params(
                    "proposals_mine",
                    json!({
                        "min_support": 1,
                        "min_confidence": 0.1,
                        "max_files_per_commit": 10,
                        "window": 50,
                    }),
                ))
                .await
                .expect("proposals_mine for reject test"),
        );
        let mined_e = mine_e.get("mined").and_then(Value::as_u64).unwrap_or(0);
        assert!(
            mined_e >= 1,
            "re-mine must regenerate the cluster (git history is immutable): {mine_e}"
        );
        let list_e = decode_text(
            &service
                .call_tool(call_params("proposals_list", json!({ "limit": 10 })))
                .await
                .expect("proposals_list for reject"),
        );
        let reject_id = list_e["proposals"][0]
            .get("id")
            .and_then(Value::as_str)
            .expect("reject id")
            .to_string();
        let reject_body = decode_text(
            &service
                .call_tool(call_params(
                    "proposal_reject",
                    json!({ "id": reject_id, "reason": "smoke-test rejection" }),
                ))
                .await
                .expect("proposal_reject"),
        );
        assert_eq!(
            reject_body.get("rejected").and_then(Value::as_bool),
            Some(true),
            "proposal_reject must return rejected=true: {reject_body}"
        );
        let mine_after = decode_text(
            &service
                .call_tool(call_params(
                    "proposals_mine",
                    json!({
                        "min_support": 1,
                        "min_confidence": 0.1,
                        "max_files_per_commit": 10,
                        "window": 50,
                    }),
                ))
                .await
                .expect("proposals_mine after reject"),
        );
        let mined_after = mine_after.get("mined").and_then(Value::as_u64).unwrap_or(0);
        assert!(
            mined_after < mined_e,
            "re-mine after reject must produce fewer candidates (tombstone suppressed): \
             mined_after={mined_after} mined_e={mined_e}"
        );
    }

    let body = decode_text(
        &service
            .call_tool(call_params("rescan", json!({})))
            .await
            .expect("rescan"),
    );
    let scanned = body.get("scanned").and_then(Value::as_u64).expect("scanned");
    assert!(scanned > 0, "rescan should walk at least the fixture files");

    let body = decode_text(
        &service
            .call_tool(call_params(
                "rescan",
                json!({ "full": true, "paths": ["does-not-exist.rs"] }),
            ))
            .await
            .expect("rescan full"),
    );
    let scanned_full = body.get("scanned").and_then(Value::as_u64).expect("scanned (full)");
    assert!(
        scanned_full > 0,
        "rescan {{full:true}} must force a full working-tree scan even with a paths scope, \
         got scanned={scanned_full}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("rescan", json!({ "paths": ["a.rs"] })))
            .await
            .expect("rescan scoped"),
    );
    let visited = ["scanned", "updated", "skipped_unchanged"]
        .iter()
        .filter_map(|k| body.get(*k).and_then(Value::as_u64))
        .sum::<u64>();
    assert!(
        visited > 0,
        "scoped rescan {{paths:[a.rs]}} must visit the path (relative paths joined to root), \
         got all-zero report {body}"
    );

    let _ = service
        .call_tool(call_params("search_symbols", json!({ "needle": "Beta", "limit": 5 })))
        .await
        .expect("search_symbols to seed a sub-millisecond telemetry row");

    let body = decode_text(
        &service
            .call_tool(call_params("telemetry_summary", json!({ "window": "all" })))
            .await
            .expect("telemetry_summary"),
    );
    let total_calls = body.get("total_calls").and_then(Value::as_u64).expect("total_calls");
    assert!(
        total_calls >= 4,
        "telemetry_summary should see at least the prior fixture calls (status/outline/search_symbols/recent_changes), got {total_calls}"
    );
    let per_tool = body.get("per_tool").and_then(Value::as_array).expect("per_tool array");
    assert!(!per_tool.is_empty(), "per_tool histogram must not be empty");

    let recent = body.get("recent").and_then(Value::as_array).expect("recent array");
    assert!(!recent.is_empty(), "recent calls must not be empty");
    for call in recent {
        assert!(
            call.get("elapsed_us").and_then(Value::as_u64).is_some(),
            "every telemetry row must carry an `elapsed_us` reading, got {call}"
        );
    }
    const SUB_MS_TOOLS: [&str; 5] = [
        "find_files",
        "find_references",
        "search_symbols",
        "workspace_grep",
        "outline",
    ];
    let sub_ms: Vec<u64> = recent
        .iter()
        .filter(|c| {
            c.get("tool")
                .and_then(Value::as_str)
                .is_some_and(|t| SUB_MS_TOOLS.contains(&t))
        })
        .filter_map(|c| c.get("elapsed_us").and_then(Value::as_u64))
        .collect();
    assert!(
        !sub_ms.is_empty(),
        "fixture must exercise at least one sub-millisecond tool: {recent:?}"
    );
    assert!(
        sub_ms.iter().any(|&us| us > 0),
        "every sub-millisecond tool recorded 0 — telemetry is truncating to milliseconds: {sub_ms:?}"
    );
    let savings_note = body.get("savings_note").and_then(Value::as_str).unwrap_or_default();
    assert!(
        savings_note.contains("estimate") || savings_note.contains("heuristic"),
        "savings_note must disclose the heuristic nature: {savings_note:?}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("cache_stats", json!({})))
            .await
            .expect("cache_stats"),
    );
    let blob_count = body.get("blob_count").and_then(Value::as_u64).expect("blob_count");
    assert!(
        blob_count >= 1,
        "freshly-scanned fixture should have blobs on disk: {body}"
    );
    assert!(
        body.get("blob_accounting_ok").and_then(Value::as_bool).unwrap_or(false),
        "orphan accounting must have run after a clean scan: {body}"
    );
    let per_view = body
        .get("per_view_file_count")
        .and_then(Value::as_array)
        .expect("per_view_file_count array");
    assert!(!per_view.is_empty(), "the working view should be listed: {body}");

    let u = |k: &str| body.get(k).and_then(Value::as_u64).unwrap_or_default();
    let total = u("total_bytes");
    let component_sum = u("blobs_bytes")
        + u("views_bytes")
        + u("lance_bytes")
        + u("git_cache_bytes")
        + u("telemetry_bytes")
        + u("git_history_bytes");
    assert_eq!(
        total,
        component_sum + u("other_bytes"),
        "total_bytes must reconcile to components + other: {body}"
    );
    assert!(
        total >= component_sum,
        "total_bytes must be at least the component sum: {body}"
    );
    assert!(
        body.get("git_history_bytes").and_then(Value::as_u64).is_some(),
        "git_history_bytes field must be present: {body}"
    );
    assert_eq!(
        body.get("blob_accounting_ok").and_then(Value::as_bool),
        Some(true),
        "blob_accounting_ok must be true on a clean scan: {body}"
    );
    if let Some(rss) = body.get("rss_bytes").and_then(Value::as_u64) {
        assert!(rss > 0, "rss_bytes, when reported, is the live server RSS: {body}");
    }

    let body = decode_text(
        &service
            .call_tool(call_params("cache_gc", json!({})))
            .await
            .expect("cache_gc"),
    );
    assert_eq!(
        body.get("removed").and_then(Value::as_u64),
        Some(0),
        "no orphaned blobs to reclaim on a clean scan: {body}"
    );
    assert_eq!(
        body.get("bytes_freed").and_then(Value::as_u64),
        Some(0),
        "zero bytes freed when nothing is orphaned: {body}"
    );
    let scanned = body.get("scanned").and_then(Value::as_u64).expect("scanned");
    assert!(scanned >= 1, "GC should have inspected blob files: {body}");

    let body = decode_text(
        &service
            .call_tool(call_params("cache_clear", json!({ "component": "telemetry" })))
            .await
            .expect("cache_clear(telemetry)"),
    );
    assert_eq!(
        body.get("component").and_then(Value::as_str),
        Some("telemetry"),
        "echoes the cleared component: {body}"
    );
    assert_eq!(
        body.get("cleared").and_then(Value::as_bool),
        Some(true),
        "telemetry clear should succeed: {body}"
    );

    let err = service
        .call_tool(call_params("cache_clear", json!({ "component": "blobs" })))
        .await;
    assert!(
        err.is_err(),
        "clearing `blobs` without confirm=true must be rejected, got: {err:?}"
    );

    for component in ["views", "all"] {
        let err = service
            .call_tool(call_params(
                "cache_clear",
                json!({ "component": component, "confirm": true }),
            ))
            .await;
        assert!(
            err.is_err(),
            "clearing `{component}` in-process must be refused (deletes the live index), got: {err:?}"
        );
    }

    let body = decode_text(
        &service
            .call_tool(call_params(
                "find_implementations",
                json!({ "trait_name": "Drawable", "limit": 100 }),
            ))
            .await
            .expect("find_implementations(Drawable)"),
    );
    let hits = body.get("hits").and_then(Value::as_array).expect("hits");
    let impl_types: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.get("impl_type").and_then(Value::as_str))
        .collect();
    assert!(
        impl_types.contains(&"Beta"),
        "find_implementations(Drawable) must include Beta from a.rs: {impl_types:?}"
    );
    assert!(
        impl_types.contains(&"Rectangle"),
        "find_implementations(Drawable) must include Rectangle from b.ts: {impl_types:?}"
    );
    assert!(
        hits.iter()
            .all(|h| h.get("start_row").and_then(Value::as_u64).unwrap_or(0) >= 1),
        "every find_implementations hit must carry a 1-based start_row"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "find_implementations",
                json!({ "trait_name": "Foo", "limit": 100 }),
            ))
            .await
            .expect("find_implementations(Foo)"),
    );
    let hits = body.get("hits").and_then(Value::as_array).expect("hits");
    let impl_types: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.get("impl_type").and_then(Value::as_str))
        .collect();
    assert!(
        impl_types.contains(&"Bar"),
        "find_implementations(Foo) must include Bar from d.py: {impl_types:?}"
    );

    let impl_page1 = decode_text(
        &service
            .call_tool(call_params(
                "find_implementations",
                json!({ "trait_name": "Drawable", "limit": 1 }),
            ))
            .await
            .expect("find_implementations page1"),
    );
    let impl_page1_hits = impl_page1
        .get("hits")
        .and_then(Value::as_array)
        .expect("impl page1 hits");
    assert_eq!(
        impl_page1_hits.len(),
        1,
        "limit=1 must return exactly 1 implementation hit"
    );
    let impl_cursor1 = impl_page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("find_implementations first page must carry next_cursor when ≥2 implementors exist")
        .to_string();
    let impl_page2 = decode_text(
        &service
            .call_tool(call_params(
                "find_implementations",
                json!({ "trait_name": "Drawable", "limit": 1, "cursor": impl_cursor1 }),
            ))
            .await
            .expect("find_implementations page2"),
    );
    let impl_page2_hits = impl_page2
        .get("hits")
        .and_then(Value::as_array)
        .expect("impl page2 hits");
    assert_eq!(
        impl_page2_hits.len(),
        1,
        "find_implementations page2 must return the remaining hit"
    );
    let impl_key_of = |h: &Value| -> (String, String) {
        (
            h.get("impl_type").and_then(Value::as_str).unwrap_or("").to_string(),
            h.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
        )
    };
    assert_ne!(
        impl_key_of(&impl_page1_hits[0]),
        impl_key_of(&impl_page2_hits[0]),
        "find_implementations pages must not overlap"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "find_implementations",
                json!({ "trait_name": "Drawable", "language": "rust", "limit": 100 }),
            ))
            .await
            .expect("find_implementations(language=rust)"),
    );
    let hits = body.get("hits").and_then(Value::as_array).expect("hits");
    let impl_types: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.get("impl_type").and_then(Value::as_str))
        .collect();
    assert!(
        impl_types.contains(&"Beta"),
        "rust-filtered Drawable must include Beta: {impl_types:?}"
    );
    assert!(
        !impl_types.contains(&"Rectangle"),
        "rust-filtered Drawable must not include Rectangle (TypeScript): {impl_types:?}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("find_references", json!({ "name": "lph", "limit": 100 })))
            .await
            .expect("find_references(substring)"),
    );
    let hits = body.get("hits").and_then(Value::as_array).expect("hits");
    assert_eq!(
        hits.len(),
        3,
        "find_references(\"lph\") must return the 3 alpha() call sites via substring: {body}"
    );
    assert!(
        hits.iter()
            .all(|h| h.get("callee").and_then(Value::as_str) == Some("alpha")),
        "every substring hit must carry the full callee=\"alpha\", not the substring"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "find_implementations",
                json!({ "trait_name": "raw", "limit": 100 }),
            ))
            .await
            .expect("find_implementations(substring)"),
    );
    let hits = body.get("hits").and_then(Value::as_array).expect("hits");
    let impl_types: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.get("impl_type").and_then(Value::as_str))
        .collect();
    assert!(
        impl_types.contains(&"Beta"),
        "find_implementations(\"raw\") must include Beta via substring on \"Drawable\": {impl_types:?}"
    );
    assert!(
        impl_types.contains(&"Rectangle"),
        "find_implementations(\"raw\") must include Rectangle via substring on \"Drawable\": {impl_types:?}"
    );
    assert_eq!(
        body.get("trait_name").and_then(Value::as_str),
        Some("raw"),
        "trait_name in response must echo the search needle"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("search_symbols", json!({ "needle": "", "limit": 100 })))
            .await
            .expect("search_symbols(empty)"),
    );
    let results = body.get("results").and_then(Value::as_array).expect("results");
    assert!(
        results.is_empty(),
        "search_symbols with empty needle must return 0 results, got {results:?}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("compress", json!({ "path": "a.rs" })))
            .await
            .expect("compress(path=a.rs)"),
    );
    assert_eq!(
        body.get("strategy").and_then(Value::as_str),
        Some("structural"),
        "code-file compress must use strategy=structural: {body}"
    );
    let original_bytes = body
        .get("original_bytes")
        .and_then(Value::as_u64)
        .expect("original_bytes");
    let compressed_bytes = body
        .get("compressed_bytes")
        .and_then(Value::as_u64)
        .expect("compressed_bytes");
    assert!(original_bytes > 0, "original_bytes must be positive for a.rs: {body}");
    assert!(
        compressed_bytes > 0,
        "compressed_bytes must be positive for a non-empty outline: {body}"
    );
    let output = body.get("output").and_then(Value::as_str).expect("output");
    assert!(
        output.contains("alpha") || output.contains("Beta"),
        "structural output must reference indexed symbols: {output:?}"
    );
    assert!(
        !output.contains("let _ = 1"),
        "structural output must NOT include function body literals: {output:?}"
    );
    let tokens_counted = body
        .get("tokens_counted")
        .and_then(Value::as_bool)
        .expect("tokens_counted");
    assert_eq!(
        tokens_counted,
        cfg!(feature = "documents"),
        "tokens_counted must track the documents feature"
    );
    let tokens_note = body.get("tokens_note").and_then(Value::as_str).expect("tokens_note");
    if cfg!(feature = "documents") {
        assert!(
            tokens_note.contains("tokenizer"),
            "real-count note must mention the tokenizer: {tokens_note:?}"
        );
    } else {
        assert!(
            tokens_note.contains("bytes/4"),
            "heuristic note must disclose bytes/4: {tokens_note:?}"
        );
    }
    let original_tokens = body
        .get("original_tokens")
        .and_then(Value::as_u64)
        .expect("original_tokens");
    let compressed_tokens = body
        .get("compressed_tokens")
        .and_then(Value::as_u64)
        .expect("compressed_tokens");
    let tokens_reduced = body
        .get("tokens_reduced")
        .and_then(Value::as_u64)
        .expect("tokens_reduced");
    assert_eq!(
        tokens_reduced,
        original_tokens.saturating_sub(compressed_tokens),
        "tokens_reduced must equal original - compressed"
    );

    let prose = "It is worth noting that this is a test paragraph.\n\n\
                 It is worth noting that this is a test paragraph.\n\n\
                 The code runs correctly.";
    let body = decode_text(
        &service
            .call_tool(call_params("compress", json!({ "text": prose })))
            .await
            .expect("compress(text prose)"),
    );
    assert_eq!(
        body.get("strategy").and_then(Value::as_str),
        Some("lexical"),
        "prose compress must use strategy=lexical: {body}"
    );
    let prose_compressed = body
        .get("compressed_bytes")
        .and_then(Value::as_u64)
        .expect("compressed_bytes");
    let prose_original = body
        .get("original_bytes")
        .and_then(Value::as_u64)
        .expect("original_bytes");
    assert!(
        prose_compressed < prose_original,
        "lexical pass must reduce size for a repeated-filler prose input: {prose_original} → {prose_compressed}"
    );
    let prose_tokens_counted = body
        .get("tokens_counted")
        .and_then(Value::as_bool)
        .expect("tokens_counted");
    assert_eq!(
        prose_tokens_counted,
        cfg!(feature = "documents"),
        "tokens_counted must track the documents feature"
    );
    let prose_orig_tokens = body
        .get("original_tokens")
        .and_then(Value::as_u64)
        .expect("original_tokens");
    let prose_comp_tokens = body
        .get("compressed_tokens")
        .and_then(Value::as_u64)
        .expect("compressed_tokens");
    let prose_reduced = body
        .get("tokens_reduced")
        .and_then(Value::as_u64)
        .expect("tokens_reduced");
    assert_eq!(
        prose_reduced,
        prose_orig_tokens.saturating_sub(prose_comp_tokens),
        "tokens_reduced must equal original - compressed"
    );

    let err = service
        .call_tool(call_params("compress", json!({ "text": "hello", "path": "a.rs" })))
        .await;
    assert!(
        err.is_err(),
        "compress with both text and path must be rejected: {err:?}"
    );

    let err = service.call_tool(call_params("compress", json!({}))).await;
    assert!(
        err.is_err(),
        "compress with neither text nor path must be rejected: {err:?}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("expand", json!({ "path": "a.rs", "name": "alpha" })))
            .await
            .expect("expand(path=a.rs, name=alpha)"),
    );
    assert_eq!(
        body.get("name").and_then(Value::as_str),
        Some("alpha"),
        "expand must echo the resolved name: {body}"
    );
    assert_eq!(
        body.get("kind").and_then(Value::as_str),
        Some("function"),
        "alpha is a function: {body}"
    );
    let expand_body = body.get("body").and_then(Value::as_str).expect("body");
    assert!(
        expand_body.contains("alpha"),
        "expanded body must contain the function source: {expand_body:?}"
    );
    assert!(
        expand_body.contains("let _ = 1"),
        "expanded body must include the function body literal (compress omits it, expand includes it): {expand_body:?}"
    );
    let start_row = body.get("start_row").and_then(Value::as_u64).expect("start_row");
    let end_row = body.get("end_row").and_then(Value::as_u64).expect("end_row");
    assert!(start_row >= 1, "start_row must be one-based: {body}");
    assert!(end_row >= start_row, "end_row must be >= start_row: {body}");
    assert_eq!(
        body.get("truncated").and_then(Value::as_bool),
        Some(false),
        "small function must not be truncated: {body}"
    );

    let err = service
        .call_tool(call_params(
            "expand",
            json!({ "path": "a.rs", "name": "nonexistent_symbol_xyz" }),
        ))
        .await;
    assert!(err.is_err(), "expand with unknown symbol must be rejected: {err:?}");

    let body = decode_text(
        &service
            .call_tool(call_params("expand", json!({ "path": "a.rs", "symbol": "alpha" })))
            .await
            .expect("expand(path=a.rs, symbol=alpha via alias)"),
    );
    assert_eq!(
        body.get("name").and_then(Value::as_str),
        Some("alpha"),
        "expand via `symbol` alias must resolve correctly: {body}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "delta",
                json!({
                    "old": "alpha\nbeta\ngamma\n",
                    "new": "alpha\nbeta2\ngamma\ndelta\n",
                }),
            ))
            .await
            .expect("delta(old, new)"),
    );
    assert_eq!(
        body.get("changed").and_then(Value::as_bool),
        Some(true),
        "differing inputs must report changed=true: {body}"
    );
    assert_eq!(
        body.get("bailed").and_then(Value::as_bool),
        Some(false),
        "small inputs must not bail: {body}"
    );
    assert_eq!(
        body.get("added").and_then(Value::as_u64),
        Some(2),
        "beta2 + delta are the two adds: {body}"
    );
    assert_eq!(
        body.get("removed").and_then(Value::as_u64),
        Some(1),
        "beta is the single deletion: {body}"
    );
    let delta_output = body.get("output").and_then(Value::as_str).expect("output");
    assert!(
        delta_output.starts_with("+2/-1"),
        "delta output must lead with the +A/-R header: {delta_output:?}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("delta", json!({ "old": "same\n", "new": "same\n" })))
            .await
            .expect("delta(identical)"),
    );
    assert_eq!(
        body.get("changed").and_then(Value::as_bool),
        Some(false),
        "identical inputs must report changed=false: {body}"
    );

    std::fs::write(root.join("checkpoint_probe.txt"), b"probe\n").unwrap();
    let body = decode_text(
        &service
            .call_tool(call_params(
                "checkpoint",
                json!({ "text": "We decided to use rayon.\nerror: build failed\n" }),
            ))
            .await
            .expect("checkpoint(text)"),
    );
    assert_eq!(
        body.get("decisions").and_then(Value::as_array),
        Some(&vec![Value::String("We decided to use rayon.".to_string())]),
        "checkpoint must extract the decision line: {body}"
    );
    assert_eq!(
        body.get("errors").and_then(Value::as_array),
        Some(&vec![Value::String("error: build failed".to_string())]),
        "checkpoint must extract the error line: {body}"
    );
    let files_changed: Vec<&str> = body
        .get("files_changed")
        .and_then(Value::as_array)
        .expect("files_changed")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        files_changed.contains(&"checkpoint_probe.txt"),
        "files_changed must come from this repo's git working tree: {files_changed:?}"
    );

    let log = "{\"tool\":\"Read\",\"target\":\"a.rs\",\"bytes\":100}\n\
               {\"tool\":\"Read\",\"target\":\"a.rs\",\"bytes\":100}\n";
    let body = decode_text(
        &service
            .call_tool(call_params("detect_waste", json!({ "log": log })))
            .await
            .expect("detect_waste(log)"),
    );
    let findings = body.get("findings").and_then(Value::as_array).expect("findings");
    assert_eq!(
        findings.len(),
        1,
        "two redundant reads of one target must yield exactly one finding: {body}"
    );
    let finding = &findings[0];
    assert_eq!(
        finding.get("kind").and_then(Value::as_str),
        Some("redundant_read"),
        "finding kind: {finding}"
    );
    assert_eq!(
        finding.get("target").and_then(Value::as_str),
        Some("a.rs"),
        "finding target: {finding}"
    );
    assert_eq!(
        finding.get("count").and_then(Value::as_u64),
        Some(2),
        "finding count: {finding}"
    );
    assert_eq!(
        finding.get("estimated_waste_bytes").and_then(Value::as_u64),
        Some(100),
        "waste is the bytes of every read after the first: {finding}"
    );
    assert_eq!(
        body.get("total_estimated_waste_bytes").and_then(Value::as_u64),
        Some(100),
        "total_estimated_waste_bytes: {body}"
    );
    assert_eq!(
        body.get("truncated").and_then(Value::as_bool),
        Some(false),
        "well under MAX_FINDINGS must not truncate: {body}"
    );

    let _ = service.cancel().await;
}

/// Build a multi-commit fixture used by the git-iterator pagination tests.
///
/// Layout: a single `paged.rs` file rewritten across 5 commits, each modifying the
/// body of `paged()`. That gives `recent_changes` and `commits_touching` ≥ 5
/// commits to page over, `find_commits_by_path` ≥ 5 matches, and `symbol_history`
/// ≥ 5 "modified" entries. The last commit in the helper rewrites only line 1 so
/// `paged.rs` blame partitions into ≥ 2 distinct hunks.
fn build_paging_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    for i in 0..5u32 {
        std::fs::write(
            root.join("paged.rs"),
            format!("pub fn paged() {{ let _ = {i}; }}\npub fn other() {{ let _ = {i}; }}\n"),
        )
        .unwrap();
        git(root, &["add", "paged.rs"]);
        git(root, &["commit", "-qm", &format!("step {i}")]);
    }
    dir
}

/// Spin up an MCP server against the paging fixture and return both halves.
async fn spawn_paging_server() -> (TempDir, rmcp::service::RunningService<rmcp::RoleClient, ()>) {
    let dir = build_paging_repo();
    let root = dir.path();
    run_scan(root);
    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let service = ().serve(transport).await.expect("rmcp handshake");
    (dir, service)
}

fn commit_shas(body: &Value) -> Vec<String> {
    body.get("commits")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.get("sha").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recent_changes_paginates_with_stable_cursor() {
    let (_dir, service) = spawn_paging_server().await;
    let page1 = decode_text(
        &service
            .call_tool(call_params("recent_changes", json!({ "limit": 2 })))
            .await
            .expect("recent_changes page1"),
    );
    let p1_shas = commit_shas(&page1);
    assert_eq!(p1_shas.len(), 2, "recent_changes limit=2 → 2 commits");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("recent_changes page1 must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params("recent_changes", json!({ "limit": 2, "cursor": cursor1 })))
            .await
            .expect("recent_changes page2"),
    );
    let p2_shas = commit_shas(&page2);
    assert_eq!(p2_shas.len(), 2, "recent_changes page2 → 2 more commits");
    assert!(
        p2_shas.iter().all(|s| !p1_shas.contains(s)),
        "recent_changes pages must not overlap: {p1_shas:?} vs {p2_shas:?}"
    );
    let bogus = basemind::testing::encode_in_memory_cursor(0, 0xDEAD_BEEF);
    let stale = decode_text(
        &service
            .call_tool(call_params("recent_changes", json!({ "limit": 2, "cursor": bogus })))
            .await
            .expect("recent_changes stale"),
    );
    assert_eq!(
        stale.get("cursor_invalidated"),
        Some(&Value::Bool(true)),
        "bogus snapshot must surface cursor_invalidated: {stale}"
    );
    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commits_touching_paginates_with_stable_cursor() {
    let (_dir, service) = spawn_paging_server().await;
    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "commits_touching",
                json!({ "path": "paged.rs", "limit": 2 }),
            ))
            .await
            .expect("commits_touching page1"),
    );
    let p1_shas = commit_shas(&page1);
    assert_eq!(p1_shas.len(), 2, "commits_touching page1 → 2 commits");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("commits_touching must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "commits_touching",
                json!({ "path": "paged.rs", "limit": 2, "cursor": cursor1 }),
            ))
            .await
            .expect("commits_touching page2"),
    );
    let p2_shas = commit_shas(&page2);
    assert_eq!(p2_shas.len(), 2, "commits_touching page2 → 2 more commits");
    assert!(
        p2_shas.iter().all(|s| !p1_shas.contains(s)),
        "commits_touching pages must not overlap: {p1_shas:?} vs {p2_shas:?}"
    );
    let bogus = basemind::testing::encode_in_memory_cursor(0, 0xDEAD_BEEF);
    let stale = decode_text(
        &service
            .call_tool(call_params(
                "commits_touching",
                json!({ "path": "paged.rs", "limit": 2, "cursor": bogus }),
            ))
            .await
            .expect("commits_touching stale"),
    );
    assert_eq!(
        stale.get("cursor_invalidated"),
        Some(&Value::Bool(true)),
        "bogus snapshot must surface cursor_invalidated: {stale}"
    );
    let _ = service.cancel().await;
}

/// `hot_files` aggregates the churn window. The paging fixture touches `paged.rs` in all 5
/// commits, so it must rank first with `commits_touching == 5`. Exercises the tool end-to-end
/// through the real `serve` subprocess (whichever of the git-history index / live-walk path is
/// active — both must agree).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hot_files_ranks_the_churned_file_first() {
    let (_dir, service) = spawn_paging_server().await;
    let body = decode_text(
        &service
            .call_tool(call_params("hot_files", json!({ "window": 50, "top_k": 5 })))
            .await
            .expect("hot_files"),
    );
    let files = body
        .get("files")
        .and_then(Value::as_array)
        .expect("hot_files returns a files array");
    assert!(!files.is_empty(), "hot_files returns entries: {body}");
    let top = &files[0];
    assert_eq!(
        top.get("path").and_then(Value::as_str),
        Some("paged.rs"),
        "paged.rs is the most-churned file: {body}"
    );
    assert_eq!(
        top.get("commits_touching").and_then(Value::as_u64),
        Some(5),
        "paged.rs was touched in all 5 commits: {body}"
    );
    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn find_commits_by_path_paginates_with_stable_cursor() {
    let (_dir, service) = spawn_paging_server().await;
    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "find_commits_by_path",
                json!({ "pattern": "paged\\.rs", "window": 50, "limit": 2 }),
            ))
            .await
            .expect("find_commits_by_path page1"),
    );
    let p1_shas = commit_shas(&page1);
    assert_eq!(p1_shas.len(), 2, "find_commits_by_path page1 → 2 commits: {page1}");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("find_commits_by_path must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "find_commits_by_path",
                json!({
                    "pattern": "paged\\.rs",
                    "window": 50,
                    "limit": 2,
                    "cursor": cursor1,
                }),
            ))
            .await
            .expect("find_commits_by_path page2"),
    );
    let p2_shas = commit_shas(&page2);
    assert!(
        !p2_shas.is_empty(),
        "find_commits_by_path page2 must have ≥ 1 commit: {page2}"
    );
    assert!(
        p2_shas.iter().all(|s| !p1_shas.contains(s)),
        "find_commits_by_path pages must not overlap: {p1_shas:?} vs {p2_shas:?}"
    );
    let bogus = basemind::testing::encode_in_memory_cursor(0, 0xDEAD_BEEF);
    let stale = decode_text(
        &service
            .call_tool(call_params(
                "find_commits_by_path",
                json!({
                    "pattern": "paged\\.rs",
                    "window": 50,
                    "limit": 2,
                    "cursor": bogus,
                }),
            ))
            .await
            .expect("find_commits_by_path stale"),
    );
    assert_eq!(
        stale.get("cursor_invalidated"),
        Some(&Value::Bool(true)),
        "bogus snapshot must surface cursor_invalidated: {stale}"
    );
    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symbol_history_paginates_with_stable_cursor() {
    let (_dir, service) = spawn_paging_server().await;
    let history_shas = |body: &Value| -> Vec<String> {
        body.get("history")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.get("sha").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "symbol_history",
                json!({ "path": "paged.rs", "name": "paged", "limit": 2 }),
            ))
            .await
            .expect("symbol_history page1"),
    );
    let p1_shas = history_shas(&page1);
    assert_eq!(p1_shas.len(), 2, "symbol_history page1 → 2 entries: {page1}");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("symbol_history must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "symbol_history",
                json!({
                    "path": "paged.rs",
                    "name": "paged",
                    "limit": 2,
                    "cursor": cursor1,
                }),
            ))
            .await
            .expect("symbol_history page2"),
    );
    let p2_shas = history_shas(&page2);
    assert!(!p2_shas.is_empty(), "symbol_history page2 must have ≥ 1 entry: {page2}");
    assert!(
        p2_shas.iter().all(|s| !p1_shas.contains(s)),
        "symbol_history pages must not overlap: {p1_shas:?} vs {p2_shas:?}"
    );
    let bogus = basemind::testing::encode_in_memory_cursor(0, 0xDEAD_BEEF);
    let stale = decode_text(
        &service
            .call_tool(call_params(
                "symbol_history",
                json!({
                    "path": "paged.rs",
                    "name": "paged",
                    "limit": 2,
                    "cursor": bogus,
                }),
            ))
            .await
            .expect("symbol_history stale"),
    );
    assert_eq!(
        stale.get("cursor_invalidated"),
        Some(&Value::Bool(true)),
        "bogus snapshot must surface cursor_invalidated: {stale}"
    );
    let _ = service.cancel().await;
}

/// Add one more commit that rewrites only line 1 so blame partitions paged.rs into
/// ≥ 2 hunks. Used by the two blame tests below.
fn split_blame_lines(root: &std::path::Path) {
    let prior = std::fs::read_to_string(root.join("paged.rs")).unwrap();
    let mut lines: Vec<&str> = prior.lines().collect();
    lines[0] = "pub fn paged() { let _ = 999; }";
    let new = lines.join("\n") + "\n";
    std::fs::write(root.join("paged.rs"), new).unwrap();
    git(root, &["commit", "-aqm", "split line ownership"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blame_file_paginates_by_start_line() {
    let (dir, service) = spawn_paging_server().await;
    split_blame_lines(dir.path());
    let _ = service.call_tool(call_params("rescan", json!({}))).await;
    let page1 = decode_text(
        &service
            .call_tool(call_params("blame_file", json!({ "path": "paged.rs", "limit": 1 })))
            .await
            .expect("blame_file page1"),
    );
    let p1_hunks = page1
        .get("hunks")
        .and_then(Value::as_array)
        .expect("blame_file page1 hunks");
    assert_eq!(p1_hunks.len(), 1, "blame_file limit=1 → 1 hunk: {page1}");
    let p1_start: Vec<u64> = p1_hunks
        .iter()
        .filter_map(|h| h.get("start_line").and_then(Value::as_u64))
        .collect();
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("blame_file must carry next_cursor when more hunks remain")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "blame_file",
                json!({ "path": "paged.rs", "limit": 1, "cursor": cursor1 }),
            ))
            .await
            .expect("blame_file page2"),
    );
    let p2_hunks = page2
        .get("hunks")
        .and_then(Value::as_array)
        .expect("blame_file page2 hunks");
    assert!(!p2_hunks.is_empty(), "blame_file page2 must have ≥ 1 hunk: {page2}");
    let p2_start: Vec<u64> = p2_hunks
        .iter()
        .filter_map(|h| h.get("start_line").and_then(Value::as_u64))
        .collect();
    assert!(
        p2_start.iter().all(|s| !p1_start.contains(s)),
        "blame_file pages must not overlap by start_line: {p1_start:?} vs {p2_start:?}"
    );
    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blame_symbol_paginates_by_start_line() {
    let (dir, service) = spawn_paging_server().await;
    split_blame_lines(dir.path());
    let _ = service.call_tool(call_params("rescan", json!({}))).await;
    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "blame_symbol",
                json!({ "path": "paged.rs", "name": "paged", "limit": 1 }),
            ))
            .await
            .expect("blame_symbol page1"),
    );
    let p1_hunks = page1
        .get("hunks")
        .and_then(Value::as_array)
        .expect("blame_symbol page1 hunks");
    assert_eq!(p1_hunks.len(), 1, "blame_symbol limit=1 → 1 hunk: {page1}");
    let p1_start = p1_hunks
        .iter()
        .filter_map(|h| h.get("start_line").and_then(Value::as_u64))
        .next()
        .expect("blame_symbol page1 start_line");
    assert!(
        p1_start >= 1,
        "blame_symbol start_line should be 1-based, got {p1_start}"
    );
    let huge_cursor = basemind::testing::encode_in_memory_cursor(9_999, 0);
    let page_empty = decode_text(
        &service
            .call_tool(call_params(
                "blame_symbol",
                json!({
                    "path": "paged.rs",
                    "name": "paged",
                    "limit": 1,
                    "cursor": huge_cursor,
                }),
            ))
            .await
            .expect("blame_symbol cursor past end"),
    );
    let empty_hunks = page_empty
        .get("hunks")
        .and_then(Value::as_array)
        .expect("blame_symbol empty page hunks");
    assert!(
        empty_hunks.is_empty(),
        "blame_symbol with cursor past end should be empty: {page_empty}"
    );
    assert!(
        page_empty.get("next_cursor").is_none(),
        "blame_symbol exhausted page must NOT carry next_cursor"
    );
    let _ = service.cancel().await;
}

/// Verify that `search_documents` with `reranker_enabled=true` is accepted at the
/// param-deserialization layer and, when the feature is active, every returned hit
/// carries a `rerank_score` field.
///
/// This test is gated with `#[ignore]` because the first run downloads the
/// `bge-reranker-base` ONNX weights (~278 MB) from HuggingFace into
/// `~/.cache/xberg/rerankers/`. Pre-warm once before unattended runs:
///
/// ```bash
/// cargo test --test mcp_smoke reranks_search_results -- --ignored --features full
/// ```
///
/// Subsequent runs are fast (cached weights).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[cfg(feature = "documents")]
async fn reranks_search_results() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);
    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let no_rerank = service
        .call_tool(call_params(
            "search_documents",
            json!({ "query": "function", "reranker_enabled": false }),
        ))
        .await;
    if let Ok(ref resp) = no_rerank {
        let body = decode_text(resp);
        if let Some(hits) = body.get("hits").and_then(Value::as_array)
            && !hits.is_empty()
        {
            for hit in hits {
                assert!(
                    hit.get("rerank_score").is_none(),
                    "reranker off — hit must not carry rerank_score: {hit}"
                );
            }
        }
    }

    let reranked = service
        .call_tool(call_params(
            "search_documents",
            json!({
                "query": "function",
                "reranker_enabled": true,
                "reranker_preset": "bge-reranker-base",
            }),
        ))
        .await;
    match &reranked {
        Ok(resp) => {
            let body = decode_text(resp);
            if let Some(hits) = body.get("hits").and_then(Value::as_array) {
                for hit in hits {
                    assert!(
                        hit.get("rerank_score").is_some(),
                        "reranker on — every hit must carry rerank_score: {hit}"
                    );
                    let score = hit["rerank_score"].as_f64().expect("rerank_score is f64");
                    assert!(
                        (0.0..=1.0).contains(&score),
                        "rerank_score must be in [0, 1], got {score}"
                    );
                }
            }
        }
        Err(e) => {
            let _ = e;
        }
    }

    let _ = service.cancel().await;
}

// behind `#[ignore]`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(feature = "documents")]
async fn summarizes_via_extractive_default() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let result = service
        .call_tool(call_params(
            "search_documents",
            json!({
                "query": "test",
                "limit": 5,
                "summarization_enabled": true,
                "summarization_strategy": "extractive",
                "summarization_max_tokens": 100,
            }),
        ))
        .await;

    match &result {
        Ok(resp) => {
            let body = decode_text(resp);
            if let Some(hits) = body.get("hits").and_then(Value::as_array) {
                for hit in hits {
                    if let Some(summary) = hit.get("summary") {
                        assert!(
                            summary.get("text").is_some(),
                            "summary must carry a `text` field: {summary}"
                        );
                        let strategy = summary
                            .get("strategy")
                            .and_then(Value::as_str)
                            .unwrap_or_else(|| panic!("summary must carry `strategy` str: {summary}"));
                        assert_eq!(
                            strategy, "extractive",
                            "per-query strategy=extractive must round-trip; got {strategy}"
                        );
                    }
                }
            }
        }
        Err(e) => {
            let msg = format!("{e:?}");
            assert!(
                !msg.contains("unknown field"),
                "summarization params must deserialize: {msg}"
            );
        }
    }

    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(feature = "documents")]
async fn search_documents_accepts_post_filter_params() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let result = service
        .call_tool(call_params(
            "search_documents",
            json!({
                "query": "test",
                "limit": 10,
                "entity_category": "person",
                "keywords_contains": "foo",
            }),
        ))
        .await;

    match &result {
        Ok(_) => {}
        Err(e) => {
            let msg = format!("{e:?}");
            assert!(
                !msg.contains("unknown field"),
                "post-filter params must deserialize: {msg}"
            );
        }
    }

    let _ = service.cancel().await;
}

/// End-to-end comms round-trip through the real `CommsClient` over an isolated Unix-socket
/// broker — NOT the user's daemon. A throwaway `UdsFrontend` is bound to a temp socket and a
/// temp store, then two clients with DISTINCT agent ids exercise the front-matter/body split:
///
/// * agent A posts (subject + body) to a shared room,
/// * agent B's `read_history` returns the FRONT-MATTER (subject present) and NOT the body,
/// * agent B's `get_body` returns the body,
/// * agent B's inbox shows the unread message, then 0 unread after a `mark_read` pass.
///
/// Isolation: a per-test temp dir for the store + a per-test socket path, so the test daemon
/// never touches the user's real `comms.sock` and parallel test runs do not collide.
// `UdsFrontend` (and the `UnixListener` this test binds) is `#[cfg(unix)]` inside `frontend_uds`,
#[cfg(all(feature = "comms", unix))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn comms_round_trip_front_matter_then_body_then_inbox() {
    use std::sync::Arc;

    use basemind::comms::client::CommsClient;
    use basemind::comms::daemon::Broker;
    use basemind::comms::frontend_uds::UdsFrontend;
    use basemind::comms::ids::AgentId;
    use basemind::comms::singleton::CommsPaths;
    use basemind::comms::store::CommsStore;
    use basemind::comms::transport::CommsFrontend;

    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("c.sock");
    let paths = CommsPaths {
        comms_dir: dir.path().to_path_buf(),
        socket_path: socket_path.clone(),
    };

    let store = Arc::new(CommsStore::open(dir.path()).expect("open comms store"));
    let broker = Arc::new(Broker::new(store));
    let listener = {
        let std_listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind temp socket");
        std_listener.set_nonblocking(true).expect("nonblocking");
        tokio::net::UnixListener::from_std(std_listener).expect("adopt listener")
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let frontend = UdsFrontend::from_listener(listener, socket_path.clone());
    let serve = tokio::spawn(async move { Box::new(frontend).serve(broker, shutdown_rx).await });

    let agent_a = AgentId::parse("agent-a").expect("agent a");
    let agent_b = AgentId::parse("agent-b").expect("agent b");
    let mut a = CommsClient::connect(&paths, agent_a, None, None)
        .await
        .expect("connect a");
    let mut b = CommsClient::connect(&paths, agent_b, None, None)
        .await
        .expect("connect b");

    let thread = a
        .start_thread(
            Some("Team".to_string()),
            None,
            vec![basemind::comms::ids::AgentId::parse("agent-b").unwrap()],
        )
        .await
        .expect("start thread")
        .id;

    let subject = "deploy status";
    let body = b"all systems green".to_vec();
    let message_id = a
        .post_message(
            thread.clone(),
            subject.to_string(),
            body.clone(),
            vec!["ops".to_string()],
            None,
        )
        .await
        .expect("post");

    let (history, _next) = b.read_history(thread.clone(), None, 10, None).await.expect("history");
    assert_eq!(history.len(), 1, "exactly one posted message");
    let seq_meta = &history[0];
    let meta = &seq_meta.meta;
    assert_eq!(seq_meta.seq, 1, "front-matter carries the per-thread seq");
    assert_eq!(meta.subject, subject, "front-matter carries the subject");
    assert_eq!(meta.id, message_id, "front-matter id matches the posted id");
    assert_eq!(
        meta.body_len,
        body.len() as u32,
        "front-matter carries body_len, not the body"
    );
    let meta_json = serde_json::to_value(meta).expect("serialize meta");
    assert!(
        meta_json.get("body").is_none(),
        "history front-matter must NOT include the body: {meta_json}"
    );
    assert!(
        meta_json.get("body_len").is_some(),
        "history front-matter must include body_len: {meta_json}"
    );

    let fetched = b.get_body(message_id.clone()).await.expect("get_body");
    assert_eq!(
        fetched.as_deref(),
        Some(body.as_slice()),
        "message_get returns the exact body"
    );

    let (inbox, unread, _c) = b
        .read_inbox(None, None, None, 10, true, None)
        .await
        .expect("inbox read+mark");
    assert_eq!(inbox.len(), 1, "the posted message is in B's inbox");
    assert_eq!(inbox[0].meta.subject, subject, "inbox carries front-matter subject");
    assert_eq!(unread, 0, "mark_read drained the unread count in this page");

    let (inbox2, unread2, _c2) = b
        .read_inbox(None, None, None, 10, false, None)
        .await
        .expect("inbox re-read");
    assert!(inbox2.is_empty(), "no unread messages remain after mark_read");
    assert_eq!(unread2, 0, "unread count stays 0 after mark_read");

    let second_id = a
        .post_message(thread.clone(), "second".to_string(), b"more".to_vec(), vec![], None)
        .await
        .expect("post second");
    let (inbox3, _u3, _c3) = b
        .read_inbox(None, None, None, 10, false, None)
        .await
        .expect("inbox shows second");
    assert_eq!(inbox3.len(), 1, "the second message is unread in B's inbox");
    assert_eq!(inbox3[0].meta.id, second_id, "inbox shows the second message");

    let (acked, cursors_advanced) = b.ack_inbox(vec![second_id.clone()], None, None).await.expect("ack");
    assert_eq!(acked, 1, "exactly one message acked");
    assert_eq!(
        cursors_advanced,
        vec![(thread.as_str().to_string(), 2)],
        "ack advances B's thread cursor to seq 2"
    );

    let (inbox4, _u4, _c4) = b
        .read_inbox(None, None, None, 10, false, None)
        .await
        .expect("inbox after ack");
    assert!(inbox4.is_empty(), "ack removed the message from B's inbox");

    let (history_after, _n) = b
        .read_history(thread.clone(), None, 10, None)
        .await
        .expect("history after ack");
    assert_eq!(history_after.len(), 2, "ack does not delete from the shared log");

    let _ = shutdown_tx.send(true);
    let _ = serve.await;
}

/// `inbox_wait` end to end over the same isolated UDS broker setup as the round-trip test above:
/// agent-b posts, agent-a's `inbox_wait` (short timeout) returns promptly with the new message and
/// `timed_out: false`; a second `inbox_wait` with nothing new posted times out.
#[cfg(all(feature = "comms", unix))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn comms_inbox_wait_delivers_then_times_out() {
    use std::sync::Arc;

    use basemind::comms::client::CommsClient;
    use basemind::comms::daemon::Broker;
    use basemind::comms::frontend_uds::UdsFrontend;
    use basemind::comms::ids::AgentId;
    use basemind::comms::singleton::CommsPaths;
    use basemind::comms::store::CommsStore;
    use basemind::comms::transport::CommsFrontend;

    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("c.sock");
    let paths = CommsPaths {
        comms_dir: dir.path().to_path_buf(),
        socket_path: socket_path.clone(),
    };

    let store = Arc::new(CommsStore::open(dir.path()).expect("open comms store"));
    let broker = Arc::new(Broker::new(store));
    let listener = {
        let std_listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind temp socket");
        std_listener.set_nonblocking(true).expect("nonblocking");
        tokio::net::UnixListener::from_std(std_listener).expect("adopt listener")
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let frontend = UdsFrontend::from_listener(listener, socket_path.clone());
    let serve = tokio::spawn(async move { Box::new(frontend).serve(broker, shutdown_rx).await });

    let agent_a = AgentId::parse("agent-a").expect("agent a");
    let agent_b = AgentId::parse("agent-b").expect("agent b");
    let mut a = CommsClient::connect(&paths, agent_a, None, None)
        .await
        .expect("connect a");
    let mut b = CommsClient::connect(&paths, agent_b, None, None)
        .await
        .expect("connect b");

    let thread = a
        .start_thread(
            Some("Team".to_string()),
            None,
            vec![basemind::comms::ids::AgentId::parse("agent-b").unwrap()],
        )
        .await
        .expect("start thread")
        .id;

    b.post_message(
        thread.clone(),
        "deploy status".to_string(),
        b"all green".to_vec(),
        vec![],
        None,
    )
    .await
    .expect("b posts");

    let (timed_out, rows, unread, _next) = a
        .wait_inbox(None, None, None, None, None, 100, std::time::Duration::from_secs(10))
        .await
        .expect("first wait_inbox");
    assert!(!timed_out, "a's wait must resolve from b's pre-existing post");
    assert_eq!(rows.len(), 1, "exactly the one posted message");
    assert_eq!(rows[0].meta.subject, "deploy status");
    assert_eq!(
        unread, 0,
        "the single post was returned in rows; none remain beyond this page"
    );

    let (timed_out2, rows2, _unread2, _next2) = a
        .wait_inbox(None, None, None, None, None, 100, std::time::Duration::from_millis(300))
        .await
        .expect("second wait_inbox");
    assert!(timed_out2, "no new post landed; the second wait must time out");
    assert!(rows2.is_empty(), "a timed-out wait returns no rows");

    let _ = shutdown_tx.send(true);
    let _ = serve.await;
}

/// End-to-end MCP contract for the headless-shell tools through a real
/// `basemind serve` child process. The child binary carries the
/// `--__internal-daemon` intercept, so `shell_spawn` actually re-execs basemind
/// as the embedded rmux daemon. `BASEMIND_SHELLS_SOCKET` sandboxes that daemon on
/// a per-test temp socket so parallel runs and the user's environment never
/// collide.
///
/// Proves the wired surface: `shell_spawn` → poll `shell_capture` until the
/// sentinel appears → `shell_kill`.
#[cfg(all(feature = "shells", unix))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_tools_spawn_capture_kill_through_mcp() {
    use std::time::{Duration, Instant};

    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    std::fs::write(
        root.join("basemind.toml"),
        b"\"$schema\" = \"v1\"\n\n[shells]\nvisual = \"headless\"\n",
    )
    .expect("write headless shells config");

    let socket = dir.path().join("shells.sock");
    let bin = env!("CARGO_BIN_EXE_basemind");
    let socket_for_env = socket.clone();
    let cmd = AsyncCommand::new(bin).configure(move |c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
        c.env("BASEMIND_SHELLS_SOCKET", &socket_for_env);
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let spawned = service
        .call_tool(call_params(
            "shell_spawn",
            json!({ "command": "echo basemind-hi; sleep 5" }),
        ))
        .await
        .expect("shell_spawn call");
    let spawned = decode_text(&spawned);
    let session_id = spawned
        .get("session_id")
        .and_then(Value::as_str)
        .expect("session_id in shell_spawn response")
        .to_string();
    let attach_command = spawned
        .get("attach_command")
        .and_then(Value::as_str)
        .expect("attach_command in shell_spawn response");
    assert!(
        attach_command.contains("--__internal-attach ")
            && attach_command.contains("--socket ")
            && attach_command.contains("--size "),
        "attach_command should be a basemind internal-attach re-exec line: {spawned:?}"
    );

    let escaped = service
        .call_tool(call_params(
            "shell_spawn",
            json!({ "command": "true", "cwd": "../../../etc" }),
        ))
        .await;
    assert!(
        escaped.is_err(),
        "shell_spawn must reject a cwd escaping the repository root: {escaped:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let captured = service
            .call_tool(call_params("shell_capture", json!({ "session_id": session_id })))
            .await
            .expect("shell_capture call");
        let text = decode_text(&captured)
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if text.contains("basemind-hi") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for sentinel via shell_capture; last text {text:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let killed = service
        .call_tool(call_params("shell_kill", json!({ "session_id": session_id })))
        .await
        .expect("shell_kill call");
    let killed = decode_text(&killed);
    assert_eq!(
        killed.get("killed").and_then(Value::as_bool),
        Some(true),
        "shell_kill should report killed=true for a live session: {killed:?}"
    );

    let second = service
        .call_tool(call_params("shell_kill", json!({ "session_id": session_id })))
        .await;
    assert!(second.is_err(), "killing an already-forgotten session_id should error");

    let _ = service.cancel().await;
}

/// Spawn `basemind serve` against `root`, optionally setting `BASEMIND_MCP_LEAN`, and return the
/// connected rmcp client service.
async fn spawn_serve(root: &Path, lean: Option<&str>) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let bin = env!("CARGO_BIN_EXE_basemind");
    let lean = lean.map(str::to_string);
    let root = root.to_path_buf();
    let cmd = AsyncCommand::new(bin).configure(move |c| {
        c.arg("--root").arg(&root).arg("serve").arg("--view").arg("working");
        c.env_remove("BASEMIND_MCP_LEAN");
        if let Some(v) = &lean {
            c.env("BASEMIND_MCP_LEAN", v);
        }
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    ().serve(transport).await.expect("rmcp handshake")
}

/// Security regression: `rescan` must reject a `paths` entry that escapes the repository root via
/// `..` traversal. `rescan` takes raw strings (not `RelPath`), and before the fix
/// `state.root.join("../../etc/passwd")` fed a traversal path into the scanner — which, with
/// `scan.respect_gitignore = false`, read and indexed a file outside the repo. A valid in-repo
/// path must still be accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_rejects_paths_escaping_the_repo_root() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root, None).await;

    let escaping = service
        .call_tool(call_params(
            "rescan",
            json!({ "paths": ["../../../../../../etc/passwd"] }),
        ))
        .await;
    assert!(
        escaping.is_err(),
        "rescan must reject a path that escapes the repo root, got: {escaping:?}"
    );

    let ok = service
        .call_tool(call_params("rescan", json!({ "paths": ["a.rs"] })))
        .await;
    assert!(ok.is_ok(), "rescan must accept a valid in-repo path, got: {ok:?}");

    let _ = service.cancel().await;
}

/// `serve` auto-scans an empty index on boot; `status` must report that indexing state separately
/// from query latency. Starting serve on a FRESH (unscanned) repo triggers the boot scan; polling
/// `status` must converge to `indexing: false` with an `index_build_ms` recording the build cost.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_auto_scan_reports_index_build_ms_on_status() {
    let dir = build_repo();
    let root = dir.path();
    let service = spawn_serve(root, None).await;

    let mut settled: Option<Value> = None;
    for _ in 0..200 {
        let result = service
            .call_tool(call_params("status", json!({})))
            .await
            .expect("status");
        let v = decode_text(&result);
        let done = v.get("file_count").and_then(Value::as_u64).unwrap_or(0) > 0
            && !v.get("indexing").and_then(Value::as_bool).unwrap_or(false);
        if done {
            settled = Some(v);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let v = settled.expect("boot auto-scan must complete and populate the index");
    assert!(
        v.get("index_build_ms").and_then(Value::as_u64).is_some(),
        "status must report index_build_ms after the boot auto-scan: {v}"
    );

    let _ = service.cancel().await;
}

/// W5 slice 3: the lean MCP surface is STRICTLY opt-in.
///
/// * `BASEMIND_MCP_LEAN=1` → exactly the three wrapper tools are advertised, and
///   `invoke_tool { search_symbols }` returns the same payload as a direct `search_symbols` call.
/// * flag UNSET → the full surface is advertised unchanged (well over the three wrappers, and
///   `search_symbols` is callable directly).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lean_surface_is_opt_in_and_round_trips_through_invoke_tool() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let full = spawn_serve(root, None).await;
    let full_tools = full.list_all_tools().await.expect("list tools (full)");
    let full_names: Vec<&str> = full_tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        full_tools.len() > 10,
        "default surface should advertise the full tool set, got {}: {full_names:?}",
        full_tools.len()
    );
    assert!(
        full_names.contains(&"search_symbols"),
        "default surface lists search_symbols: {full_names:?}"
    );
    assert!(
        !full_names.contains(&"invoke_tool"),
        "default surface must NOT expose the lean wrappers: {full_names:?}"
    );

    let annotations_of = |name: &str| {
        full_tools
            .iter()
            .find(|t| t.name.as_ref() == name)
            .unwrap_or_else(|| panic!("tool {name} present in full surface"))
            .annotations
            .clone()
            .unwrap_or_else(|| panic!("tool {name} must carry ToolAnnotations"))
    };
    for read_only in [
        "outline",
        "search_symbols",
        "find_references",
        "status",
        "list_files",
        "find_files",
    ] {
        assert_eq!(
            annotations_of(read_only).read_only_hint,
            Some(true),
            "read-only tool {read_only} must advertise read_only_hint=true"
        );
    }
    for mutating in ["rescan", "cache_clear"] {
        assert_eq!(
            annotations_of(mutating).read_only_hint,
            Some(false),
            "mutating tool {mutating} must advertise read_only_hint=false"
        );
    }
    assert_eq!(
        annotations_of("cache_clear").destructive_hint,
        Some(true),
        "cache_clear must advertise destructive_hint=true"
    );
    let direct = decode_text(
        &full
            .call_tool(call_params("search_symbols", json!({ "needle": "Greet", "limit": 10 })))
            .await
            .expect("direct search_symbols"),
    );
    let _ = full.cancel().await;

    let lean = spawn_serve(root, Some("1")).await;
    let lean_tools = lean.list_all_tools().await.expect("list tools (lean)");
    let mut lean_names: Vec<&str> = lean_tools.iter().map(|t| t.name.as_ref()).collect();
    lean_names.sort_unstable();
    assert_eq!(
        lean_names,
        vec!["get_tool_schema", "invoke_tool", "list_tools"],
        "lean mode advertises exactly the three wrapper tools"
    );

    let listing = decode_text(
        &lean
            .call_tool(call_params("list_tools", json!({})))
            .await
            .expect("lean list_tools"),
    );
    let listed = listing.get("tools").and_then(Value::as_array).expect("tools array");
    assert!(
        listed
            .iter()
            .any(|t| t.get("name").and_then(Value::as_str) == Some("search_symbols")),
        "lean list_tools should surface the real search_symbols tool: {listing}"
    );

    let schema = decode_text(
        &lean
            .call_tool(call_params("get_tool_schema", json!({ "tool_name": "search_symbols" })))
            .await
            .expect("lean get_tool_schema"),
    );
    assert_eq!(
        schema.get("name").and_then(Value::as_str),
        Some("search_symbols"),
        "schema echoes the tool name: {schema}"
    );
    assert!(
        schema.get("input_schema").is_some(),
        "schema carries the input_schema: {schema}"
    );

    let via_invoke = decode_text(
        &lean
            .call_tool(call_params(
                "invoke_tool",
                json!({
                    "tool_name": "search_symbols",
                    "tool_input": { "needle": "Greet", "limit": 10 }
                }),
            ))
            .await
            .expect("lean invoke_tool"),
    );
    let mut via_invoke = via_invoke;
    let mut direct = direct;
    for (label, body) in [("invoke_tool", &mut via_invoke), ("direct", &mut direct)] {
        let removed = body
            .as_object_mut()
            .expect("response is a JSON object")
            .remove("elapsed_us");
        assert!(
            removed.is_some_and(|v| v.as_u64().is_some()),
            "{label} search_symbols response must carry an `elapsed_us` reading"
        );
    }
    assert_eq!(
        via_invoke, direct,
        "invoke_tool result must match a direct search_symbols call (latency field aside)"
    );

    let bad = lean
        .call_tool(call_params(
            "invoke_tool",
            json!({ "tool_name": "definitely_not_a_tool", "tool_input": {} }),
        ))
        .await;
    assert!(bad.is_err(), "invoke_tool rejects unknown tool names");

    let _ = lean.cancel().await;
}

/// 0.8.0: the server advertises reusable prompt templates and renders them with arguments.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompts_are_listed_and_rendered_with_arguments() {
    use rmcp::model::GetPromptRequestParams;

    let dir = build_repo();
    let root = dir.path();
    run_scan(root);
    let server = spawn_serve(root, None).await;

    let prompts = server.list_all_prompts().await.expect("list_all_prompts");
    let names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
    for expected in ["onboard-repo", "trace-symbol", "explain-file", "review-working-tree"] {
        assert!(
            names.contains(&expected),
            "prompt `{expected}` must be advertised, got: {names:?}"
        );
    }

    let trace = prompts
        .iter()
        .find(|p| p.name == "trace-symbol")
        .expect("trace-symbol present");
    let args = trace.arguments.as_ref().expect("trace-symbol has arguments");
    assert!(
        args.iter().any(|a| a.name == "symbol"),
        "trace-symbol must declare a `symbol` argument, got: {:?}",
        args.iter().map(|a| &a.name).collect::<Vec<_>>()
    );

    let rendered = server
        .get_prompt(
            GetPromptRequestParams::new("trace-symbol")
                .with_arguments(serde_json::json!({ "symbol": "Greeter" }).as_object().unwrap().clone()),
        )
        .await
        .expect("get_prompt trace-symbol");
    assert!(
        !rendered.messages.is_empty(),
        "rendered prompt must carry at least one message"
    );
    let body = rendered
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        body.contains("Greeter") && body.contains("search_symbols"),
        "rendered trace-symbol must interpolate the symbol and name basemind tools, got: {body}"
    );

    let _ = server.cancel().await;
}

/// 0.8.0: the server completes prompt arguments from the indexed code map.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completes_prompt_arguments_from_the_code_map() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);
    let server = spawn_serve(root, None).await;

    let symbols = server
        .complete_prompt_argument("trace-symbol", "symbol", "al", None)
        .await
        .expect("complete symbol argument");
    assert!(
        symbols.values.iter().any(|v| v == "alpha"),
        "symbol completion for `al` must include `alpha`, got: {:?}",
        symbols.values
    );
    assert!(
        symbols.values.iter().all(|v| v.starts_with("al")),
        "every symbol completion must honor the prefix, got: {:?}",
        symbols.values
    );

    let paths = server
        .complete_prompt_argument("explain-file", "path", "a", None)
        .await
        .expect("complete path argument");
    assert!(
        paths.values.iter().any(|v| v == "a.rs"),
        "path completion for `a` must include `a.rs`, got: {:?}",
        paths.values
    );

    let none = server
        .complete_prompt_argument("onboard-repo", "nope", "x", None)
        .await
        .expect("complete unknown argument is not an error");
    assert!(
        none.values.is_empty(),
        "uncompletable argument yields no values, got: {:?}",
        none.values
    );

    let _ = server.cancel().await;
}

/// 0.8.0: `rescan` emits a logging notification (with counts) and progress notifications when
/// the client supplies a progress token. Uses a capturing client handler to observe both.
#[allow(deprecated)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_emits_logging_and_progress_notifications() {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use rmcp::model::{LoggingMessageNotificationParam, NumberOrString, ProgressNotificationParam};
    use rmcp::service::NotificationContext;
    use rmcp::{ClientHandler, RoleClient};

    #[derive(Clone, Default)]
    struct Capture {
        logs: Arc<StdMutex<Vec<LoggingMessageNotificationParam>>>,
        progress: Arc<StdMutex<Vec<ProgressNotificationParam>>>,
    }

    impl ClientHandler for Capture {
        async fn on_logging_message(
            &self,
            params: LoggingMessageNotificationParam,
            _context: NotificationContext<RoleClient>,
        ) {
            self.logs.lock().unwrap().push(params);
        }
        async fn on_progress(&self, params: ProgressNotificationParam, _context: NotificationContext<RoleClient>) {
            self.progress.lock().unwrap().push(params);
        }
    }

    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let capture = Capture::default();
    let logs = Arc::clone(&capture.logs);
    let progress = Arc::clone(&capture.progress);

    let bin = env!("CARGO_BIN_EXE_basemind");
    let root_buf = root.to_path_buf();
    let cmd = AsyncCommand::new(bin).configure(move |c| {
        c.arg("--root").arg(&root_buf).arg("serve").arg("--view").arg("working");
        c.env_remove("BASEMIND_MCP_LEAN");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let server = capture.serve(transport).await.expect("rmcp handshake");

    let mut params = call_params("rescan", json!({}));
    rmcp::model::RequestParamsMeta::set_progress_token(
        &mut params,
        rmcp::model::ProgressToken(NumberOrString::String("rescan-1".into())),
    );
    server.call_tool(params).await.expect("rescan call");

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let captured_logs = logs.lock().unwrap().clone();
    let captured_progress = progress.lock().unwrap().clone();

    assert!(
        captured_logs
            .iter()
            .any(|l| l.data.get("event").and_then(|v| v.as_str()) == Some("rescan_complete")),
        "rescan must emit a `rescan_complete` logging notification, got: {:?}",
        captured_logs.iter().map(|l| &l.data).collect::<Vec<_>>()
    );

    assert!(
        captured_progress.len() >= 2,
        "rescan with a progress token must emit start + done progress, got {}",
        captured_progress.len()
    );
    assert!(
        captured_progress.iter().any(|p| p.total.is_none()),
        "expected an indeterminate start progress (total: None)"
    );
    assert!(
        captured_progress
            .iter()
            .any(|p| p.total == Some(p.progress) && p.total.is_some()),
        "expected a completion progress where progress == total (file count)"
    );
    let first = &captured_progress[0].progress_token;
    assert!(
        captured_progress.iter().all(|p| &p.progress_token == first),
        "all progress notifications must carry the same request token"
    );

    let _ = server.cancel().await;
}

/// Sane upper bound for one tool call against the tiny in-test fixture: 60 seconds expressed in
/// microseconds. Deliberately generous — the point is not to assert a performance target (that
/// belongs in `harden.rs`), but to catch a unit error. If `elapsed_us` were ever populated with
/// nanoseconds, or with a raw `Duration` debug value, a real call against a 1-file repo would blow
/// past this; a genuine microsecond reading cannot.
const SANE_ELAPSED_US_MAX: u64 = 60_000_000;

/// Extract `elapsed_us` from a tool response, asserting it is present and plausibly a microsecond
/// reading.
///
/// Deliberately does NOT assert `> 0`. A genuinely sub-microsecond operation — an `outline` served
/// straight from the warm in-RAM map, say — truncates to `0` honestly, so a blanket non-zero
/// assertion would test something the contract does not promise and would flake on a fast machine.
/// Callers that perform work with a hard floor above a microsecond (a store read, a git walk) assert
/// `> 0` themselves via [`assert_stamped`].
fn assert_sane_elapsed_us(tool: &str, body: &Value) -> u64 {
    let us = body
        .get("elapsed_us")
        .unwrap_or_else(|| panic!("{tool}: response must carry `elapsed_us`: {body}"))
        .as_u64()
        .unwrap_or_else(|| panic!("{tool}: `elapsed_us` must be an unsigned integer: {body}"));
    assert!(
        us < SANE_ELAPSED_US_MAX,
        "{tool}: `elapsed_us` = {us} is not a plausible microsecond reading (unit error?)"
    );
    us
}

/// Assert `elapsed_us` was actually stamped from the body timer rather than left at the `0`
/// initializer. Only valid for tools whose measured region contains work with a floor comfortably
/// above one microsecond (reading a blob off disk, walking git history).
fn assert_stamped(tool: &str, body: &Value) {
    let us = assert_sane_elapsed_us(tool, body);
    assert!(
        us > 0,
        "{tool}: `elapsed_us` is 0 — the body timer was never stamped into the response \
         (this tool reads from disk / walks git, so it cannot honestly take under 1 µs)"
    );
}

/// The timing contract: every latency-relevant tool reports its own server-side handler latency in
/// microseconds, on both the code-map and the git surface.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn latency_tools_report_microsecond_elapsed_us() {
    let (_dir, service) = spawn_paging_server().await;

    let search = decode_text(
        &service
            .call_tool(call_params("search_symbols", json!({ "needle": "paged" })))
            .await
            .expect("search_symbols"),
    );
    assert!(
        search.get("total").and_then(Value::as_u64).unwrap_or(0) >= 1,
        "fixture should match the `paged` symbol: {search}"
    );
    assert_sane_elapsed_us("search_symbols", &search);

    let recent = decode_text(
        &service
            .call_tool(call_params("recent_changes", json!({ "limit": 2 })))
            .await
            .expect("recent_changes"),
    );
    assert_eq!(commit_shas(&recent).len(), 2, "fixture has 5 commits; asked for 2");
    assert_stamped("recent_changes", &recent);

    let outline = decode_text(
        &service
            .call_tool(call_params("outline", json!({ "path": "paged.rs", "l2": true })))
            .await
            .expect("outline"),
    );
    assert!(
        outline
            .get("symbols")
            .and_then(Value::as_array)
            .is_some_and(|s| !s.is_empty()),
        "fixture outline should carry symbols: {outline}"
    );
    assert_stamped("outline", &outline);

    let _ = service.cancel().await;
}

/// `elapsed_us` is an ADDITIVE field: a client built against the previous response shape — one that
/// has never heard of `elapsed_us` — must still deserialize the new response unchanged.
///
/// This is the compatibility guarantee the repo's schema convention requires (new response fields
/// are additive). It holds because no response struct sets `deny_unknown_fields`, so serde skips
/// keys the old client doesn't know.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elapsed_us_is_additive_for_older_clients() {
    /// A verbatim copy of the pre-`elapsed_us` `search_symbols` response shape.
    #[derive(serde::Deserialize)]
    struct LegacySearchResponse {
        total: usize,
        truncated: bool,
        results: Vec<Value>,
    }

    /// A pre-`elapsed_us` git response shape.
    #[derive(serde::Deserialize)]
    struct LegacyRecentChangesResponse {
        commits: Vec<Value>,
    }

    let (_dir, service) = spawn_paging_server().await;

    let search = decode_text(
        &service
            .call_tool(call_params("search_symbols", json!({ "needle": "paged" })))
            .await
            .expect("search_symbols"),
    );
    assert!(
        search.get("elapsed_us").is_some(),
        "precondition: the new response really does carry the new field"
    );
    let legacy: LegacySearchResponse =
        serde_json::from_value(search).expect("an older client must still deserialize the response");
    assert!(legacy.total >= 1, "old client still reads `total`");
    assert!(!legacy.truncated, "old client still reads `truncated`");
    assert_eq!(legacy.results.len(), legacy.total, "old client still reads `results`");

    let recent = decode_text(
        &service
            .call_tool(call_params("recent_changes", json!({ "limit": 2 })))
            .await
            .expect("recent_changes"),
    );
    let legacy: LegacyRecentChangesResponse =
        serde_json::from_value(recent).expect("an older client must still deserialize the git response");
    assert_eq!(legacy.commits.len(), 2, "old client still reads `commits`");

    let _ = service.cancel().await;
}

/// A repo wide enough to exceed the old `scan_cap = max(limit * 8, 2000)` files-visited bound,
/// with the only occurrence of a rare token in the last file by path order (`by_path` is a
/// `BTreeMap`, so `zzz/` sorts after `src/`). That placement is the whole point: a rare
/// identifier — precisely what one greps for — is exactly what does not live in the first
/// 2000 files.
fn build_wide_repo(files: usize, rare_token: &str) -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    let src = root.join("src");
    std::fs::create_dir_all(&src).expect("mkdir src");
    for i in 0..files {
        std::fs::write(src.join(format!("f{i:05}.rs")), format!("pub fn filler{i}() {{}}\n")).expect("write filler");
    }
    let far = root.join("zzz");
    std::fs::create_dir_all(&far).expect("mkdir zzz");
    std::fs::write(far.join("rare.rs"), format!("pub fn {rare_token}() {{}}\n")).expect("write rare");

    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "wide"]);
    dir
}

/// Regression: `workspace_grep` must scan the whole indexed corpus, not the first
/// `limit * 8` files.
///
/// The old bound was a files-VISITED cap, which is the wrong bound for a linear content scan:
/// a default `limit = 100` grep visited 2000 files of a 68 k-file monorepo (2.9 %, in path order)
/// and returned a fast, confident, wrong zero for any token that lived past the cut. `limit` caps
/// HITS; the corpus is always fully scanned (subject to the `language` / `path_contains` filters).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_grep_finds_a_rare_token_past_the_old_scan_cap() {
    const RARE: &str = "OptimizationStatusZQX";
    let dir = build_wide_repo(2_100, RARE);
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root, None).await;

    let body = decode_text(
        &service
            .call_tool(call_params(
                "workspace_grep",
                json!({ "pattern": RARE, "include_context": false }),
            ))
            .await
            .expect("workspace_grep"),
    );

    let hits = body.get("hits").and_then(Value::as_array).expect("hits");
    assert_eq!(
        hits.len(),
        1,
        "the rare token lives past the old 2000-file scan cap and must still be found: {body}"
    );
    assert_eq!(
        hits[0].get("path").and_then(Value::as_str),
        Some("zzz/rare.rs"),
        "hit must point at the far file: {body}"
    );
    assert_eq!(
        body.get("total_matches").and_then(Value::as_u64),
        Some(1),
        "a full-corpus scan reports the exact match count: {body}"
    );
    assert_eq!(
        body.get("total_files_matched").and_then(Value::as_u64),
        Some(1),
        "exactly one file contains the rare token: {body}"
    );
    assert_eq!(
        body.get("truncated").and_then(Value::as_bool),
        Some(false),
        "every match was returned, so nothing was truncated: {body}"
    );

    let _ = service.cancel().await;
}

/// `limit` cuts a page, it does not cut the corpus — so paging with `next_cursor` must reconstruct
/// the complete result exactly: no hit dropped, no hit served twice.
///
/// The interesting case is a file holding more matches than `limit` (the fixture's `a.rs` has
/// several `pub fn` occurrences). A file-granular cursor would either replay that file's leading
/// hits forever or skip its tail, so the cursor resolves to a HIT, not to a file. Paged with
/// `limit = 1`, every step exercises a mid-file resume.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paging_a_grep_one_hit_at_a_time_reconstructs_the_whole_result() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root, None).await;

    let whole = decode_text(
        &service
            .call_tool(call_params(
                "workspace_grep",
                json!({ "pattern": "pub fn", "include_context": false, "limit": 1000 }),
            ))
            .await
            .expect("workspace_grep(unpaged)"),
    );
    assert_eq!(
        whole.get("truncated").and_then(Value::as_bool),
        Some(false),
        "limit=1000 covers the fixture, so the baseline must be complete: {whole}"
    );
    let expected: Vec<(String, u64)> = grep_keys(&whole);
    assert!(
        expected.len() >= 3,
        "fixture must hold several 'pub fn' matches to make paging meaningful, got {expected:?}"
    );

    let mut paged: Vec<(String, u64)> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..(expected.len() + 1) {
        let mut args = json!({ "pattern": "pub fn", "include_context": false, "limit": 1 });
        if let Some(c) = &cursor {
            args["cursor"] = Value::String(c.clone());
        }
        let page = decode_text(
            &service
                .call_tool(call_params("workspace_grep", args))
                .await
                .expect("workspace_grep(page)"),
        );
        paged.extend(grep_keys(&page));
        match page.get("next_cursor").and_then(Value::as_str) {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
    }

    assert_eq!(
        paged, expected,
        "paging one hit at a time must reproduce the unpaged result exactly — no loss, no replay"
    );

    let _ = service.cancel().await;
}

/// `(path, line_num)` of every hit in a grep response, in response order.
fn grep_keys(body: &Value) -> Vec<(String, u64)> {
    body.get("hits")
        .and_then(Value::as_array)
        .expect("hits")
        .iter()
        .map(|h| {
            (
                h.get("path").and_then(Value::as_str).unwrap_or_default().to_string(),
                h.get("line_num").and_then(Value::as_u64).unwrap_or_default(),
            )
        })
        .collect()
}

/// **P0 contract, at the MCP surface.** `find_callers` must never present a resolution-limited
/// subset as the complete caller set.
///
/// The fixture is the shape that broke in production: `pkg/mod.py` defines `target` and calls it
/// itself (so intra-file resolution "succeeds"), while two other files reach it through
/// `from pkg import mod` + `mod.target()` — a module-object import, which binds a module rather than
/// the function, so the cross-file join has no export to bind and those callers are invisible to
/// resolution. `find_callers` used to return only the resolvable subset with no truncation flag; an
/// agent asking "what calls this?" before a refactor got a confident, precise-looking, wrong answer.
///
/// The name scan is the sound floor, so `find_callers` and `find_references` must AGREE on the count
/// for this unambiguous name, and `resolved_total` may only ever be a subset of `total`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn find_callers_never_reports_a_resolution_limited_subset_as_complete() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir(root.join("pkg")).expect("pkg dir");
    std::fs::write(root.join("pkg/__init__.py"), b"").unwrap();
    std::fs::write(
        root.join("pkg/mod.py"),
        b"def target():\n    return 1\n\n\ndef seed():\n    return target()\n",
    )
    .unwrap();
    std::fs::write(
        root.join("caller_a.py"),
        b"from pkg import mod\n\n\ndef go():\n    return mod.target()\n",
    )
    .unwrap();
    std::fs::write(
        root.join("caller_b.py"),
        b"from pkg import mod\n\n\ndef go2():\n    return mod.target() + mod.target()\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);
    run_scan(root);

    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let references = decode_text(
        &service
            .call_tool(call_params(
                "find_references",
                json!({ "name": "target", "limit": 500 }),
            ))
            .await
            .expect("find_references"),
    );
    let reference_total = references.get("total").and_then(Value::as_u64).expect("total");
    assert_eq!(
        reference_total, 4,
        "ground truth: 4 target() call sites across the fixture: {references}"
    );

    let callers = decode_text(
        &service
            .call_tool(call_params(
                "find_callers",
                json!({ "path": "pkg/mod.py", "name": "target", "limit": 500 }),
            ))
            .await
            .expect("find_callers"),
    );
    assert_eq!(
        callers.get("total").and_then(Value::as_u64),
        Some(reference_total),
        "find_callers must agree with find_references on an unambiguous name — never a subset: {callers}"
    );
    let hits = callers.get("hits").and_then(Value::as_array).expect("hits");
    let paths: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.get("path").and_then(Value::as_str))
        .collect();
    assert!(
        paths.contains(&"caller_a.py") && paths.contains(&"caller_b.py"),
        "callers reached through an unresolvable module import must still be reported: {paths:?}"
    );
    let resolved_total = callers
        .get("resolved_total")
        .and_then(Value::as_u64)
        .expect("resolved_total must always be reported so the agent can tell proven from complete");
    assert!(
        resolved_total <= reference_total,
        "resolved_total is a LOWER BOUND on the truth — it may never exceed total: {callers}"
    );
    let _ = service.cancel().await;
}
