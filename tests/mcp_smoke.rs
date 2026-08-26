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
use serde_json::{Value, json};
use tempfile::TempDir;

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

/// SEP-2106 contract: every tool result carries `structured_content` equal to the parsed JSON
/// text mirror, so typed clients get the same payload without re-parsing the text block.
fn assert_structured_matches_text(result: &CallToolResult) {
    let structured = result
        .structured_content
        .as_ref()
        .expect("tool result must carry SEP-2106 structured_content");
    assert_eq!(
        structured,
        &decode_text(result),
        "structured_content must match the JSON text mirror"
    );
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

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let instructions = service
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .unwrap_or_default();
    assert!(
        instructions.contains("basemind first"),
        "server instructions should state the prefer-basemind-over-grep directive: {instructions}"
    );
    assert!(
        instructions.contains("fraction of the tokens"),
        "server instructions should state the context-economy rationale: {instructions}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("admin", json!({ "mode": "status"})))
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

    let outline_result = service
        .call_tool(call_params(
            "code",
            json!({ "mode": "outline", "path": "a.rs", "l2": false }),
        ))
        .await
        .expect("outline");
    assert_structured_matches_text(&outline_result);
    let body = decode_text(&outline_result);
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
            .call_tool(call_params(
                "code",
                json!({ "mode": "symbols", "needle": "Greet", "limit": 10 }),
            ))
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
        .call_tool(call_params(
            "code",
            json!({ "mode": "symbols", "needle": "draw", "limit": 50 }),
        ))
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
            "code",
            json!({ "mode": "symbols", "needle": "draw", "limit": 50, "format": "toon" }),
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
                "git",
                json!({ "mode": "recent", "limit": 5, "include_files": true }),
            ))
            .await
            .expect("git recent"),
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
                "git",
                json!({ "mode": "search", "pattern": "tweak", "field": "message", "limit": 10 }),
            ))
            .await
            .expect("git search"),
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
                "git",
                json!({ "mode": "search", "pattern": "tweak", "field": "author" }),
            ))
            .await
            .expect("git search author scope"),
    );
    assert_eq!(
        body.get("commits").and_then(Value::as_array).map(Vec::len),
        Some(0),
        "'tweak' is a message token, not an author token"
    );
    let body = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({ "mode": "search", "pattern": "tweak alpha", "field": "all" }),
            ))
            .await
            .expect("git search AND same-commit"),
    );
    assert_eq!(
        body.get("commits").and_then(Value::as_array).map(Vec::len),
        Some(1),
        "'tweak' AND 'alpha' both in the 'tweak alpha' commit"
    );
    let body = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({ "mode": "search", "pattern": "init tweak", "field": "all" }),
            ))
            .await
            .expect("git search AND cross-commit"),
    );
    assert_eq!(
        body.get("commits").and_then(Value::as_array).map(Vec::len),
        Some(0),
        "'init' (c1) AND 'tweak' (c2) share no commit"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({ "mode": "symbol_history", "path": "a.rs", "name": "alpha", "limit": 10 }),
            ))
            .await
            .expect("git symbol_history"),
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
                "git",
                json!({
                    "mode": "symbol_history",
                    "path": "a.rs",
                    "name": "alpha",
                    "limit": 10,
                    "hash_mode": "structural"
                }),
            ))
            .await
            .expect("git symbol_history(structural)"),
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
            .call_tool(call_params(
                "code",
                json!({ "mode": "references", "name": "alpha", "limit": 100 }),
            ))
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
            .call_tool(call_params(
                "code",
                json!({ "mode": "references", "name": "alpha", "limit": 2 }),
            ))
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
                "code",
                json!({ "mode": "references", "name": "alpha", "limit": 2, "cursor": cursor1 }),
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
            .call_tool(call_params(
                "code",
                json!({ "mode": "callers", "path": "a.rs", "name": "alpha" }),
            ))
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
                "code",
                json!({ "mode": "callers", "path": "a.rs", "name": "alpha", "limit": 2 }),
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
                "code",
                json!({ "mode": "callers",
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
                "code",
                json!({ "mode": "definition", "path": "a.rs", "line": 1, "column": 0 }),
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
                "graph",
                json!({ "mode": "calls", "name": "inner", "direction": "callers", "max_depth": 2 }),
            ))
            .await
            .expect("graph calls callers"),
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

    // callees over codegraph: `caller()` (c.rs) invokes `alpha`, `zed`, and the undefined ~keep
    // `other()`. Resolved in-repo callees surface; the unresolved external call carries no ~keep
    // graph edge, so it is not a node — the resolved-only contract this migration locks in. ~keep
    let body = decode_text(
        &service
            .call_tool(call_params(
                "graph",
                json!({ "mode": "calls", "name": "caller", "direction": "callees", "max_depth": 1 }),
            ))
            .await
            .expect("graph calls callees"),
    );
    let nodes = body.get("nodes").and_then(Value::as_array).expect("callee nodes");
    let names: Vec<String> = nodes
        .iter()
        .filter_map(|n| n.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    assert_eq!(
        nodes[0].get("name").and_then(Value::as_str),
        Some("caller"),
        "callees root is `caller`: {names:?}"
    );
    assert!(
        names.contains(&"alpha".to_string()) && names.contains(&"zed".to_string()),
        "callees must surface the resolved in-repo targets `alpha` and `zed`: {names:?}"
    );
    assert!(
        !names.contains(&"other".to_string()),
        "an unresolved external call (`other`) carries no graph edge and must not appear: {names:?}"
    );

    let sym = decode_text(
        &service
            .call_tool(call_params(
                "graph",
                json!({ "mode": "map", "granularity": "symbol", "include_churn": false }),
            ))
            .await
            .expect("graph map symbol"),
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
                "graph",
                json!({ "mode": "map", "granularity": "file", "include_churn": false }),
            ))
            .await
            .expect("graph map file"),
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
                "graph",
                json!({ "mode": "map", "granularity": "file", "include_churn": false }),
            ))
            .await
            .expect("graph map file (repeat)"),
    );
    assert_eq!(
        filem.get("nodes"),
        filem2.get("nodes"),
        "architecture_map file-tier node order must be deterministic across calls"
    );

    // Module tier is the DEFAULT granularity and the only path through the directory-rollup grouping
    // (`assign_file_groups`); exercise it end-to-end for node emission + cross-call determinism.
    let modm = decode_text(
        &service
            .call_tool(call_params(
                "graph",
                json!({ "mode": "map", "granularity": "module", "include_churn": false }),
            ))
            .await
            .expect("graph map module"),
    );
    let mod_nodes = modm.get("nodes").and_then(Value::as_array).expect("module nodes");
    assert!(!mod_nodes.is_empty(), "module tier must emit grouped nodes: {modm:?}");
    let modm2 = decode_text(
        &service
            .call_tool(call_params(
                "graph",
                json!({ "mode": "map", "granularity": "module", "include_churn": false }),
            ))
            .await
            .expect("graph map module (repeat)"),
    );
    assert_eq!(
        modm.get("nodes"),
        modm2.get("nodes"),
        "architecture_map module-tier node order must be deterministic across calls"
    );

    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "code",
                json!({ "mode": "symbols", "needle": "a", "limit": 1 }),
            ))
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
                "code",
                json!({ "mode": "symbols", "needle": "a", "limit": 1, "cursor": cursor1 }),
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
            .call_tool(call_params(
                "code",
                json!({ "mode": "symbols", "needle": "a", "limit": 100 }),
            ))
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
                "code",
                json!({ "mode": "symbols", "needle": "a", "limit": 100, "max_tokens": 1 }),
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
            .call_tool(call_params("code", json!({ "mode": "files", "limit": 4 })))
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
            .call_tool(call_params(
                "code",
                json!({ "mode": "files", "limit": 4, "cursor": cursor1 }),
            ))
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
            .call_tool(call_params(
                "code",
                json!({ "mode": "symbols", "needle": "a", "limit": 1 }),
            ))
            .await
            .expect("search_symbols pre-rescan"),
    );
    let stale_cursor = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("pre-rescan cursor")
        .to_string();
    let _ = service
        .call_tool(call_params("admin", json!({ "mode": "rescan"})))
        .await
        .expect("rescan");
    let stale_response = decode_text(
        &service
            .call_tool(call_params(
                "code",
                json!({ "mode": "symbols", "needle": "a", "limit": 1, "cursor": stale_cursor }),
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
            .call_tool(call_params("code", json!({ "mode": "files", "limit": 1 })))
            .await
            .expect("list_files pre-rescan"),
    );
    let stale_cursor = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("list_files pre-rescan cursor")
        .to_string();
    let _ = service
        .call_tool(call_params("admin", json!({ "mode": "rescan"})))
        .await
        .expect("rescan");
    let stale_response = decode_text(
        &service
            .call_tool(call_params(
                "code",
                json!({ "mode": "files", "limit": 1, "cursor": stale_cursor }),
            ))
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
            .call_tool(call_params("code", json!({ "mode": "find", "query": "cy1" })))
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
            .call_tool(call_params(
                "code",
                json!({ "mode": "find", "query": "zzzznonexistentqueryxyz" }),
            ))
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
                "code",
                json!({ "mode": "find", "query": "rs", "language": "python" }),
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
            .call_tool(call_params("code", json!({ "mode": "find", "query": "d" })))
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
                "code",
                json!({ "mode": "references", "name": "no_such_callee_anywhere" }),
            ))
            .await
            .expect("find_references(missing)"),
    );
    let hits = body.get("hits").and_then(Value::as_array).expect("hits");
    assert!(hits.is_empty(), "unknown callee should yield no hits");

    let body = decode_text(
        &service
            .call_tool(call_params("git", json!({ "mode": "blame", "path": "a.rs" })))
            .await
            .expect("git blame"),
    );
    let hunks = body.get("hunks").and_then(Value::as_array).expect("hunks");
    assert!(!hunks.is_empty(), "blame should return hunks on a real file");

    let body = decode_text(
        &service
            .call_tool(call_params(
                "code",
                json!({ "mode": "grep", "pattern": "pub fn", "include_context": false }),
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
                "code",
                json!({ "mode": "grep", "pattern": "pub fn", "limit": 1, "include_context": false }),
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
        .call_tool(call_params(
            "code",
            json!({ "mode": "grep", "pattern": "[invalid_regex(" }),
        ))
        .await;
    assert!(
        invalid_result.is_err(),
        "invalid regex should produce a protocol-level MCP error"
    );

    let _ = service
        .call_tool(call_params(
            "memory",
            json!({ "mode": "put",  "key": "smoke_key", "value": "hello", "embed": false }),
        ))
        .await;
    let _ = service
        .call_tool(call_params("memory", json!({ "mode": "get",  "key": "smoke_key" })))
        .await;
    let _ = service.call_tool(call_params("memory", json!({ "mode": "list"}))).await;
    let _ = service
        .call_tool(call_params("memory", json!({ "mode": "delete",  "key": "smoke_key" })))
        .await;
    let _ = service
        .call_tool(call_params("memory", json!({ "mode": "documents",  "query": "hello" })))
        .await;

    #[cfg(not(feature = "code-search"))]
    {
        let sc = service
            .call_tool(call_params("code", json!({ "mode": "semantic", "query": "hello" })))
            .await;
        assert!(
            sc.is_err(),
            "search_code without the code-search feature must return an MCP error"
        );
        let gc = service
            .call_tool(call_params("code", json!({ "mode": "chunk", "path": "src/lib.rs" })))
            .await;
        assert!(
            gc.is_err(),
            "get_chunk without the code-search feature must return an MCP error"
        );
    }
    #[cfg(feature = "code-search")]
    {
        let sc = service
            .call_tool(call_params("code", json!({ "mode": "semantic", "query": "hello" })))
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
            .call_tool(call_params("code", json!({ "mode": "chunk", "path": "a.rs" })))
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
                "code",
                json!({ "mode": "semantic", "query": "hello", "lane": "keyword" }),
            ))
            .await;
        if let Ok(result) = &kw {
            let body = decode_text(result);
            assert_eq!(
                body.get("query").and_then(Value::as_str),
                Some("hello"),
                "the keyword lane must echo the query field: {body}"
            );
            assert!(
                body.get("hits").and_then(Value::as_array).is_some(),
                "the keyword lane response must carry a hits array: {body}"
            );
        }

        let bad_mode = service
            .call_tool(call_params(
                "code",
                json!({ "mode": "semantic", "query": "hello", "lane": "bogus" }),
            ))
            .await;
        assert!(
            bad_mode.is_err(),
            "`code` mode=\"semantic\" must reject an unknown lane with an MCP error"
        );

        let hy = service
            .call_tool(call_params(
                "code",
                json!({ "mode": "semantic", "query": "hello", "lane": "hybrid", "rerank": false, "rerank_preset": "bge-reranker-base" }),
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
            "memory",
            json!({ "mode": "documents",  "query": "hello", "reranker_preset": "bge-reranker-base" }),
        ))
        .await;
    if let Ok(r) = &override_result {
        let _ = r;
    }

    #[cfg(feature = "documents")]
    {
        let json_result = service
            .call_tool(call_params("memory", json!({ "mode": "documents",  "query": "hello" })))
            .await;
        let toon_result = service
            .call_tool(call_params(
                "memory",
                json!({ "mode": "documents",  "query": "hello", "output_format": "toon" }),
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
                "memory",
                json!({ "mode": "documents",  "query": "hello", "output_format": "toon" }),
            ))
            .await;
    }

    #[cfg(feature = "memory")]
    {
        for i in 0..3 {
            let _ = service
                .call_tool(call_params(
                    "memory",
                    json!({ "mode": "put",
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
                    "memory",
                    json!({ "mode": "list",  "prefix": "paging_key_", "limit": 2 }),
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
                    "memory",
                    json!({ "mode": "list",
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
                "memory",
                json!({ "mode": "put",
                    "key": "audit_probe",
                    "value": "a memory note with no code refs",
                    "embed": false,
                }),
            ))
            .await
            .expect("memory_put audit_probe");

        let body = decode_text(
            &service
                .call_tool(call_params("memory", json!({ "mode": "audit",  "key": "audit_probe" })))
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
                    "memory",
                    json!({ "mode": "audit",  "key": "audit_probe", "dry_run": true }),
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
                .call_tool(call_params("memory", json!({ "mode": "audit",  "limit": 50 })))
                .await
                .expect("memory_audit range"),
        );
        let range_audited = range_body.get("audited").and_then(Value::as_u64).expect("audited");
        assert!(
            range_audited >= 1,
            "range audit must cover at least the audit_probe key: {range_body}"
        );

        let _ = service
            .call_tool(call_params(
                "memory",
                json!({ "mode": "delete",  "key": "audit_probe" }),
            ))
            .await
            .expect("memory_delete audit_probe");
    }

    #[cfg(feature = "memory")]
    {
        let mine_body = decode_text(
            &service
                .call_tool(call_params("memory", json!({ "mode": "mine"})))
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
                .call_tool(call_params(
                    "memory",
                    json!({ "mode": "proposals",  "kind": "skill", "limit": 50 }),
                ))
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
                    "memory",
                    json!({ "mode": "mine",
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
                .call_tool(call_params("memory", json!({ "mode": "proposals",  "limit": 10 })))
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
                .call_tool(call_params("memory", json!({ "mode": "accept",  "id": accept_id })))
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
                .call_tool(call_params("memory", json!({ "mode": "audit",  "key": &memory_key })))
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
            .call_tool(call_params("admin", json!({ "mode": "rescan"})))
            .await
            .expect("rescan after file deletion");
        let stale_audit = decode_text(
            &service
                .call_tool(call_params(
                    "memory",
                    json!({ "mode": "audit",  "key": &memory_key, "dry_run": true }),
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
            .call_tool(call_params("admin", json!({ "mode": "rescan"})))
            .await
            .expect("rescan after restore");
        let _ = service
            .call_tool(call_params("memory", json!({ "mode": "delete",  "key": &memory_key })))
            .await;

        let mine_e = decode_text(
            &service
                .call_tool(call_params(
                    "memory",
                    json!({ "mode": "mine",
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
                .call_tool(call_params("memory", json!({ "mode": "proposals",  "limit": 10 })))
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
                    "memory",
                    json!({ "mode": "reject",  "id": reject_id, "reason": "smoke-test rejection" }),
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
                    "memory",
                    json!({ "mode": "mine",
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
            .call_tool(call_params("admin", json!({ "mode": "rescan"})))
            .await
            .expect("rescan"),
    );
    let scanned = body.get("scanned").and_then(Value::as_u64).expect("scanned");
    assert!(scanned > 0, "rescan should walk at least the fixture files");

    let body = decode_text(
        &service
            .call_tool(call_params(
                "admin",
                json!({ "mode": "rescan",  "full": true, "paths": ["does-not-exist.rs"] }),
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
            .call_tool(call_params("admin", json!({ "mode": "rescan",  "paths": ["a.rs"] })))
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
        .call_tool(call_params(
            "code",
            json!({ "mode": "symbols", "needle": "Beta", "limit": 5 }),
        ))
        .await
        .expect("search_symbols to seed a sub-millisecond telemetry row");

    let body = decode_text(
        &service
            .call_tool(call_params("admin", json!({ "mode": "telemetry",  "window": "all" })))
            .await
            .expect("telemetry_summary"),
    );
    let total_calls = body.get("total_calls").and_then(Value::as_u64).expect("total_calls");
    assert!(
        total_calls >= 4,
        "telemetry_summary should see at least the prior fixture calls (admin:status/outline/search_symbols/recent_changes), got {total_calls}"
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
        "code:find",
        "code:references",
        "code:symbols",
        "code:grep",
        "code:outline",
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
            .call_tool(call_params("admin", json!({ "mode": "cache_stats"})))
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
            .call_tool(call_params("admin", json!({ "mode": "gc"})))
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
            .call_tool(call_params(
                "admin",
                json!({ "mode": "cache_clear",  "component": "telemetry" }),
            ))
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
        .call_tool(call_params(
            "admin",
            json!({ "mode": "cache_clear",  "component": "blobs" }),
        ))
        .await;
    assert!(
        err.is_err(),
        "clearing `blobs` without confirm=true must be rejected, got: {err:?}"
    );

    for component in ["views", "all"] {
        let err = service
            .call_tool(call_params(
                "admin",
                json!({ "mode": "cache_clear",  "component": component, "confirm": true }),
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
                "code",
                json!({ "mode": "implementations", "trait_name": "Drawable", "limit": 100 }),
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
                "code",
                json!({ "mode": "implementations", "trait_name": "Foo", "limit": 100 }),
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
                "code",
                json!({ "mode": "implementations", "trait_name": "Drawable", "limit": 1 }),
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
                "code",
                json!({ "mode": "implementations", "trait_name": "Drawable", "limit": 1, "cursor": impl_cursor1 }),
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
                "code",
                json!({ "mode": "implementations", "trait_name": "Drawable", "language": "rust", "limit": 100 }),
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
            .call_tool(call_params(
                "code",
                json!({ "mode": "references", "name": "lph", "limit": 100 }),
            ))
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
                "code",
                json!({ "mode": "implementations", "trait_name": "raw", "limit": 100 }),
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
            .call_tool(call_params(
                "code",
                json!({ "mode": "symbols", "needle": "", "limit": 100 }),
            ))
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
            .call_tool(call_params("admin", json!({ "mode": "compress",  "path": "a.rs" })))
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
            .call_tool(call_params("admin", json!({ "mode": "compress",  "text": prose })))
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
        .call_tool(call_params(
            "admin",
            json!({ "mode": "compress",  "text": "hello", "path": "a.rs" }),
        ))
        .await;
    assert!(
        err.is_err(),
        "compress with both text and path must be rejected: {err:?}"
    );

    let err = service
        .call_tool(call_params("admin", json!({ "mode": "compress"})))
        .await;
    assert!(
        err.is_err(),
        "compress with neither text nor path must be rejected: {err:?}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "code",
                json!({ "mode": "expand", "path": "a.rs", "name": "alpha" }),
            ))
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
            "code",
            json!({ "mode": "expand", "path": "a.rs", "name": "nonexistent_symbol_xyz" }),
        ))
        .await;
    assert!(err.is_err(), "expand with unknown symbol must be rejected: {err:?}");

    let body = decode_text(
        &service
            .call_tool(call_params(
                "code",
                json!({ "mode": "expand", "path": "a.rs", "symbol": "alpha" }),
            ))
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
                "admin",
                json!({ "mode": "delta",
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
            .call_tool(call_params(
                "admin",
                json!({ "mode": "delta",  "old": "same\n", "new": "same\n" }),
            ))
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
                "admin",
                json!({ "mode": "checkpoint",  "text": "We decided to use rayon.\nerror: build failed\n" }),
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
            .call_tool(call_params("admin", json!({ "mode": "waste",  "log": log })))
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
    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
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
async fn git_recent_paginates_with_stable_cursor() {
    let (_dir, service) = spawn_paging_server().await;
    let page1 = decode_text(
        &service
            .call_tool(call_params("git", json!({ "mode": "recent", "limit": 2 })))
            .await
            .expect("git recent page1"),
    );
    let p1_shas = commit_shas(&page1);
    assert_eq!(p1_shas.len(), 2, "git recent limit=2 → 2 commits");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("git recent page1 must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({ "mode": "recent", "limit": 2, "cursor": cursor1 }),
            ))
            .await
            .expect("git recent page2"),
    );
    let p2_shas = commit_shas(&page2);
    assert_eq!(p2_shas.len(), 2, "git recent page2 → 2 more commits");
    assert!(
        p2_shas.iter().all(|s| !p1_shas.contains(s)),
        "git recent pages must not overlap: {p1_shas:?} vs {p2_shas:?}"
    );
    let bogus = basemind::testing::encode_in_memory_cursor(0, 0xDEAD_BEEF);
    let stale = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({ "mode": "recent", "limit": 2, "cursor": bogus }),
            ))
            .await
            .expect("git recent stale"),
    );
    assert_eq!(
        stale.get("cursor_invalidated"),
        Some(&Value::Bool(true)),
        "bogus snapshot must surface cursor_invalidated: {stale}"
    );
    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_touching_paginates_with_stable_cursor() {
    let (_dir, service) = spawn_paging_server().await;
    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({ "mode": "touching", "path": "paged.rs", "limit": 2 }),
            ))
            .await
            .expect("git touching page1"),
    );
    let p1_shas = commit_shas(&page1);
    assert_eq!(p1_shas.len(), 2, "git touching page1 → 2 commits");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("git touching must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({ "mode": "touching", "path": "paged.rs", "limit": 2, "cursor": cursor1 }),
            ))
            .await
            .expect("git touching page2"),
    );
    let p2_shas = commit_shas(&page2);
    assert_eq!(p2_shas.len(), 2, "git touching page2 → 2 more commits");
    assert!(
        p2_shas.iter().all(|s| !p1_shas.contains(s)),
        "git touching pages must not overlap: {p1_shas:?} vs {p2_shas:?}"
    );
    let bogus = basemind::testing::encode_in_memory_cursor(0, 0xDEAD_BEEF);
    let stale = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({ "mode": "touching", "path": "paged.rs", "limit": 2, "cursor": bogus }),
            ))
            .await
            .expect("git touching stale"),
    );
    assert_eq!(
        stale.get("cursor_invalidated"),
        Some(&Value::Bool(true)),
        "bogus snapshot must surface cursor_invalidated: {stale}"
    );
    let _ = service.cancel().await;
}

/// `git` mode `churn` aggregates the churn window. The paging fixture touches `paged.rs` in all 5
/// commits, so it must rank first with `commits_touching == 5`. Exercises the tool end-to-end
/// through the real `serve` subprocess (whichever of the git-history index / live-walk path is
/// active — both must agree).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_churn_ranks_the_churned_file_first() {
    let (_dir, service) = spawn_paging_server().await;
    let body = decode_text(
        &service
            .call_tool(call_params("git", json!({ "mode": "churn", "window": 50, "top_k": 5 })))
            .await
            .expect("git churn"),
    );
    let files = body
        .get("files")
        .and_then(Value::as_array)
        .expect("git churn returns a files array");
    assert!(!files.is_empty(), "git churn returns entries: {body}");
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
async fn git_by_path_paginates_with_stable_cursor() {
    let (_dir, service) = spawn_paging_server().await;
    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({ "mode": "by_path", "pattern": "paged\\.rs", "window": 50, "limit": 2 }),
            ))
            .await
            .expect("git by_path page1"),
    );
    let p1_shas = commit_shas(&page1);
    assert_eq!(p1_shas.len(), 2, "git by_path page1 → 2 commits: {page1}");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("git by_path must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({
                    "mode": "by_path",
                    "pattern": "paged\\.rs",
                    "window": 50,
                    "limit": 2,
                    "cursor": cursor1,
                }),
            ))
            .await
            .expect("git by_path page2"),
    );
    let p2_shas = commit_shas(&page2);
    assert!(!p2_shas.is_empty(), "git by_path page2 must have ≥ 1 commit: {page2}");
    assert!(
        p2_shas.iter().all(|s| !p1_shas.contains(s)),
        "git by_path pages must not overlap: {p1_shas:?} vs {p2_shas:?}"
    );
    let bogus = basemind::testing::encode_in_memory_cursor(0, 0xDEAD_BEEF);
    let stale = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({
                    "mode": "by_path",
                    "pattern": "paged\\.rs",
                    "window": 50,
                    "limit": 2,
                    "cursor": bogus,
                }),
            ))
            .await
            .expect("git by_path stale"),
    );
    assert_eq!(
        stale.get("cursor_invalidated"),
        Some(&Value::Bool(true)),
        "bogus snapshot must surface cursor_invalidated: {stale}"
    );
    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_symbol_history_paginates_with_stable_cursor() {
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
                "git",
                json!({ "mode": "symbol_history", "path": "paged.rs", "name": "paged", "limit": 2 }),
            ))
            .await
            .expect("git symbol_history page1"),
    );
    let p1_shas = history_shas(&page1);
    assert_eq!(p1_shas.len(), 2, "git symbol_history page1 → 2 entries: {page1}");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("git symbol_history must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({
                    "mode": "symbol_history",
                    "path": "paged.rs",
                    "name": "paged",
                    "limit": 2,
                    "cursor": cursor1,
                }),
            ))
            .await
            .expect("git symbol_history page2"),
    );
    let p2_shas = history_shas(&page2);
    assert!(
        !p2_shas.is_empty(),
        "git symbol_history page2 must have ≥ 1 entry: {page2}"
    );
    assert!(
        p2_shas.iter().all(|s| !p1_shas.contains(s)),
        "git symbol_history pages must not overlap: {p1_shas:?} vs {p2_shas:?}"
    );
    let bogus = basemind::testing::encode_in_memory_cursor(0, 0xDEAD_BEEF);
    let stale = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({
                    "mode": "symbol_history",
                    "path": "paged.rs",
                    "name": "paged",
                    "limit": 2,
                    "cursor": bogus,
                }),
            ))
            .await
            .expect("git symbol_history stale"),
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
async fn git_blame_paginates_by_start_line() {
    let (dir, service) = spawn_paging_server().await;
    split_blame_lines(dir.path());
    let _ = service
        .call_tool(call_params("admin", json!({ "mode": "rescan"})))
        .await;
    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({ "mode": "blame", "path": "paged.rs", "limit": 1 }),
            ))
            .await
            .expect("git blame page1"),
    );
    let p1_hunks = page1
        .get("hunks")
        .and_then(Value::as_array)
        .expect("git blame page1 hunks");
    assert_eq!(p1_hunks.len(), 1, "git blame limit=1 → 1 hunk: {page1}");
    let p1_start: Vec<u64> = p1_hunks
        .iter()
        .filter_map(|h| h.get("start_line").and_then(Value::as_u64))
        .collect();
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("git blame must carry next_cursor when more hunks remain")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({ "mode": "blame", "path": "paged.rs", "limit": 1, "cursor": cursor1 }),
            ))
            .await
            .expect("git blame page2"),
    );
    let p2_hunks = page2
        .get("hunks")
        .and_then(Value::as_array)
        .expect("git blame page2 hunks");
    assert!(!p2_hunks.is_empty(), "git blame page2 must have ≥ 1 hunk: {page2}");
    let p2_start: Vec<u64> = p2_hunks
        .iter()
        .filter_map(|h| h.get("start_line").and_then(Value::as_u64))
        .collect();
    assert!(
        p2_start.iter().all(|s| !p1_start.contains(s)),
        "git blame pages must not overlap by start_line: {p1_start:?} vs {p2_start:?}"
    );
    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_blame_symbol_paginates_by_start_line() {
    let (dir, service) = spawn_paging_server().await;
    split_blame_lines(dir.path());
    let _ = service
        .call_tool(call_params("admin", json!({ "mode": "rescan"})))
        .await;
    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({ "mode": "blame_symbol", "path": "paged.rs", "name": "paged", "limit": 1 }),
            ))
            .await
            .expect("git blame_symbol page1"),
    );
    let p1_hunks = page1
        .get("hunks")
        .and_then(Value::as_array)
        .expect("git blame_symbol page1 hunks");
    assert_eq!(p1_hunks.len(), 1, "git blame_symbol limit=1 → 1 hunk: {page1}");
    let p1_start = p1_hunks
        .iter()
        .filter_map(|h| h.get("start_line").and_then(Value::as_u64))
        .next()
        .expect("git blame_symbol page1 start_line");
    assert!(
        p1_start >= 1,
        "git blame_symbol start_line should be 1-based, got {p1_start}"
    );
    let huge_cursor = basemind::testing::encode_in_memory_cursor(9_999, 0);
    let page_empty = decode_text(
        &service
            .call_tool(call_params(
                "git",
                json!({
                    "mode": "blame_symbol",
                    "path": "paged.rs",
                    "name": "paged",
                    "limit": 1,
                    "cursor": huge_cursor,
                }),
            ))
            .await
            .expect("git blame_symbol cursor past end"),
    );
    let empty_hunks = page_empty
        .get("hunks")
        .and_then(Value::as_array)
        .expect("git blame_symbol empty page hunks");
    assert!(
        empty_hunks.is_empty(),
        "git blame_symbol with cursor past end should be empty: {page_empty}"
    );
    assert!(
        page_empty.get("next_cursor").is_none(),
        "git blame_symbol exhausted page must NOT carry next_cursor"
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
    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let no_rerank = service
        .call_tool(call_params(
            "memory",
            json!({ "mode": "documents",  "query": "function", "reranker_enabled": false }),
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
            "memory",
            json!({ "mode": "documents",
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

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let result = service
        .call_tool(call_params(
            "memory",
            json!({ "mode": "documents",
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

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let result = service
        .call_tool(call_params(
            "memory",
            json!({ "mode": "documents",
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

    // `wait_inbox` never marks read (per its tool contract) — the caller acks once it has handled ~keep
    // the page. Advance a's read cursor past b's post so the next wait sees a genuinely empty inbox ~keep
    // and blocks to the timeout rather than re-delivering the same message. ~keep
    a.read_inbox(None, None, None, 100, true, None)
        .await
        .expect("mark the delivered page read");

    let (timed_out2, rows2, _unread2, _next2) = a
        .wait_inbox(None, None, None, None, None, 100, std::time::Duration::from_millis(300))
        .await
        .expect("second wait_inbox");
    assert!(timed_out2, "no new post landed; the second wait must time out");
    assert!(rows2.is_empty(), "a timed-out wait returns no rows");

    let _ = shutdown_tx.send(true);
    let _ = serve.await;
}

/// End-to-end MCP contract for the `shell` domain tool through a real
/// `basemind serve` child process. The child binary carries the
/// `--__internal-daemon` intercept, so mode `spawn` actually re-execs basemind
/// as the embedded rmux daemon. `BASEMIND_SHELLS_SOCKET` sandboxes that daemon on
/// a per-test temp socket so parallel runs and the user's environment never
/// collide.
///
/// Proves the wired surface: `spawn` → poll `capture` until the sentinel
/// appears → `kill`.
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

    // SAFETY: single-threaded test setup before the in-process server touches rmux; this is the ~keep
    // only shells test in the binary, so no sibling test observes these vars concurrently. ~keep
    //   - BASEMIND_SHELLS_SOCKET sandboxes the embedded daemon on a per-test socket. ~keep
    //   - point_sdk_daemon_at makes the embedded rmux daemon re-exec the real `basemind` binary; ~keep
    //     `current_exe()` here is the test harness, which cannot host the daemon (the child-process ~keep
    //     serve used to supply this via `main`). ~keep
    unsafe {
        std::env::set_var("BASEMIND_SHELLS_SOCKET", dir.path().join("shells.sock"));
        basemind::shells::daemon::point_sdk_daemon_at(std::path::Path::new(env!("CARGO_BIN_EXE_basemind")));
        // The embedded rmux daemon is a cold re-exec of the ~1 GB `--features full` debug binary; ~keep
        // paging it in on a busy machine can exceed the rmux SDK's 5 s default startup deadline (a warm ~keep
        // binary answers in ~50 ms). Production re-execs the already-resident daemon binary, so this ~keep
        // generous timeout only accommodates the test's cold-start; it never masks a real hang. ~keep
        std::env::set_var("RMUX_SDK_TIMEOUT_MS", "60000");
    }
    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let spawned = service
        .call_tool(call_params(
            "shell",
            json!({ "mode": "spawn", "command": "echo basemind-hi", "cwd": "." }),
        ))
        .await
        .expect("shell spawn call");
    let spawned = decode_text(&spawned);
    let session_id = spawned
        .get("session_id")
        .and_then(Value::as_str)
        .expect("session_id in shell spawn response")
        .to_string();
    let attach_command = spawned
        .get("attach_command")
        .and_then(Value::as_str)
        .expect("attach_command in shell spawn response");
    assert!(
        attach_command.contains("--__internal-attach ")
            && attach_command.contains("--socket ")
            && attach_command.contains("--size "),
        "attach_command should be a basemind internal-attach re-exec line: {spawned:?}"
    );

    let escaped = service
        .call_tool(call_params(
            "shell",
            json!({ "mode": "spawn", "command": "true", "cwd": "../../../etc" }),
        ))
        .await;
    assert!(
        escaped.is_err(),
        "shell mode=spawn must reject a cwd escaping the repository root: {escaped:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let listed = service
            .call_tool(call_params("shell", json!({ "mode": "list" })))
            .await
            .expect("shell list call while waiting for completion");
        let completed = decode_text(&listed)
            .get("sessions")
            .and_then(Value::as_array)
            .is_some_and(|sessions| {
                sessions.iter().any(|session| {
                    session.get("session_id").and_then(Value::as_str) == Some(session_id.as_str())
                        && session.get("alive").and_then(Value::as_bool) == Some(false)
                })
            });
        if completed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the one-line shell command to complete"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let captured = service
        .call_tool(call_params(
            "shell",
            json!({ "mode": "capture", "session_id": session_id, "lines": 1 }),
        ))
        .await
        .expect("capture completed one-line shell command");
    assert_eq!(
        decode_text(&captured).get("text").and_then(Value::as_str),
        Some("basemind-hi"),
        "shell capture must preserve the only output row after completion"
    );

    let oversized = service
        .call_tool(call_params(
            "shell",
            json!({ "mode": "capture", "session_id": session_id, "lines": 501 }),
        ))
        .await;
    assert!(
        oversized.is_err(),
        "shell capture must reject an unbounded line request"
    );

    let killed = service
        .call_tool(call_params(
            "shell",
            json!({ "mode": "kill", "session_id": session_id }),
        ))
        .await
        .expect("shell kill call");
    let killed = decode_text(&killed);
    assert_eq!(
        killed.get("killed").and_then(Value::as_bool),
        Some(true),
        "shell mode=kill should report killed=true for a live session: {killed:?}"
    );

    let second = service
        .call_tool(call_params(
            "shell",
            json!({ "mode": "kill", "session_id": session_id }),
        ))
        .await;
    assert!(second.is_err(), "killing an already-forgotten session_id should error");

    let _ = service.cancel().await;
}

/// Every advertised `shell` mode validates its arguments before touching the shell daemon: the
/// mode is in the advertised schema's enum, a field belonging to another mode is refused, and a
/// mode run without its required field names the exact `mode`/field pair.
///
/// No daemon is sandboxed here on purpose — every assertion is a rejection, so nothing reaches
/// rmux. That is what makes this test cheap enough to cover all six modes.
#[cfg(all(feature = "shells", unix))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_tool_validates_every_mode_before_running_it() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let tools = service.list_all_tools().await.expect("list tools");
    let shell = tools
        .iter()
        .find(|t| t.name == "shell")
        .expect("the shell tool is advertised");
    let modes = shell
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|p| p.get("mode"))
        .and_then(|m| m.get("enum"))
        .and_then(Value::as_array)
        .expect("shell advertises a flat mode enum");
    for expected in ["spawn", "send", "capture", "kill", "list", "broadcast"] {
        assert!(
            modes.iter().any(|m| m.as_str() == Some(expected)),
            "shell mode enum is missing {expected:?}: {modes:?}"
        );
    }

    // `lines` belongs to `capture`; silently ignoring it on a `send` would read to an agent as a
    // send that also captured output.
    let foreign = service
        .call_tool(call_params(
            "shell",
            json!({ "mode": "send", "session_id": "bmsh-nope", "text": "x", "lines": 5 }),
        ))
        .await;
    let foreign = format!("{foreign:?}");
    assert!(
        foreign.contains("lines"),
        "shell mode=send must reject the capture-only `lines`: {foreign}"
    );

    for (mode, field) in [
        ("spawn", "command"),
        ("send", "session_id"),
        ("capture", "session_id"),
        ("kill", "session_id"),
        ("broadcast", "session_ids"),
    ] {
        let missing = service.call_tool(call_params("shell", json!({ "mode": mode }))).await;
        let missing = format!("{missing:?}");
        assert!(
            missing.contains(field) && missing.contains(mode),
            "shell mode={mode} without `{field}` must name the pair: {missing}"
        );
    }

    let _ = service.cancel().await;
}

/// Agent lifecycle modes stay in the flat schema and reject fields belonging to another mode
/// before a cleanup can reach the broker.
#[cfg(all(feature = "comms", unix))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_cleanup_and_status_are_advertised_and_validate_fields() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);
    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let tools = service.list_all_tools().await.expect("list tools");
    let agents = tools
        .iter()
        .find(|tool| tool.name == "agents")
        .expect("agents advertised");
    let modes = agents
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get("mode"))
        .and_then(|mode| mode.get("enum"))
        .and_then(Value::as_array)
        .expect("agents advertises a flat mode enum");
    for expected in ["cleanup", "status"] {
        assert!(
            modes.iter().any(|mode| mode.as_str() == Some(expected)),
            "agents mode enum is missing {expected:?}: {modes:?}"
        );
    }

    let cleanup = service
        .call_tool(call_params(
            "agents",
            json!({ "mode": "cleanup", "thread": "th-invalid" }),
        ))
        .await
        .expect_err("cleanup rejects a thread filter");
    assert!(format!("{cleanup:?}").contains("mode `cleanup` does not accept `thread`"));
    let status = service
        .call_tool(call_params("agents", json!({ "mode": "status", "apply": true })))
        .await
        .expect_err("status rejects cleanup's apply flag");
    assert!(format!("{status:?}").contains("mode `status` does not accept `apply`"));
    let apply = service
        .call_tool(call_params("agents", json!({ "mode": "cleanup", "apply": true })))
        .await
        .expect_err("MCP cleanup is preview-only");
    assert!(format!("{apply:?}").contains("preview-only over MCP"));

    let _ = service.cancel().await;
}

/// Spawn `basemind serve` against `root`, optionally setting `BASEMIND_MCP_LEAN`, and return the
/// connected rmcp client service.
async fn spawn_serve(root: &Path, lean: Option<&str>) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    // Force the lean surface per-server (env-independent) so a lean and a full server can coexist
    // in this one test process. Mirrors `lean_mode_enabled`'s truthiness for the tested values.
    let lean_on = lean.is_some_and(|v| {
        let v = v.trim();
        !(v.is_empty()
            || v.eq_ignore_ascii_case("0")
            || v.eq_ignore_ascii_case("off")
            || v.eq_ignore_ascii_case("false"))
    });
    let transport = basemind::mcp::serve_in_memory_lean(root, "working", lean_on)
        .await
        .expect("in-memory serve");
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
            "admin",
            json!({ "mode": "rescan",  "paths": ["../../../../../../etc/passwd"] }),
        ))
        .await;
    assert!(
        escaping.is_err(),
        "rescan must reject a path that escapes the repo root, got: {escaping:?}"
    );

    let ok = service
        .call_tool(call_params("admin", json!({ "mode": "rescan",  "paths": ["a.rs"] })))
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

    // ~keep Poll for up to 30s: on a loaded Windows runner the boot scan (daemon spawn + named-pipe
    // ~keep comms + scan) can take well over the ~10s a 200-iteration budget allowed, which flaked here.
    let mut settled: Option<Value> = None;
    for _ in 0..600 {
        let result = service
            .call_tool(call_params("admin", json!({ "mode": "status"})))
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
/// SEP-2106 allows exactly ONE `output_schema` per tool, and every one of the nine domain tools has
/// modes returning differently-shaped responses — so none of them advertises one. Expressing the
/// union would mean nested structs, which schemars emits as `$ref` into `$defs`, the construct the
/// Anthropic input_schema subset rejects silently and registry-wide (GH #50).
///
/// This asserts the absence deliberately rather than leaving it untested: a well-meaning "add the
/// output schema back" change would look like an improvement and take the whole registry down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_advertise_output_schema() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let server = spawn_serve(root, None).await;
    let tools = server.list_all_tools().await.expect("list tools");

    let schema_of = |name: &str| {
        tools
            .iter()
            .find(|t| t.name.as_ref() == name)
            .unwrap_or_else(|| panic!("tool {name} present in full surface"))
            .output_schema
            .clone()
    };
    for domain in ["code", "admin", "git", "graph", "memory"] {
        assert!(
            schema_of(domain).is_none(),
            "the consolidated `{domain}` tool must NOT advertise a single output_schema for its modes"
        );
    }

    // Nothing on the advertised surface may carry one: the ban is a property of the surface, not a
    // list of known offenders, so a newly added tool is covered without editing this test.
    let with_schema: Vec<&str> = tools
        .iter()
        .filter(|t| t.output_schema.is_some())
        .map(|t| t.name.as_ref())
        .collect();
    assert!(
        with_schema.is_empty(),
        "no domain tool may advertise an output_schema (SEP-2106 allows one, and each has many \
         response shapes): {with_schema:?}"
    );

    let _ = server.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lean_surface_is_opt_in_and_round_trips_through_invoke_tool() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let full = spawn_serve(root, None).await;
    let full_tools = full.list_all_tools().await.expect("list tools (full)");
    let full_names: Vec<&str> = full_tools.iter().map(|t| t.name.as_ref()).collect();
    // The surface is nine domain tools at most (fewer when `crawl` / `comms` / `shells` are off),
    // which is the whole point of the consolidation: hosts defer tools and surface them by keyword
    // search, so a count that creeps back up is a regression, not growth. Asserted as a ceiling
    // plus the always-on names, rather than a floor that a re-flattening would still satisfy.
    assert!(
        full_tools.len() <= 9,
        "the surface must stay at nine domain tools or fewer, got {}: {full_names:?}",
        full_tools.len()
    );
    for domain in ["code", "graph", "git", "memory", "admin"] {
        assert!(
            full_names.contains(&domain),
            "default surface must advertise the always-on `{domain}` tool: {full_names:?}"
        );
    }
    assert!(
        full_names.contains(&"code"),
        "default surface lists the `code` domain tool: {full_names:?}"
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
    // `code` is the one domain whose every mode is a read, so its annotation union stays read-only.
    assert_eq!(
        annotations_of("code").read_only_hint,
        Some(true),
        "every `code` mode is a read, so the tool must advertise read_only_hint=true"
    );
    // `admin` (formerly `rescan` / `cache_clear` / … as distinct tools) bundles both read-only modes
    // (`status`) and destructive ones (`rescan`, `cache_clear`) behind one tool, so its ONE set of
    // annotations is read_only_hint=false / destructive_hint=true for the whole tool — including its
    // read-only modes. This is an intentional consequence of consolidation, not a regression: a host
    // that gates on tool-level annotations must treat the whole `admin` tool as mutating.
    assert_eq!(
        annotations_of("admin").read_only_hint,
        Some(false),
        "the consolidated `admin` tool must advertise read_only_hint=false"
    );
    assert_eq!(
        annotations_of("admin").destructive_hint,
        Some(true),
        "the consolidated `admin` tool must advertise destructive_hint=true"
    );
    let direct = decode_text(
        &full
            .call_tool(call_params(
                "code",
                json!({ "mode": "symbols", "needle": "Greet", "limit": 10 }),
            ))
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
            .any(|t| t.get("name").and_then(Value::as_str) == Some("code")),
        "lean list_tools should surface the real `code` domain tool: {listing}"
    );

    let schema = decode_text(
        &lean
            .call_tool(call_params("get_tool_schema", json!({ "tool_name": "code" })))
            .await
            .expect("lean get_tool_schema"),
    );
    assert_eq!(
        schema.get("name").and_then(Value::as_str),
        Some("code"),
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
                    "tool_name": "code",
                    "tool_input": { "mode": "symbols", "needle": "Greet", "limit": 10 }
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
            "{label} code:symbols response must carry an `elapsed_us` reading"
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
        body.contains("Greeter") && body.contains("`code` mode `symbols`"),
        "rendered trace-symbol must interpolate the symbol and route to a real tool+mode, got: {body}"
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

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let server = capture.serve(transport).await.expect("rmcp handshake");

    let mut params = call_params("admin", json!({ "mode": "rescan"}));
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
            .call_tool(call_params("code", json!({ "mode": "symbols", "needle": "paged" })))
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
            .call_tool(call_params("git", json!({ "mode": "recent", "limit": 2 })))
            .await
            .expect("recent_changes"),
    );
    assert_eq!(commit_shas(&recent).len(), 2, "fixture has 5 commits; asked for 2");
    assert_stamped("recent_changes", &recent);

    let outline = decode_text(
        &service
            .call_tool(call_params(
                "code",
                json!({ "mode": "outline", "path": "paged.rs", "l2": true }),
            ))
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
            .call_tool(call_params("code", json!({ "mode": "symbols", "needle": "paged" })))
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
            .call_tool(call_params("git", json!({ "mode": "recent", "limit": 2 })))
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
                "code",
                json!({ "mode": "grep", "pattern": RARE, "include_context": false }),
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
                "code",
                json!({ "mode": "grep", "pattern": "pub fn", "include_context": false, "limit": 1000 }),
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
        let mut args = json!({ "mode": "grep", "pattern": "pub fn", "include_context": false, "limit": 1 });
        if let Some(c) = &cursor {
            args["cursor"] = Value::String(c.clone());
        }
        let page = decode_text(
            &service
                .call_tool(call_params("code", args))
                .await
                .expect("code grep (page)"),
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

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let references = decode_text(
        &service
            .call_tool(call_params(
                "code",
                json!({ "mode": "references", "name": "target", "limit": 500 }),
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
                "code",
                json!({ "mode": "callers", "path": "pkg/mod.py", "name": "target", "limit": 500 }),
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

/// ADR-0001/0002: `architecture_map`'s `edges` param is live (calls/imports/inherits) and
/// every edge carries a provenance tag + numeric confidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn architecture_map_emits_typed_provenance_edges() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    std::fs::write(
        root.join("core.rs"),
        "pub fn engine() {}\npub fn helper() { engine(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app.rs"),
        "use crate::core::engine;\npub fn run() { engine(); helper(); }\n",
    )
    .unwrap();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let body = decode_text(
        &service
            .call_tool(call_params(
                "graph",
                json!({ "mode": "map", "granularity": "file", "edges": "all", "include_churn": false}),
            ))
            .await
            .expect("architecture_map"),
    );
    let edges = body.get("edges").and_then(Value::as_array).expect("edges array");
    assert!(!edges.is_empty(), "file tier should have inter-file edges: {body}");

    // Every edge carries a provenance tag on the fixed ladder and a matching confidence.
    for e in edges {
        let prov = e.get("provenance").and_then(Value::as_str).expect("edge provenance");
        let conf = e.get("confidence").and_then(Value::as_f64).expect("edge confidence");
        match prov {
            "extracted" => assert_eq!(conf, 1.0),
            "inferred" => assert_eq!(conf, 0.5),
            "ambiguous" => assert_eq!(conf, 0.2),
            other => panic!("unexpected provenance tag {other:?} in {e}"),
        }
    }

    // `edges: "all"` surfaces the previously-dead import lane, not just calls.
    let has_import = edges
        .iter()
        .any(|e| e.get("kind").and_then(Value::as_str) == Some("imports"));
    assert!(
        has_import,
        "edges=all must surface import edges beyond calls: {edges:?}"
    );

    let _ = service.cancel().await;
}

/// Assert every edge in a traversal payload carries a provenance tag on the fixed ladder with
/// a matching confidence, and that its `from`/`to` index valid nodes.
fn assert_graph_edges_well_formed(body: &Value) {
    let nodes = body.get("nodes").and_then(Value::as_array).expect("nodes array");
    let edges = body.get("edges").and_then(Value::as_array).expect("edges array");
    for e in edges {
        let prov = e.get("provenance").and_then(Value::as_str).expect("edge provenance");
        let conf = e.get("confidence").and_then(Value::as_f64).expect("edge confidence");
        match prov {
            "extracted" => assert_eq!(conf, 1.0),
            "inferred" => assert_eq!(conf, 0.5),
            "ambiguous" => assert_eq!(conf, 0.2),
            other => panic!("unexpected provenance tag {other:?} in {e}"),
        }
        let from = e.get("from").and_then(Value::as_u64).expect("edge from") as usize;
        let to = e.get("to").and_then(Value::as_u64).expect("edge to") as usize;
        assert!(
            from < nodes.len() && to < nodes.len(),
            "edge endpoints index into nodes: {e}"
        );
    }
}

/// ADR-0003: the `neighbors`, `path`, and `subgraph` traversal tools walk the shared code-graph
/// and report typed, provenance-tagged edges.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traversal_tools_walk_the_shared_graph() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    // Call chain across files: run -> helper -> engine. ~keep
    std::fs::write(
        root.join("core.rs"),
        "pub fn engine() {}\npub fn helper() { engine(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app.rs"),
        "use crate::core::helper;\npub fn run() { helper(); }\n",
    )
    .unwrap();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    // neighbors(helper, both): reaches engine (helper->engine) and run (run->helper). ~keep
    let neighbors = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "neighbors", "name": "helper", "direction": "both", "depth": 2, "edges": "all"}),
        ))
        .await
        .expect("neighbors");
    assert_structured_matches_text(&neighbors);
    let body = decode_text(&neighbors);
    let nodes = body.get("nodes").and_then(Value::as_array).expect("nodes");
    assert_eq!(
        nodes[0].get("name").and_then(Value::as_str),
        Some("helper"),
        "root is the first node: {body}"
    );
    let reached: Vec<&str> = nodes
        .iter()
        .filter_map(|n| n.get("name").and_then(Value::as_str))
        .collect();
    assert!(reached.contains(&"engine"), "helper should reach engine: {reached:?}");
    assert!(
        reached.contains(&"run"),
        "run should reach helper (incoming): {reached:?}"
    );
    assert_graph_edges_well_formed(&body);

    // path(run -> engine): the confidence-weighted route run -> helper -> engine. ~keep
    let path = service
        .call_tool(call_params(
            "graph",
            json!({"mode": "path", "from": "run", "to": "engine", "edges": "calls"}),
        ))
        .await
        .expect("path");
    let body = decode_text(&path);
    assert_eq!(
        body.get("found").and_then(Value::as_bool),
        Some(true),
        "route found: {body}"
    );
    let names: Vec<&str> = body
        .get("nodes")
        .and_then(Value::as_array)
        .expect("path nodes")
        .iter()
        .filter_map(|n| n.get("name").and_then(Value::as_str))
        .collect();
    assert_eq!(names, vec!["run", "helper", "engine"], "ordered path: {body}");
    let edge_count = body
        .get("edges")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(edge_count, names.len() - 1, "one edge between each pair of path nodes");
    // path reports scan truncation like its siblings — a small fixture never hits the cap.
    assert_eq!(
        body.get("truncated").and_then(Value::as_bool),
        Some(false),
        "path exposes truncated=false on a complete scan: {body}"
    );
    assert_graph_edges_well_formed(&body);

    // subgraph(helper): a readable neighborhood; nodes carry a centrality score.
    let subgraph = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "subgraph", "name": "helper", "depth": 2, "edges": "all", "max_nodes": 10}),
        ))
        .await
        .expect("subgraph");
    let body = decode_text(&subgraph);
    let nodes = body.get("nodes").and_then(Value::as_array).expect("subgraph nodes");
    assert!(!nodes.is_empty(), "subgraph has nodes: {body}");
    assert!(
        nodes.iter().all(|n| n.get("centrality").is_some()),
        "every subgraph node carries a centrality score: {body}"
    );
    assert_graph_edges_well_formed(&body);

    // edges="contains" honors the containment lane — it must not silently degrade to calls-only.
    let contains = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "neighbors", "name": "engine", "direction": "both", "edges": "contains"}),
        ))
        .await
        .expect("neighbors contains");
    let body = decode_text(&contains);
    let has_contains = body
        .get("edges")
        .and_then(Value::as_array)
        .map(|es| {
            es.iter()
                .any(|e| e.get("kind").and_then(Value::as_str) == Some("contains"))
        })
        .unwrap_or(false);
    assert!(has_contains, "edges=contains must yield a containment edge: {body}");

    // A bogus edges value is rejected, not silently coerced to a different lane set.
    let bogus = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "neighbors", "name": "engine", "edges": "nonsense"}),
        ))
        .await;
    assert!(
        bogus.is_err(),
        "an invalid edges value must be an error, not a silent default"
    );

    // Unresolved-name contract (stated in each tool's description): an unknown symbol yields an
    // empty result, never an error — so an agent can probe freely without exception handling.
    let missing = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "neighbors", "name": "does_not_exist_zzz"}),
        ))
        .await
        .expect("neighbors on an unknown name returns Ok, not an error");
    assert!(
        decode_text(&missing)
            .get("nodes")
            .and_then(Value::as_array)
            .is_some_and(|a| a.is_empty()),
        "neighbors of an unresolved name is empty"
    );
    let missing = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "subgraph", "name": "does_not_exist_zzz"}),
        ))
        .await
        .expect("subgraph on an unknown name returns Ok, not an error");
    assert!(
        decode_text(&missing)
            .get("nodes")
            .and_then(Value::as_array)
            .is_some_and(|a| a.is_empty()),
        "subgraph of an unresolved name is empty"
    );
    let missing = service
        .call_tool(call_params(
            "graph",
            json!({"mode": "path", "from": "does_not_exist_zzz", "to": "engine"}),
        ))
        .await
        .expect("path from an unknown source returns Ok, not an error");
    assert_eq!(
        decode_text(&missing).get("found").and_then(Value::as_bool),
        Some(false),
        "path from an unresolved source is not found, not an error"
    );

    let _ = service.cancel().await;
}

/// ADR-0004: the `communities` tool clusters the shared code-graph into de-facto modules with
/// deterministic, LLM-free labels.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn communities_cluster_the_shared_graph() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    // Two clusters: a math module (add/mul/compute) and a text module (upper/lower/render), each
    // internally wired, joined by a single cross-cluster call.
    std::fs::write(
        root.join("math.rs"),
        "pub fn add() {}\npub fn mul() { add(); }\npub fn compute() { add(); mul(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("text.rs"),
        "pub fn upper() {}\npub fn lower() { upper(); }\npub fn render() { upper(); lower(); }\n",
    )
    .unwrap();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let communities = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "communities", "algorithm": "label_propagation", "edges": "all"}),
        ))
        .await
        .expect("communities");
    assert_structured_matches_text(&communities);
    let body = decode_text(&communities);
    assert_eq!(
        body.get("algorithm").and_then(Value::as_str),
        Some("label_propagation"),
        "algorithm is echoed: {body}"
    );
    let list = body.get("communities").and_then(Value::as_array).expect("communities");
    assert!(!list.is_empty(), "at least one community detected: {body}");
    for c in list {
        assert!(
            c.get("label").and_then(Value::as_str).is_some_and(|s| !s.is_empty()),
            "every community has a non-empty label: {c}"
        );
        let members = c.get("members").and_then(Value::as_array).expect("members");
        assert!(!members.is_empty(), "community has members: {c}");
        assert!(
            members.iter().all(|m| m.get("centrality").is_some()),
            "every member carries a centrality score: {c}"
        );
    }

    // Louvain is the opt-in higher-quality algorithm; it must also run and echo its name.
    let louvain = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "communities", "algorithm": "louvain"}),
        ))
        .await
        .expect("communities louvain");
    let body = decode_text(&louvain);
    assert_eq!(
        body.get("algorithm").and_then(Value::as_str),
        Some("louvain"),
        "louvain algorithm echoed: {body}"
    );

    // max_communities=1 caps the returned list below the detected count and flags truncated.
    let capped = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "communities", "algorithm": "label_propagation", "max_communities": 1}),
        ))
        .await
        .expect("communities capped");
    let body = decode_text(&capped);
    let returned = body
        .get("communities")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let detected = body.get("num_communities").and_then(Value::as_u64).unwrap_or(0);
    assert_eq!(returned, 1, "capped to one community: {body}");
    assert!(detected >= 2, "the two modules are distinct, so ≥2 detected: {body}");
    assert_eq!(
        body.get("truncated").and_then(Value::as_bool),
        Some(true),
        "capping the community list flags truncated: {body}"
    );

    // An invalid algorithm is rejected, not silently defaulted.
    let bogus = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "communities", "algorithm": "kmeans"}),
        ))
        .await;
    assert!(bogus.is_err(), "an invalid algorithm must be an error");

    let _ = service.cancel().await;
}

/// ADR-0005: the `graph_export` tool renders the shared code-graph into text formats over one
/// canonical payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_export_renders_every_format() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    std::fs::write(
        root.join("core.rs"),
        "pub fn engine() {}\npub fn helper() { engine(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app.rs"),
        "use crate::core::helper;\npub fn run() { helper(); }\n",
    )
    .unwrap();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    // node_link: the response body carries valid node-link JSON as a string.
    let export = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "export", "format": "node_link", "edges": "all"}),
        ))
        .await
        .expect("graph_export node_link");
    assert_structured_matches_text(&export);
    let body = decode_text(&export);
    assert_eq!(body.get("format").and_then(Value::as_str), Some("node_link"), "{body}");
    assert!(
        body.get("node_count").and_then(Value::as_u64).unwrap_or(0) >= 3,
        "nodes: {body}"
    );
    let content = body.get("content").and_then(Value::as_str).expect("content");
    let doc: Value = serde_json::from_str(content).expect("content is valid node-link JSON");
    assert!(
        doc.get("nodes")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
    );
    assert!(doc.get("links").and_then(Value::as_array).is_some());

    // Each remaining format renders and echoes its name with a recognizable header/keyword.
    for (fmt, needle) in [
        ("dot", "digraph basemind"),
        ("mermaid", "graph LR"),
        ("graphml", "<graphml"),
        ("cypher", "CREATE ("),
        ("html", "<!doctype html>"),
        ("svg", "<svg xmlns="),
    ] {
        let out = service
            .call_tool(call_params("graph", json!({ "mode": "export", "format": fmt})))
            .await
            .unwrap_or_else(|_| panic!("graph_export {fmt}"));
        let body = decode_text(&out);
        assert_eq!(body.get("format").and_then(Value::as_str), Some(fmt), "{body}");
        let content = body.get("content").and_then(Value::as_str).unwrap_or("");
        assert!(content.contains(needle), "format {fmt} missing {needle:?}: {content}");
    }

    // max_nodes=2 caps the view below the node count, flags truncated, and every edge endpoint
    // stays within the remapped node range.
    let capped = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "export", "format": "node_link", "edges": "all", "max_nodes": 2}),
        ))
        .await
        .expect("graph_export capped");
    let body = decode_text(&capped);
    let node_count = body.get("node_count").and_then(Value::as_u64).unwrap_or(0);
    assert!(node_count <= 2, "capped to max_nodes: {body}");
    assert_eq!(
        body.get("truncated").and_then(Value::as_bool),
        Some(true),
        "capping flags truncated: {body}"
    );
    let doc: Value = serde_json::from_str(body.get("content").and_then(Value::as_str).unwrap()).unwrap();
    for link in doc.get("links").and_then(Value::as_array).into_iter().flatten() {
        for end in ["source", "target"] {
            let idx = link.get(end).and_then(Value::as_u64).unwrap();
            assert!(idx < node_count, "edge {end} {idx} in range after cap: {link}");
        }
    }

    // write: true persists the rendered content to the cache and returns its path; the inline
    // content is still returned, and the on-disk file matches it byte-for-byte.
    let written = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "export", "format": "svg", "write": true}),
        ))
        .await
        .expect("graph_export write");
    let body = decode_text(&written);
    let output_path = body
        .get("output_path")
        .and_then(Value::as_str)
        .expect("output_path present when write=true");
    assert!(
        output_path.ends_with(".svg"),
        "export named by format extension: {output_path}"
    );
    let on_disk = std::fs::read_to_string(output_path).expect("export file exists on disk");
    let inline = body.get("content").and_then(Value::as_str).expect("content");
    assert_eq!(on_disk, inline, "written file matches inline content");
    assert!(on_disk.contains("<svg xmlns="), "written file is the SVG document");

    // Without write, no file path is returned.
    let unwritten = service
        .call_tool(call_params("graph", json!({ "mode": "export", "format": "svg"})))
        .await
        .expect("graph_export no-write");
    assert!(
        decode_text(&unwritten).get("output_path").is_none(),
        "output_path omitted when write is not set"
    );

    // An invalid format is rejected, not silently defaulted.
    let bogus = service
        .call_tool(call_params("graph", json!({ "mode": "export", "format": "nonesuch"})))
        .await;
    assert!(bogus.is_err(), "an unsupported format must be an error");

    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_export_bounds_unfocused_dense_views_by_default() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    let mut source = String::new();
    for caller in 0..32 {
        source.push_str(&format!("pub fn function_{caller}() {{"));
        for callee in 0..32 {
            if caller != callee {
                source.push_str(&format!(" function_{callee}();"));
            }
        }
        source.push_str(" }\n");
    }
    std::fs::write(root.join("dense.rs"), source).expect("write dense graph fixture");
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let bounded = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "export", "format": "mermaid", "write": false }),
        ))
        .await
        .expect("bounded graph export");
    let body = decode_text(&bounded);
    let edge_count = body.get("edge_count").and_then(Value::as_u64).unwrap_or(u64::MAX);
    let edge_count_total = body.get("edge_count_total").and_then(Value::as_u64).unwrap_or(0);
    assert!(
        edge_count <= 200,
        "unfocused export must apply the safe default edge cap: {body}"
    );
    assert!(
        edge_count_total > edge_count,
        "export must report the pre-cap edge total: {body}"
    );
    assert_eq!(
        body.get("truncated").and_then(Value::as_bool),
        Some(true),
        "the response must disclose the default cap: {body}"
    );

    let explicitly_bounded = service
        .call_tool(call_params(
            "graph",
            json!({
                "mode": "export",
                "format": "mermaid",
                "write": false,
                "max_edges": 7
            }),
        ))
        .await
        .expect("graph export accepts an explicit max_edges cap");
    let body = decode_text(&explicitly_bounded);
    let edge_count = body.get("edge_count").and_then(Value::as_u64).unwrap_or(u64::MAX);
    let edge_count_total = body.get("edge_count_total").and_then(Value::as_u64).unwrap_or(0);
    assert!(
        edge_count <= 7,
        "explicit max_edges must tighten the export cap: {body}"
    );
    assert!(
        edge_count_total > edge_count,
        "explicit cap must preserve the pre-cap total: {body}"
    );
    assert_eq!(
        body.get("truncated").and_then(Value::as_bool),
        Some(true),
        "the response must disclose the explicit cap: {body}"
    );

    for mode in ["display", "open"] {
        let bounded = service
            .call_tool(call_params(
                "graph",
                json!({
                    "mode": mode,
                    "format": "svg",
                    "open": false,
                    "max_edges": 7
                }),
            ))
            .await
            .unwrap_or_else(|error| panic!("graph {mode} accepts max_edges: {error}"));
        let body = decode_text(&bounded);
        let edge_count = body.get("edge_count").and_then(Value::as_u64).unwrap_or(u64::MAX);
        let edge_count_total = body.get("edge_count_total").and_then(Value::as_u64).unwrap_or(0);
        assert!(edge_count <= 7, "graph {mode} must apply max_edges: {body}");
        assert!(
            edge_count_total > edge_count,
            "graph {mode} must report the pre-cap total: {body}"
        );
        assert_eq!(
            body.get("truncated").and_then(Value::as_bool),
            Some(true),
            "graph {mode} must disclose the cap: {body}"
        );
    }

    let _ = service.cancel().await;
}

/// ADR-0007: the `display` tool renders a *visual* view, always writes it to the export cache, and
/// (with `open: false`, the headless/test path) degrades to export-only without spawning a viewer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn display_writes_visual_view_and_degrades_without_opening() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    std::fs::write(
        root.join("core.rs"),
        "pub fn engine() {}\npub fn helper() { engine(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app.rs"),
        "use crate::core::helper;\npub fn run() { helper(); }\n",
    )
    .unwrap();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    // Default format is html; open:false takes the export-only path (never launches a viewer in CI).
    let shown = service
        .call_tool(call_params("graph", json!({ "mode": "display", "open": false})))
        .await
        .expect("display html");
    assert_structured_matches_text(&shown);
    let body = decode_text(&shown);
    assert_eq!(
        body.get("format").and_then(Value::as_str),
        Some("html"),
        "default format html: {body}"
    );
    assert_eq!(
        body.get("displayed").and_then(Value::as_bool),
        Some(false),
        "open:false does not launch a viewer: {body}"
    );
    assert_eq!(
        body.get("method").and_then(Value::as_str),
        Some("export"),
        "degrades to export-only: {body}"
    );
    assert!(
        body.get("node_count").and_then(Value::as_u64).unwrap_or(0) >= 3,
        "nodes: {body}"
    );
    // The product is the file, not inline bytes: output_path is always present and content is not.
    assert!(
        body.get("content").is_none(),
        "display does not return rendered bytes inline: {body}"
    );
    let output_path = body
        .get("output_path")
        .and_then(Value::as_str)
        .expect("output_path always present");
    assert!(
        output_path.ends_with(".html"),
        "html export named by extension: {output_path}"
    );
    let on_disk = std::fs::read_to_string(output_path).expect("export file exists on disk");
    assert!(
        on_disk.contains("<!doctype html>"),
        "written file is the interactive HTML page"
    );

    // svg is the other accepted visual format.
    let svg = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "display", "format": "svg", "open": false}),
        ))
        .await
        .expect("display svg");
    let body = decode_text(&svg);
    assert_eq!(body.get("format").and_then(Value::as_str), Some("svg"), "{body}");
    assert!(
        body.get("output_path")
            .and_then(Value::as_str)
            .is_some_and(|p| p.ends_with(".svg")),
        "svg export path: {body}"
    );

    // A graph *data* format is rejected — display shows a picture; graph_export returns the data.
    let rejected = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "display", "format": "node_link", "open": false}),
        ))
        .await;
    assert!(
        rejected.is_err(),
        "a non-visual format must be rejected, not silently rendered"
    );

    let _ = service.cancel().await;
}

/// ADR-0006: the `ui` tool renders the interactive graph, always writes it to the export cache, and
/// returns a URL. With no daemon serving (the isolated test comms dir has none) it degrades to the
/// `file://` export; `open: false` returns the URL without launching a viewer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_returns_url_and_writes_export_without_opening() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    std::fs::write(
        root.join("core.rs"),
        "pub fn engine() {}\npub fn helper() { engine(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app.rs"),
        "use crate::core::helper;\npub fn run() { helper(); }\n",
    )
    .unwrap();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    // Default format html; open:false returns the URL without launching (never spawns a viewer in CI).
    let shown = service
        .call_tool(call_params("graph", json!({ "mode": "open", "open": false})))
        .await
        .expect("ui html");
    assert_structured_matches_text(&shown);
    let body = decode_text(&shown);
    assert_eq!(
        body.get("format").and_then(Value::as_str),
        None,
        "ui response carries a url/method, not a format echo: {body}"
    );
    // The isolated test comms dir has no daemon portfile, so the tool degrades to the file export.
    assert_eq!(
        body.get("served").and_then(Value::as_bool),
        Some(false),
        "no daemon serving in the isolated test env: {body}"
    );
    assert_eq!(
        body.get("method").and_then(Value::as_str),
        Some("file"),
        "degrades to the file export: {body}"
    );
    let url = body.get("url").and_then(Value::as_str).expect("url present");
    assert!(url.starts_with("file://"), "file fallback URL: {url}");
    assert!(
        body.get("node_count").and_then(Value::as_u64).unwrap_or(0) >= 3,
        "nodes: {body}"
    );
    // The product is the URL + the written file, not inline bytes.
    assert!(
        body.get("content").is_none(),
        "ui does not return rendered bytes inline: {body}"
    );
    let output_path = body
        .get("output_path")
        .and_then(Value::as_str)
        .expect("output_path always present");
    assert!(
        output_path.ends_with(".html"),
        "html export named by extension: {output_path}"
    );
    let output_name = Path::new(output_path)
        .file_name()
        .and_then(|name| name.to_str())
        .expect("export filename");
    assert!(
        url.ends_with(output_name),
        "the file:// URL points at the written export: url={url} path={output_path}"
    );
    let on_disk = std::fs::read_to_string(output_path).expect("export file exists on disk");
    assert!(
        on_disk.contains("<!doctype html>"),
        "written file is the interactive HTML page"
    );

    // Knob plumbing: `max_nodes` caps the rendered graph and the response reports the cut. The repo has
    // threads the graph-shaping knobs through `render_ui_parts`, not just the defaults.
    let capped = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "open", "max_nodes": 1, "open": false}),
        ))
        .await
        .expect("ui max_nodes=1");
    let capped = decode_text(&capped);
    assert_eq!(
        capped.get("node_count").and_then(Value::as_u64),
        Some(1),
        "max_nodes=1 caps the rendered graph to one node: {capped}"
    );
    assert_eq!(
        capped.get("truncated").and_then(Value::as_bool),
        Some(true),
        "capping a larger graph reports truncated: {capped}"
    );
    // `focus` is threaded end-to-end (param -> render): the tool accepts it and renders without error.
    let focused = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "open", "focus": "core.rs", "open": false}),
        ))
        .await
        .expect("ui focus renders");
    let focused = decode_text(&focused);
    assert!(
        focused.get("node_count").and_then(Value::as_u64).is_some(),
        "focus renders a graph carrying a node count: {focused}"
    );

    // A graph *data* format is rejected — the UI shows a picture; graph_export returns the data.
    let rejected = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "open", "format": "node_link", "open": false}),
        ))
        .await;
    assert!(
        rejected.is_err(),
        "a non-visual format must be rejected, not silently rendered"
    );

    let _ = service.cancel().await;
}

/// ADR-0009: a `// WHY:` comment citing an ADR surfaces a rationale node with an `annotates` edge to
/// the code it precedes and a `cites` edge to the decision record. Extraction populates
/// `l1.rationale` at scan time; `graph_export edges="rationale"` renders both edge kinds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rationale_lane_surfaces_annotates_and_cites_edges() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("docs/adr")).unwrap();
    std::fs::write(root.join("docs/adr/0001-graph.md"), b"# ADR-0001: graph\n").unwrap();
    std::fs::write(
        root.join("core.rs"),
        b"// WHY: keep the lock scope tight; see ADR-0001\npub fn engine() {}\n",
    )
    .unwrap();
    basemind::lang::ensure_grammars().expect("grammar bootstrap");
    let cfg = basemind::config::default_for_root(root);
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

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let export = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "export", "edges": "rationale", "format": "node_link" }),
        ))
        .await
        .expect("graph_export rationale");
    assert_structured_matches_text(&export);
    let body = decode_text(&export);
    let content = body.get("content").and_then(Value::as_str).unwrap_or_default();
    assert!(
        content.contains("annotates"),
        "the rationale lane must annotate the engine fn: {content}"
    );
    assert!(
        content.contains("cites"),
        "the ADR-0001 citation must produce a cites edge to the decision record: {content}"
    );

    let _ = service.cancel().await;
}

/// ADR-0008: a scanned document that cites an indexed source file surfaces a `Documents` edge in the
/// shared code-graph. The document tier writes the doc→code link at scan time; the serve cache-warm
/// path reloads it, and `graph_export edges="documents"` renders the resulting edge.
#[cfg(feature = "documents")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn documents_lane_surfaces_doc_to_code_edges() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("core.rs"), b"pub fn engine() {}\n").unwrap();
    // SVG, not .md/.txt/.csv: those have tree-sitter grammars and route to the code tier, while SVG
    // is xberg-extractable yet grammar-less, so it lands in the document tier (same choice bug_44 /
    // scan_smoke make). Its text cites the indexed `core.rs`, producing a path-citation doc link.
    std::fs::write(
        root.join("notes.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\">\n\
          <text>The engine lives in core.rs and drives the whole pipeline.</text>\n\
          </svg>\n",
    )
    .unwrap();

    // Embedding-free doc scan: doc links are keyword / path-citation heuristics over chunk text and
    // do not depend on an ONNX embedding model being present in the test environment.
    let mut cfg = basemind::config::default_for_root(root);
    cfg.documents.embed = false;
    basemind::lang::ensure_grammars().expect("grammar bootstrap");
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

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let export = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "export", "edges": "documents", "format": "node_link" }),
        ))
        .await
        .expect("graph_export documents");
    assert_structured_matches_text(&export);
    let body = decode_text(&export);
    let edge_count = body.get("edge_count").and_then(Value::as_u64).unwrap_or(0);
    assert!(
        edge_count >= 1,
        "the documents lane must surface a doc→code edge for the core.rs citation: {body}"
    );
    let content = body.get("content").and_then(Value::as_str).unwrap_or_default();
    assert!(
        content.contains("documents"),
        "the rendered graph must carry a documents edge: {content}"
    );

    let _ = service.cancel().await;
}

/// ADR-0008 regression: the doc↔code lane must SURVIVE an unscoped rescan. Before the fix only the
/// boot cache-warm attached `doc_links`; an unscoped `scan_and_refresh` rebuilt the `MapCache` bare, so
/// the `documents` lane silently went empty after the first rescan. `doc_links_cache::attach_async`
/// now reattaches the persisted links on that path. This test fails on the pre-fix tree.
#[cfg(feature = "documents")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn documents_lane_survives_unscoped_rescan() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("core.rs"), b"pub fn engine() {}\n").unwrap();
    std::fs::write(
        root.join("notes.svg"),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\">\n\
          <text>The engine lives in core.rs and drives the whole pipeline.</text>\n\
          </svg>\n",
    )
    .unwrap();
    let mut cfg = basemind::config::default_for_root(root);
    cfg.documents.embed = false;
    basemind::lang::ensure_grammars().expect("grammar bootstrap");
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

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let doc_edges = json!({ "mode": "export", "edges": "documents", "format": "node_link" });

    // Baseline: the boot cache-warm attached the persisted link.
    let before = service
        .call_tool(call_params("graph", doc_edges.clone()))
        .await
        .expect("graph_export documents (before)");
    let edges_before = decode_text(&before)
        .get("edge_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    assert!(edges_before >= 1, "baseline documents edge missing after warm");

    // An unscoped rescan rebuilds the whole MapCache — the path that used to wipe doc_links.
    let _ = service
        .call_tool(call_params("admin", json!({ "mode": "rescan"})))
        .await
        .expect("rescan");

    let after = service
        .call_tool(call_params("graph", doc_edges))
        .await
        .expect("graph_export documents (after)");
    let edges_after = decode_text(&after)
        .get("edge_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    assert!(edges_after >= 1, "the documents lane was wiped by an unscoped rescan");

    let _ = service.cancel().await;
}

/// Per-mode contract for the consolidated `workspace` tool.
///
/// The five modes' bodies all talk to the comms broker daemon, which a hermetic smoke run has no
/// business spawning — so what is asserted here is the layer that runs BEFORE the daemon round-trip
/// and is exactly the layer consolidation introduced: `mode` is required, every mode is advertised,
/// a field belonging to another mode is rejected rather than ignored, and a mode that needs
/// `repo_id` / `name` names the missing pair instead of failing anonymously downstream.
///
/// A refusal reaches the caller two ways — `Lenient` renders a parameter-shape failure as an
/// `is_error` result, while a helper-level rejection is a `-32602` — so [`refusal`] normalizes both
/// to the message text and every assertion is made on the wording an agent actually reads.
#[cfg(all(feature = "comms", any(unix, windows)))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_tool_validates_every_mode_before_touching_the_registry() {
    /// The refusal text of a call that must not have succeeded, whichever way it was refused.
    fn refusal(result: Result<CallToolResult, rmcp::service::ServiceError>, context: &str) -> String {
        match result {
            Err(error) => error.to_string(),
            Ok(result) => {
                assert_eq!(result.is_error, Some(true), "{context} must be refused: {result:?}");
                result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        }
    }

    let dir = build_repo();
    let root = dir.path();

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let tools = service.list_all_tools().await.expect("list tools");
    let workspace = tools
        .iter()
        .find(|t| t.name == "workspace")
        .expect("the `workspace` domain tool must be advertised under --features comms");
    let schema = serde_json::to_string(&workspace.input_schema).expect("serialize inputSchema");
    for mode in ["workspaces", "worktrees", "branches", "claim", "release"] {
        assert!(
            schema.contains(&format!("\"{mode}\"")),
            "`workspace` inputSchema must advertise mode `{mode}`: {schema}"
        );
    }

    // `mode` is required: an omitted mode errors, it never silently picks an operation.
    let message = refusal(
        service.call_tool(call_params("workspace", json!({}))).await,
        "an omitted mode",
    );
    assert!(
        message.contains("missing field `mode`"),
        "an omitted mode must name the field: {message}"
    );

    let message = refusal(
        service
            .call_tool(call_params("workspace", json!({ "mode": "worktree" })))
            .await,
        "an unknown mode",
    );
    assert!(
        message.contains("worktrees") && message.contains("claim"),
        "an unknown mode must list the accepted set: {message}"
    );

    // mode `workspaces` takes neither `repo_id` nor `name` — a stray field is rejected, not ignored.
    let message = refusal(
        service
            .call_tool(call_params(
                "workspace",
                json!({ "mode": "workspaces", "repo_id": "path:/nowhere" }),
            ))
            .await,
        "mode `workspaces` with a stray `repo_id`",
    );
    assert!(
        message.contains("mode `workspaces` does not accept") && message.contains("`repo_id`"),
        "mode `workspaces` must name the field it rejected: {message}"
    );

    // modes `worktrees` / `branches` require `repo_id` and reject `name`.
    for mode in ["worktrees", "branches"] {
        let message = refusal(
            service
                .call_tool(call_params("workspace", json!({ "mode": mode })))
                .await,
            "a repo_id-less list mode",
        );
        assert!(
            message.contains(&format!("mode=\"{mode}\" requires `repo_id`")),
            "mode `{mode}` must name the missing `repo_id`: {message}"
        );

        let message = refusal(
            service
                .call_tool(call_params(
                    "workspace",
                    json!({ "mode": mode, "repo_id": "path:/nowhere", "name": "(main)" }),
                ))
                .await,
            "a list mode given `name`",
        );
        assert!(
            message.contains("`name`"),
            "mode `{mode}` must reject `name` rather than ignore it: {message}"
        );
    }

    // modes `claim` / `release` require both `repo_id` and `name`.
    for mode in ["claim", "release"] {
        let message = refusal(
            service
                .call_tool(call_params("workspace", json!({ "mode": mode, "name": "(main)" })))
                .await,
            "a repo_id-less claim/release",
        );
        assert!(
            message.contains(&format!("mode=\"{mode}\" requires `repo_id`")),
            "mode `{mode}` must name the missing `repo_id`: {message}"
        );

        let message = refusal(
            service
                .call_tool(call_params(
                    "workspace",
                    json!({ "mode": mode, "repo_id": "path:/nowhere" }),
                ))
                .await,
            "a name-less claim/release",
        );
        assert!(
            message.contains(&format!("mode=\"{mode}\" requires `name`")),
            "mode `{mode}` must name the missing `name`: {message}"
        );
    }

    let _ = service.cancel().await;
}

/// Per-mode contract for the consolidated `admin` tool: `mode` is required, every mode is
/// advertised in the input schema, a field belonging to another mode is rejected rather than
/// ignored, and a mode that needs a field it did not receive names the exact `mode`/field pair.
///
/// A refusal reaches the caller two ways — `Lenient` renders a parameter-shape failure as an
/// `is_error` result, while a helper-level rejection is a `-32602` — so [`refusal`] normalizes both
/// to the message text and every assertion is made on the wording an agent actually reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_tool_validates_every_mode_before_running_it() {
    /// The refusal text of a call that must not have succeeded, whichever way it was refused.
    fn refusal(result: Result<CallToolResult, rmcp::service::ServiceError>, context: &str) -> String {
        match result {
            Err(error) => error.to_string(),
            Ok(result) => {
                assert_eq!(result.is_error, Some(true), "{context} must be refused: {result:?}");
                result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        }
    }

    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let tools = service.list_all_tools().await.expect("list tools");
    let admin = tools
        .iter()
        .find(|t| t.name.as_ref() == "admin")
        .expect("the `admin` domain tool must always be advertised");
    let schema = serde_json::to_string(&admin.input_schema).expect("serialize inputSchema");
    for mode in [
        "status",
        "repo",
        "rescan",
        "cache_stats",
        "gc",
        "cache_clear",
        "telemetry",
        "compress",
        "delta",
        "checkpoint",
        "waste",
    ] {
        assert!(
            schema.contains(&format!("\"{mode}\"")),
            "`admin` inputSchema must advertise mode `{mode}`: {schema}"
        );
    }

    // `mode` is required: an omitted mode errors, it never silently picks an operation.
    let message = refusal(
        service.call_tool(call_params("admin", json!({}))).await,
        "an omitted mode",
    );
    assert!(
        message.contains("missing field `mode`"),
        "an omitted mode must name the field: {message}"
    );

    // An unknown mode names every accepted spelling.
    let message = refusal(
        service
            .call_tool(call_params("admin", json!({ "mode": "reindex" })))
            .await,
        "an unknown mode",
    );
    assert!(
        message.contains("rescan") && message.contains("cache_clear"),
        "an unknown mode must list the accepted set: {message}"
    );

    // mode `status` takes no sibling fields — a stray field is rejected, not ignored.
    let message = refusal(
        service
            .call_tool(call_params("admin", json!({ "mode": "status", "paths": ["a.rs"] })))
            .await,
        "mode `status` with a stray `paths`",
    );
    assert!(
        message.contains("`admin` mode `status` does not accept") && message.contains("`paths`"),
        "mode `status` must name the field it rejected: {message}"
    );

    // A field belonging to a DIFFERENT mode (not just an arbitrary unknown one) is also rejected,
    // never silently dropped: `component` belongs to `cache_clear`, not `rescan`.
    let message = refusal(
        service
            .call_tool(call_params("admin", json!({ "mode": "rescan", "component": "blobs" })))
            .await,
        "mode `rescan` given a `cache_clear` field",
    );
    assert!(
        message.contains("`admin` mode `rescan` does not accept") && message.contains("`component`"),
        "mode `rescan` must reject a sibling mode's field rather than ignore it: {message}"
    );

    // `delta` requires both `old` and `new` — the missing one is named exactly.
    let message = refusal(
        service
            .call_tool(call_params("admin", json!({ "mode": "delta", "old": "a" })))
            .await,
        "a `new`-less delta",
    );
    assert!(
        message.contains("`admin` mode=\"delta\" requires `new`"),
        "mode `delta` must name the missing `new`: {message}"
    );

    // `checkpoint` requires `text`.
    let message = refusal(
        service
            .call_tool(call_params("admin", json!({ "mode": "checkpoint" })))
            .await,
        "a text-less checkpoint",
    );
    assert!(
        message.contains("`admin` mode=\"checkpoint\" requires `text`"),
        "mode `checkpoint` must name the missing `text`: {message}"
    );

    let _ = service.cancel().await;
}

/// Per-mode contract for the consolidated `memory` tool, mirroring the `admin` and `workspace`
/// coverage above: `mode` is required, every mode is advertised, a field belonging to another mode
/// is rejected rather than ignored, and a mode missing a required field names the exact pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_tool_validates_every_mode_before_running_it() {
    /// The refusal text of a call that must not have succeeded, whichever way it was refused.
    fn refusal(result: Result<CallToolResult, rmcp::service::ServiceError>, context: &str) -> String {
        match result {
            Err(error) => error.to_string(),
            Ok(result) => {
                assert_eq!(result.is_error, Some(true), "{context} must be refused: {result:?}");
                result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        }
    }

    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let tools = service.list_all_tools().await.expect("list tools");
    let memory = tools
        .iter()
        .find(|t| t.name.as_ref() == "memory")
        .expect("the `memory` domain tool must always be advertised");
    let schema = serde_json::to_string(&memory.input_schema).expect("serialize inputSchema");
    for mode in [
        "put",
        "get",
        "list",
        "search",
        "delete",
        "audit",
        "documents",
        "mine",
        "proposals",
        "accept",
        "reject",
    ] {
        assert!(
            schema.contains(&format!("\"{mode}\"")),
            "`memory` inputSchema must advertise mode `{mode}`: {schema}"
        );
    }

    // `mode` is required: an omitted mode errors, it never silently picks an operation.
    let message = refusal(
        service.call_tool(call_params("memory", json!({}))).await,
        "an omitted mode",
    );
    assert!(
        message.contains("missing field `mode`"),
        "an omitted mode must name the field: {message}"
    );

    // An unknown mode names every accepted spelling.
    let message = refusal(
        service
            .call_tool(call_params("memory", json!({ "mode": "recall" })))
            .await,
        "an unknown mode",
    );
    assert!(
        message.contains("put") && message.contains("get") && message.contains("delete"),
        "an unknown mode must list the accepted set: {message}"
    );

    // A field belonging to a DIFFERENT mode is rejected, not silently ignored: `window` belongs to
    // `mine`, not `get`.
    let message = refusal(
        service
            .call_tool(call_params(
                "memory",
                json!({ "mode": "get", "key": "k", "window": 10 }),
            ))
            .await,
        "mode `get` given a `mine` field",
    );
    assert!(
        message.contains("`memory` mode `get` does not accept") && message.contains("`window`"),
        "mode `get` must reject a sibling mode's field rather than ignore it: {message}"
    );

    // `put` requires both `key` and `value` — the missing one is named exactly. `require()` only ~keep
    // runs inside `run_memory_ops`, which is itself gated on `--features memory`: a build without it ~keep
    // answers with the feature-gate message for every mode before ever reaching `require()`, so the ~keep
    // exact-field wording is only checked on a `--features memory` build. ~keep
    let message = refusal(
        service
            .call_tool(call_params("memory", json!({ "mode": "put", "key": "k" })))
            .await,
        "a value-less put",
    );
    if cfg!(feature = "memory") {
        assert!(
            message.contains("`memory`: mode=\"put\" requires `value`"),
            "mode `put` must name the missing `value`: {message}"
        );
    } else {
        assert!(
            message.contains("requires the `memory` feature"),
            "without --features memory, mode `put` must name the missing feature: {message}"
        );
    }

    // `search` requires `query`, same feature-gate caveat as `put` above.
    let message = refusal(
        service
            .call_tool(call_params("memory", json!({ "mode": "search" })))
            .await,
        "a query-less search",
    );
    if cfg!(feature = "memory") {
        assert!(
            message.contains("`memory`: mode=\"search\" requires `query`"),
            "mode `search` must name the missing `query`: {message}"
        );
    } else {
        assert!(
            message.contains("requires the `memory` feature"),
            "without --features memory, mode `search` must name the missing feature: {message}"
        );
    }

    let _ = service.cancel().await;
}

/// Per-mode contract for the consolidated `git` tool, mirroring the `admin` / `workspace` /
/// `memory` coverage above: every mode is advertised, `mode` is required, a field belonging to
/// another mode is rejected rather than ignored, and a mode missing a required field names the
/// exact `mode`/field pair.
///
/// Consolidation moved eleven operations from tool names into `mode`. Coverage has to be asserted
/// on the modes or it silently stops covering them — `tools/list` would still show one healthy
/// `git` tool while any number of its modes were unreachable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_tool_validates_every_mode_before_running_it() {
    /// The refusal text of a call that must not have succeeded, whichever way it was refused.
    fn refusal(result: Result<CallToolResult, rmcp::service::ServiceError>, context: &str) -> String {
        match result {
            Err(error) => error.to_string(),
            Ok(result) => {
                assert_eq!(result.is_error, Some(true), "{context} must be refused: {result:?}");
                result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        }
    }

    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let tools = service.list_all_tools().await.expect("list tools");
    let git = tools
        .iter()
        .find(|t| t.name.as_ref() == "git")
        .expect("the `git` domain tool must always be advertised");
    let schema = serde_json::to_string(&git.input_schema).expect("serialize inputSchema");
    for mode in [
        "status",
        "recent",
        "touching",
        "by_path",
        "churn",
        "diff",
        "diff_outline",
        "blame",
        "blame_symbol",
        "symbol_history",
        "search",
    ] {
        assert!(
            schema.contains(&format!("\"{mode}\"")),
            "`git` inputSchema must advertise mode `{mode}`: {schema}"
        );
    }

    // `mode` is required: an omitted mode errors, it never silently picks an operation.
    let message = refusal(
        service.call_tool(call_params("git", json!({}))).await,
        "an omitted mode",
    );
    assert!(
        message.contains("missing field `mode`"),
        "an omitted mode must name the field: {message}"
    );

    // An unknown mode names the accepted spellings.
    let message = refusal(
        service.call_tool(call_params("git", json!({ "mode": "log" }))).await,
        "an unknown mode",
    );
    assert!(
        message.contains("recent") && message.contains("blame_symbol"),
        "an unknown mode must list the accepted set: {message}"
    );

    // mode `status` takes no sibling fields — a stray field is rejected, not ignored.
    let message = refusal(
        service
            .call_tool(call_params("git", json!({ "mode": "status", "path": "a.rs" })))
            .await,
        "mode `status` with a stray `path`",
    );
    assert!(
        message.contains("`git` mode `status` does not accept") && message.contains("`path`"),
        "mode `status` must name the field it rejected: {message}"
    );

    // A field belonging to a DIFFERENT mode is rejected, never silently dropped: `rev` belongs to
    // `blame` / `diff_outline`, not `recent`. Ignoring it would read as a log at that revision.
    let message = refusal(
        service
            .call_tool(call_params("git", json!({ "mode": "recent", "rev": "HEAD" })))
            .await,
        "mode `recent` given a `blame` field",
    );
    assert!(
        message.contains("`git` mode `recent` does not accept") && message.contains("`rev`"),
        "mode `recent` must reject a sibling mode's field rather than ignore it: {message}"
    );

    // `blame` cannot run without `path` — the missing field is named with its mode.
    let message = refusal(
        service.call_tool(call_params("git", json!({ "mode": "blame" }))).await,
        "a path-less blame",
    );
    assert!(
        message.contains("`git` mode=\"blame\" requires `path`"),
        "mode `blame` must name the missing `path`: {message}"
    );

    let _ = service.cancel().await;
}

/// Per-mode contract for the consolidated `graph` tool: every one of its nine modes is advertised,
/// `mode` is required, a field belonging to another mode is rejected rather than ignored, and a
/// mode missing a required field names the exact `mode`/field pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_tool_validates_every_mode_before_running_it() {
    /// The refusal text of a call that must not have succeeded, whichever way it was refused.
    fn refusal(result: Result<CallToolResult, rmcp::service::ServiceError>, context: &str) -> String {
        match result {
            Err(error) => error.to_string(),
            Ok(result) => {
                assert_eq!(result.is_error, Some(true), "{context} must be refused: {result:?}");
                result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        }
    }

    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let tools = service.list_all_tools().await.expect("list tools");
    let graph = tools
        .iter()
        .find(|t| t.name.as_ref() == "graph")
        .expect("the `graph` domain tool must always be advertised");
    let schema = serde_json::to_string(&graph.input_schema).expect("serialize inputSchema");
    for mode in [
        "calls",
        "neighbors",
        "path",
        "subgraph",
        "communities",
        "map",
        "export",
        "display",
        "open",
    ] {
        assert!(
            schema.contains(&format!("\"{mode}\"")),
            "`graph` inputSchema must advertise mode `{mode}`: {schema}"
        );
    }

    // `mode` is required: an omitted mode errors, it never silently picks an operation.
    let message = refusal(
        service.call_tool(call_params("graph", json!({}))).await,
        "an omitted mode",
    );
    assert!(
        message.contains("missing field `mode`"),
        "an omitted mode must name the field: {message}"
    );

    // An unknown mode names the accepted spellings.
    let message = refusal(
        service
            .call_tool(call_params("graph", json!({ "mode": "callgraph" })))
            .await,
        "an unknown mode",
    );
    assert!(
        message.contains("calls") && message.contains("communities"),
        "an unknown mode must list the accepted set: {message}"
    );

    // A field belonging to a DIFFERENT mode is rejected, never silently dropped: `name` belongs to
    // `calls` / `neighbors` / `subgraph`, not to `communities`. Ignoring it would read to an agent
    // as a clustering scoped to that symbol.
    let message = refusal(
        service
            .call_tool(call_params("graph", json!({ "mode": "communities", "name": "alpha" })))
            .await,
        "mode `communities` given a `subgraph` field",
    );
    assert!(
        message.contains("`graph` mode `communities` does not accept") && message.contains("`name`"),
        "mode `communities` must reject a sibling mode's field rather than ignore it: {message}"
    );

    // `calls` walks the call lane only — an `edges` lane selector is rejected, not quietly ignored.
    let message = refusal(
        service
            .call_tool(call_params(
                "graph",
                json!({ "mode": "calls", "name": "alpha", "edges": "all" }),
            ))
            .await,
        "mode `calls` with a stray `edges`",
    );
    assert!(
        message.contains("`graph` mode `calls` does not accept") && message.contains("`edges`"),
        "mode `calls` must name the field it rejected: {message}"
    );

    // `calls` requires `name` — the missing field is named with its mode.
    let message = refusal(
        service
            .call_tool(call_params("graph", json!({ "mode": "calls" })))
            .await,
        "a name-less calls walk",
    );
    assert!(
        message.contains("`graph` mode=\"calls\" requires `name`"),
        "mode `calls` must name the missing `name`: {message}"
    );

    // `path` requires both ends — the missing one is named exactly.
    let message = refusal(
        service
            .call_tool(call_params("graph", json!({ "mode": "path", "from": "alpha" })))
            .await,
        "a `to`-less path",
    );
    assert!(
        message.contains("`graph` mode=\"path\" requires `to`"),
        "mode `path` must name the missing `to`: {message}"
    );

    let _ = service.cancel().await;
}
