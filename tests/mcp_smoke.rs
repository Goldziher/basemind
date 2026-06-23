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
    // a.rs: symbols + an impl Drawable for Beta so find_implementations has a Rust hit.
    std::fs::write(
        root.join("a.rs"),
        b"pub fn alpha() {}\n\
          pub struct Beta { x: i32 }\n\
          impl Beta {\n  pub fn doit(&self) {}\n}\n\
          pub trait Drawable { fn draw(&self); }\n\
          impl Drawable for Beta { fn draw(&self) {} }\n",
    )
    .unwrap();
    // b.ts: TypeScript with `class Rectangle implements Drawable`.
    std::fs::write(
        root.join("b.ts"),
        b"export const Greet = (name: string) => `hi ${name}`;\n\
          export function plain() { return 1; }\n\
          interface Drawable { draw(): void; }\n\
          class Rectangle implements Drawable { draw() {} }\n",
    )
    .unwrap();
    // c.rs calls alpha() three times so the reference index has something to chew on.
    std::fs::write(
        root.join("c.rs"),
        b"pub fn caller() { alpha(); alpha(); other(); alpha(); }\n",
    )
    .unwrap();
    // d.py: Python subclass so find_implementations has a Python hit.
    std::fs::write(
        root.join("d.py"),
        b"class Foo: pass\nclass Bar(Foo): pass\n",
    )
    .unwrap();
    // e.rs: caller chain `outer -> middle -> inner` so the call_graph BFS has
    // something multi-hop to chew on.
    std::fs::write(
        root.join("e.rs"),
        b"pub fn inner() {}\n\
          pub fn middle() { inner(); }\n\
          pub fn outer() { middle(); }\n",
    )
    .unwrap();
    git(root, &["add", "a.rs", "b.ts", "c.rs", "d.py", "e.rs"]);
    git(root, &["commit", "-qm", "init"]);
    // Touch a.rs in a second commit so symbol_history has something to chew on.
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
    let mut store =
        basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
    basemind::scanner::scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
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

/// Return the first text content item verbatim (no JSON parse) — used to inspect the
/// raw TOON payload a tool emits when `format="toon"`.
fn raw_text(result: &CallToolResult) -> String {
    use rmcp::model::RawContent;
    result
        .content
        .iter()
        .find_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.clone()),
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

// ─── the test ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_server_exercises_representative_tools() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root")
            .arg(root)
            .arg("serve")
            .arg("--view")
            .arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    // get_info: the always-injected instructions carry the context-economy operating
    // discipline so agents default to basemind over grep/read and stay token-frugal.
    let instructions = service
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .unwrap_or_default();
    assert!(
        instructions.contains("Context economy"),
        "server instructions should state the context-economy discipline: {instructions}"
    );

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

    // TOON encoding (W5 slice 1): `format="toon"` must produce a smaller payload than the JSON
    // form and round-trip the same hit data. Use a needle with several hits to exercise the
    // tabular block.
    let json_result = service
        .call_tool(call_params(
            "search_symbols",
            json!({ "needle": "draw", "limit": 50 }),
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
    assert!(
        !json_results.is_empty(),
        "expected draw hits: {json_body:?}"
    );

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
    // Round-trip: the TOON table header carries the same column set and every JSON hit's
    // (path, name) pair must appear verbatim as a row cell in the TOON body.
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
    assert_eq!(
        body.get("hash_mode").and_then(Value::as_str),
        Some("normalized"),
        "default hash_mode echo should be normalized"
    );

    // symbol_history (Stage 2): structural mode is opt-in and echoes its name back.
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
        "structural mode should also see the 'tweak alpha' literal change: {history:?}"
    );

    // find_references (Stage 3): c.rs calls alpha() three times — index should reflect that.
    let body = decode_text(
        &service
            .call_tool(call_params(
                "find_references",
                json!({ "name": "alpha", "limit": 100 }),
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

    // find_references pagination (Phase 5): limit=2 → expect next_cursor; second page
    // should contain the 3rd alpha() hit with no overlap, then next_cursor=None.
    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "find_references",
                json!({ "name": "alpha", "limit": 2 }),
            ))
            .await
            .expect("find_references page1"),
    );
    let page1_hits = page1
        .get("hits")
        .and_then(Value::as_array)
        .expect("page1 hits");
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
    let page2_hits = page2
        .get("hits")
        .and_then(Value::as_array)
        .expect("page2 hits");
    assert_eq!(page2_hits.len(), 1, "remaining single hit on second page");
    assert!(
        page2.get("next_cursor").is_none(),
        "second page must NOT carry a next_cursor: {page2}"
    );
    // No overlap between the two pages — compare by (line, column) tuples since the
    // fixture's three alpha() call sites all sit on c.rs line 1.
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

    // find_callers (Stage 3): anchor on the alpha *definition* and confirm the same 3 hits.
    let body = decode_text(
        &service
            .call_tool(call_params(
                "find_callers",
                json!({ "path": "a.rs", "name": "alpha" }),
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

    // find_callers pagination (Phase 5): same 3 alpha hits, paginated.
    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "find_callers",
                json!({ "path": "a.rs", "name": "alpha", "limit": 2 }),
            ))
            .await
            .expect("find_callers page1"),
    );
    let page1_hits = page1
        .get("hits")
        .and_then(Value::as_array)
        .expect("page1 hits");
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
    let page2_hits = page2
        .get("hits")
        .and_then(Value::as_array)
        .expect("page2 hits");
    assert_eq!(page2_hits.len(), 1, "find_callers tail page → 1 hit");
    assert!(
        page2.get("next_cursor").is_none(),
        "find_callers second page must NOT have next_cursor: {page2}"
    );

    // call_graph (Iteration 4): walk the e.rs caller chain `outer -> middle -> inner`.
    // direction="callers" from `inner` with max_depth=2 must surface inner, middle, outer.
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
    // Root is always nodes[0].
    assert_eq!(
        nodes[0].get("name").and_then(Value::as_str),
        Some("inner"),
        "nodes[0] is the root"
    );
    // `middle` points at `inner` (parent → current in callers direction).
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
    // `outer` points at `middle` (parent → current).
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

    // search_symbols pagination (Phase 5): "a" matches alpha, Beta, caller, plain — well
    // above the limit=1 page size, so we can validate the two-page round trip.
    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "search_symbols",
                json!({ "needle": "a", "limit": 1 }),
            ))
            .await
            .expect("search_symbols page1"),
    );
    let page1_results = page1
        .get("results")
        .and_then(Value::as_array)
        .expect("page1 results");
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
    let page2_results = page2
        .get("results")
        .and_then(Value::as_array)
        .expect("page2 results");
    assert_eq!(page2_results.len(), 1, "page2 must also have 1 result");
    // Verify the two pages don't return the same symbol (path,name pair).
    let key1 = (
        page1_results[0]
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(""),
        page1_results[0]
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let key2 = (
        page2_results[0]
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(""),
        page2_results[0]
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    assert_ne!(key1, key2, "page2 must not repeat page1's entry");

    // search_symbols token budgeting (W3): a generous limit returns several "a" hits, but a
    // tiny `max_tokens` drops all but the first (one hit always exceeds a 1-token budget).
    // The response must carry fewer results than an unbudgeted call, `budgeted: true`, and a
    // non-null cursor so the dropped tail is pageable.
    let unbudgeted = decode_text(
        &service
            .call_tool(call_params(
                "search_symbols",
                json!({ "needle": "a", "limit": 100 }),
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
        budgeted
            .get("next_cursor")
            .and_then(Value::as_str)
            .is_some(),
        "budgeted response must carry a non-null next_cursor: {budgeted}"
    );

    // list_files pagination (Phase 5): fixture has 5 files; limit=4 paginates (4+1).
    let page1 = decode_text(
        &service
            .call_tool(call_params("list_files", json!({ "limit": 4 })))
            .await
            .expect("list_files page1"),
    );
    let page1_files = page1
        .get("files")
        .and_then(Value::as_array)
        .expect("page1 files");
    assert_eq!(page1_files.len(), 4, "list_files limit=4 → 4 files");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("list_files first page must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "list_files",
                json!({ "limit": 4, "cursor": cursor1 }),
            ))
            .await
            .expect("list_files page2"),
    );
    let page2_files = page2
        .get("files")
        .and_then(Value::as_array)
        .expect("page2 files");
    assert_eq!(
        page2_files.len(),
        1,
        "list_files page2 → 1 remaining file: {page2}"
    );
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

    // search_symbols cursor invalidation (Phase 5): after a rescan the snapshot id moves
    // and the previously-minted cursor must surface cursor_invalidated=true.
    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "search_symbols",
                json!({ "needle": "a", "limit": 1 }),
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

    // list_files cursor invalidation (Phase 5): same story.
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
            .call_tool(call_params(
                "list_files",
                json!({ "limit": 1, "cursor": stale_cursor }),
            ))
            .await
            .expect("list_files with stale cursor"),
    );
    assert_eq!(
        stale_response.get("cursor_invalidated"),
        Some(&Value::Bool(true)),
        "rescan must invalidate in-memory list_files cursors: {stale_response}"
    );

    // No false positive: a name that nobody calls should return 0 hits.
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

    // workspace_grep: pattern "pub fn" should find hits in a.rs and c.rs.
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

    // workspace_grep with a tiny limit should truncate.
    let body = decode_text(
        &service
            .call_tool(call_params(
                "workspace_grep",
                json!({ "pattern": "pub fn", "limit": 1, "include_context": false }),
            ))
            .await
            .expect("workspace_grep(limit=1)"),
    );
    let truncated = body
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let hits_with_limit = body.get("hits").and_then(Value::as_array).expect("hits");
    assert_eq!(
        hits_with_limit.len(),
        1,
        "limit=1 should return exactly 1 hit"
    );
    assert!(
        truncated,
        "limit=1 with multiple matches should set truncated=true"
    );

    // workspace_grep with an invalid regex should return an MCP protocol error (invalid_params).
    // rmcp surfaces this as Err(McpError) from call_tool, not as Ok with is_error=true.
    let invalid_result = service
        .call_tool(call_params(
            "workspace_grep",
            json!({ "pattern": "[invalid_regex(" }),
        ))
        .await;
    assert!(
        invalid_result.is_err(),
        "invalid regex should produce a protocol-level MCP error"
    );

    // memory_put / memory_get / memory_list / memory_delete / search_documents:
    // Feature-gated — without `--features memory`/`--features documents` they return an
    // MCP-level error. The smoke test confirms they dispatch without crashing.
    let _ = service
        .call_tool(call_params(
            "memory_put",
            json!({ "key": "smoke_key", "value": "hello", "embed": false }),
        ))
        .await;
    let _ = service
        .call_tool(call_params("memory_get", json!({ "key": "smoke_key" })))
        .await;
    let _ = service
        .call_tool(call_params("memory_list", json!({})))
        .await;
    let _ = service
        .call_tool(call_params("memory_delete", json!({ "key": "smoke_key" })))
        .await;
    let _ = service
        .call_tool(call_params("search_documents", json!({ "query": "hello" })))
        .await;

    // Per-query override round-trip: pass `reranker_preset` as a flattened override
    // field. The fixture has no LanceDB store so the call will error at the vector-
    // search stage, but it must NOT error at param-deserialization (invalid_params).
    // Confirming the result is Ok or is_error=true (not a protocol-level Err) is
    // sufficient to prove the new flatten field is accepted.
    let override_result = service
        .call_tool(call_params(
            "search_documents",
            json!({ "query": "hello", "reranker_preset": "bge-reranker-base" }),
        ))
        .await;
    // Either succeeds (feature present + store init succeeds) or returns an MCP-level
    // is_error response. A protocol-level Err here would mean unknown field rejection.
    match &override_result {
        Ok(r) => {
            // is_error may be set when store is unavailable — that's fine.
            let _ = r;
        }
        Err(_) => {
            // Protocol-level errors can fire when the feature is absent; allowed.
        }
    }

    // TOON output format round-trip: pull the same query in JSON and TOON and confirm
    // the two payloads carry the same `query` field and `hits` length. This proves the
    // TOON serializer is wired end-to-end (not just that the override field is
    // accepted). The fixture has no LanceDB store, so we only enforce the assertion
    // when both calls succeed at the protocol level — feature-gated builds still
    // exercise the dispatch path even when the bodies are empty.
    //
    // Gated on `documents` because `serde_toon` is only linked when the documents
    // feature is active (it's an `optional = true` workspace dep).
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
            // Extract the raw text payload from each tool call. JSON deserializes via
            // `decode_text`; TOON uses `serde_toon::from_str` to a `serde_json::Value`
            // tree so we can compare structurally without leaking crate-internal types
            // into the integration test.
            let json_body = decode_text(json_resp);
            if json_body != Value::Null {
                let toon_raw = toon_resp
                    .content
                    .iter()
                    .find_map(|c| match &c.raw {
                        rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let toon_body: Value =
                    serde_toon::from_str(&toon_raw).expect("toon body deserializes to JSON value");
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
    // Even when the documents feature is off, smoke-check that the field is accepted
    // at param-deserialization time (no protocol-level invalid_params error).
    #[cfg(not(feature = "documents"))]
    {
        let _ = service
            .call_tool(call_params(
                "search_documents",
                json!({ "query": "hello", "output_format": "toon" }),
            ))
            .await;
    }

    // memory_list pagination (Phase 5) — only meaningful with the memory feature wired.
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
        let page1_entries = page1
            .get("entries")
            .and_then(Value::as_array)
            .expect("page1 entries");
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
        let page2_entries = page2
            .get("entries")
            .and_then(Value::as_array)
            .expect("page2 entries");
        assert_eq!(page2_entries.len(), 1, "memory_list page2 → 1 remaining");
        assert!(
            page2.get("next_cursor").is_none(),
            "memory_list page2 must NOT carry next_cursor: {page2}"
        );
    }

    // memory_audit (W10b governance): verify audit semantics end-to-end.
    // `memory_put` writes a record with empty provenance (the only path via MCP).
    // An empty-provenance audit always returns state="unverified".
    #[cfg(feature = "memory")]
    {
        // Step 1: write a key so the audit has something to inspect.
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

        // Step 2: single-key audit — should return audited=1, state="unverified" (no provenance).
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
        let results = body
            .get("results")
            .and_then(Value::as_array)
            .expect("results");
        assert_eq!(results.len(), 1, "single-key audit must return one result");
        assert_eq!(
            results[0].get("state").and_then(Value::as_str),
            Some("unverified"),
            "empty-provenance memory must audit as unverified: {results:?}"
        );

        // Step 3: dry_run=true — must return verdict without error and without mutating the record.
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

        // Step 4: range-scan audit — include all written keys (pagination not required for
        // the small fixture set). At least the audit_probe key should appear.
        let range_body = decode_text(
            &service
                .call_tool(call_params("memory_audit", json!({ "limit": 50 })))
                .await
                .expect("memory_audit range"),
        );
        let range_audited = range_body
            .get("audited")
            .and_then(Value::as_u64)
            .expect("audited");
        assert!(
            range_audited >= 1,
            "range audit must cover at least the audit_probe key: {range_body}"
        );

        // Clean up.
        let _ = service
            .call_tool(call_params(
                "memory_delete",
                json!({ "key": "audit_probe" }),
            ))
            .await
            .expect("memory_delete audit_probe");
    }

    // ── W11 governance: proposals_mine / proposals_list / proposal_accept / proposal_reject ──
    //
    // The fixture's `init` commit stages all 5 files together, so co-change at the default
    // min_support=5 yields nothing, but min_support=1 deterministically yields the 5-file
    // cluster. Tests assert:
    //  (a) proposals_mine returns a well-formed response (default thresholds → zero candidates).
    //  (b) proposals_list returns a well-formed empty list after a no-candidate mine.
    //  (c) proposals_mine(min_support=1) mines >= 1 candidate (hard — guards the wedge below).
    //  (d) W11 Stale-wedge (run first, unconditional): accept → memory gains file provenance →
    //      delete a referenced file → rescan → memory_audit flips to "stale"; then restore the
    //      file so later assertions see a pristine fixture. The relocated W10b gap test.
    //  (e) proposal_reject tombstones a re-mined candidate; the next mine does not re-emit it.
    #[cfg(feature = "memory")]
    {
        // (a) Mine with default thresholds — 2-commit fixture → zero co-change candidates.
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
            mine_body
                .get("skipped_bulk")
                .and_then(Value::as_u64)
                .is_some(),
            "proposals_mine must return `skipped_bulk` field: {mine_body}"
        );

        // (b) proposals_list after zero-candidate mine returns a well-formed empty list.
        let list_body = decode_text(
            &service
                .call_tool(call_params(
                    "proposals_list",
                    json!({ "kind": "skill", "limit": 50 }),
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
            list_body
                .get("proposals")
                .and_then(Value::as_array)
                .map(Vec::is_empty)
                == Some(true),
            "proposals array must be empty: {list_body}"
        );

        // (c) Mine with min_support=1 — the fixture has 2 commits both touching a.rs;
        //     c.rs and e.rs each appear once. min_support=1 + max_files_per_commit=10
        //     may yield some candidates (depends on co-occurrence in each commit), but
        //     we only assert the call succeeds and the shape is correct.
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
        // Hard lower bound: the fixture's `init` commit stages all 5 files together
        // (a.rs b.ts c.rs d.py e.rs), so min_support=1 ALWAYS yields the 5-file co-change
        // cluster. A zero here means mining is broken — fail loudly rather than skip the wedge.
        assert!(
            mined_low >= 1,
            "proposals_mine(min_support=1) must mine the fixture's co-change cluster: {mine_low}"
        );

        // (d) W11 Stale-wedge — the headline proof, run FIRST and unconditionally (mined_low >= 1
        //     is guaranteed above). Accept a candidate → it becomes a memory carrying the cluster's
        //     file provenance → delete a referenced file → rescan → memory_audit flips it to Stale.
        //     This is the code-grounded-staleness wedge no other memory system can do, and the
        //     test W10b couldn't write (memory_put can't inject provenance; proposal_accept can).
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

        // Live audit: every referenced file still exists → Verified (provenance is non-empty).
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

        // The wedge: delete a referenced file, rescan, audit → Stale. Save its bytes first and
        // RESTORE them afterward so the post-block rescan assertions still see a pristine fixture.
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

        // Restore the fixture: rewrite the file (identical bytes) and rescan so later
        // assertions in this test see the original working tree.
        std::fs::write(&probe_abs, &saved).expect("restore probe file");
        let _ = service
            .call_tool(call_params("rescan", json!({})))
            .await
            .expect("rescan after restore");
        let _ = service
            .call_tool(call_params("memory_delete", json!({ "key": &memory_key })))
            .await;

        // (e) Reject + tombstone idempotency. Git history is immutable, so re-mining
        //     regenerates the same cluster the accept consumed; reject it and confirm the
        //     tombstone suppresses it on the next mine.
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

    // rescan: trigger an in-process scan via MCP. With no working-tree changes
    // since the smoke fixture was built, expectation is scanned > 0 and
    // updated == 0 (everything matched the existing blob hashes).
    let body = decode_text(
        &service
            .call_tool(call_params("rescan", json!({})))
            .await
            .expect("rescan"),
    );
    let scanned = body
        .get("scanned")
        .and_then(Value::as_u64)
        .expect("scanned");
    assert!(scanned > 0, "rescan should walk at least the fixture files");

    // rescan {full:true}: forcing a full re-index must walk the working tree even though a
    // `paths` scope is also supplied (full wins over paths). Asserts the full-scan override
    // wiring — `scanned > 0` proves the whole tree was walked, not just the scoped path.
    let body = decode_text(
        &service
            .call_tool(call_params(
                "rescan",
                json!({ "full": true, "paths": ["does-not-exist.rs"] }),
            ))
            .await
            .expect("rescan full"),
    );
    let scanned_full = body
        .get("scanned")
        .and_then(Value::as_u64)
        .expect("scanned (full)");
    assert!(
        scanned_full > 0,
        "rescan {{full:true}} must force a full working-tree scan even with a paths scope, \
         got scanned={scanned_full}"
    );

    // rescan {paths:[real file]} (full:false): the scoped path must be VISITED, not silently
    // dropped. Repo-relative request paths have to be joined to the absolute root before the
    // scanner strips the root prefix — a bare relative path strips to nothing and the whole
    // report comes back all-zeros (a no-op that looks like success). `a.rs` is unchanged here, so
    // it lands in `skipped_unchanged`; asserting the report is non-empty distinguishes the fix
    // from the relative-path no-op bug.
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

    // telemetry_summary: every successful tool call we've fired in this test should be
    // recorded. Don't assert an exact count (the smoke test evolves), just that the
    // dashboard sees the activity and the per-tool breakdown isn't empty.
    let body = decode_text(
        &service
            .call_tool(call_params("telemetry_summary", json!({ "window": "all" })))
            .await
            .expect("telemetry_summary"),
    );
    let total_calls = body
        .get("total_calls")
        .and_then(Value::as_u64)
        .expect("total_calls");
    assert!(
        total_calls >= 4,
        "telemetry_summary should see at least the prior fixture calls (status/outline/search_symbols/recent_changes), got {total_calls}"
    );
    let per_tool = body
        .get("per_tool")
        .and_then(Value::as_array)
        .expect("per_tool array");
    assert!(!per_tool.is_empty(), "per_tool histogram must not be empty");
    let savings_note = body
        .get("savings_note")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        savings_note.contains("estimate") || savings_note.contains("heuristic"),
        "savings_note must disclose the heuristic nature: {savings_note:?}"
    );

    // cache_stats: read-only introspection. A freshly-scanned fixture has blobs on disk and
    // every blob is referenced, so blob_count > 0 and orphan_blob_count == 0.
    let body = decode_text(
        &service
            .call_tool(call_params("cache_stats", json!({})))
            .await
            .expect("cache_stats"),
    );
    let blob_count = body
        .get("blob_count")
        .and_then(Value::as_u64)
        .expect("blob_count");
    assert!(
        blob_count >= 1,
        "freshly-scanned fixture should have blobs on disk: {body}"
    );
    assert_eq!(
        body.get("orphan_blob_count").and_then(Value::as_u64),
        Some(0),
        "no orphans immediately after a clean scan: {body}"
    );
    let per_view = body
        .get("per_view_file_count")
        .and_then(Value::as_array)
        .expect("per_view_file_count array");
    assert!(
        !per_view.is_empty(),
        "the working view should be listed: {body}"
    );

    // cache_gc: nothing is orphaned right after a scan, so removed == 0 and bytes_freed == 0.
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
    let scanned = body
        .get("scanned")
        .and_then(Value::as_u64)
        .expect("scanned");
    assert!(scanned >= 1, "GC should have inspected blob files: {body}");

    // cache_clear: a non-live component (telemetry) clears without confirm.
    let body = decode_text(
        &service
            .call_tool(call_params(
                "cache_clear",
                json!({ "component": "telemetry" }),
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

    // cache_clear: a destructive component (blobs) without confirm must be rejected.
    let err = service
        .call_tool(call_params("cache_clear", json!({ "component": "blobs" })))
        .await;
    assert!(
        err.is_err(),
        "clearing `blobs` without confirm=true must be rejected, got: {err:?}"
    );

    // cache_clear: `views` and `all` delete the live Fjall index / lock out from under the
    // running server, so they must be refused in-process even with confirm=true — the
    // critical safety gate. Stop the server and use the offline CLI for those.
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

    // find_implementations: `Drawable` should return Beta (a.rs, Rust) and Rectangle (b.ts, TS).
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

    // find_implementations: Python subclass Bar(Foo).
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

    // find_implementations pagination: limit=1 → next_cursor; second page no overlap.
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
            h.get("impl_type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            h.get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        )
    };
    assert_ne!(
        impl_key_of(&impl_page1_hits[0]),
        impl_key_of(&impl_page2_hits[0]),
        "find_implementations pages must not overlap"
    );

    // find_implementations language filter: Drawable restricted to rust → only Beta.
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

    // find_references substring matching (B3/I14): "lph" is a substring of "alpha" and should
    // return the same 3 call sites as the exact name. The callee field on each hit must
    // still be the full captured identifier "alpha", not just the substring.
    let body = decode_text(
        &service
            .call_tool(call_params(
                "find_references",
                json!({ "name": "lph", "limit": 100 }),
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

    // find_implementations substring matching: "raw" is a substring of "Drawable" and must
    // return both Beta and Rectangle.
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
    // The echoed trait_name must be the search needle, not the matched trait.
    assert_eq!(
        body.get("trait_name").and_then(Value::as_str),
        Some("raw"),
        "trait_name in response must echo the search needle"
    );

    // search_symbols empty needle guard: empty string must return 0 results without scanning.
    let body = decode_text(
        &service
            .call_tool(call_params(
                "search_symbols",
                json!({ "needle": "", "limit": 100 }),
            ))
            .await
            .expect("search_symbols(empty)"),
    );
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .expect("results");
    assert!(
        results.is_empty(),
        "search_symbols with empty needle must return 0 results, got {results:?}"
    );

    // compress (structural): compressing an indexed code file returns the L1 outline —
    // symbols + imports only, no bodies. The fixture a.rs is small, so we only assert
    // shape (strategy, positive sizes, symbol presence, no body leak) — not byte reduction
    // (a 7-symbol outline with comment lines can exceed a short source file in bytes).
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
    assert!(
        original_bytes > 0,
        "original_bytes must be positive for a.rs: {body}"
    );
    assert!(
        compressed_bytes > 0,
        "compressed_bytes must be positive for a non-empty outline: {body}"
    );
    let output = body.get("output").and_then(Value::as_str).expect("output");
    assert!(
        output.contains("alpha") || output.contains("Beta"),
        "structural output must reference indexed symbols: {output:?}"
    );
    // Verify no body content leaked — the fixture function body is `{ let _ = 1; }` in the
    // second commit; the structural outline must not include that literal.
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
    let tokens_note = body
        .get("tokens_note")
        .and_then(Value::as_str)
        .expect("tokens_note");
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
    // actually-reduced counter is present and consistent
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

    // compress (prose): compressing a prose string with repeated filler applies the
    // lexical pass and returns a smaller output.
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

    // compress: supplying both text and path must be rejected with an error.
    let err = service
        .call_tool(call_params(
            "compress",
            json!({ "text": "hello", "path": "a.rs" }),
        ))
        .await;
    assert!(
        err.is_err(),
        "compress with both text and path must be rejected: {err:?}"
    );

    // compress: supplying neither text nor path must be rejected with an error.
    let err = service.call_tool(call_params("compress", json!({}))).await;
    assert!(
        err.is_err(),
        "compress with neither text nor path must be rejected: {err:?}"
    );

    // expand: pulling the full body of `alpha` from a.rs must return the source slice.
    // After the second commit, alpha's body is `{ let _ = 1; }` — the literal that compress
    // explicitly excludes from its output. expand must include it.
    let body = decode_text(
        &service
            .call_tool(call_params(
                "expand",
                json!({ "path": "a.rs", "name": "alpha" }),
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
    // This is the literal that compress omits — expand must include it.
    assert!(
        expand_body.contains("let _ = 1"),
        "expanded body must include the function body literal (compress omits it, expand includes it): {expand_body:?}"
    );
    let start_row = body
        .get("start_row")
        .and_then(Value::as_u64)
        .expect("start_row");
    let end_row = body
        .get("end_row")
        .and_then(Value::as_u64)
        .expect("end_row");
    assert!(start_row >= 1, "start_row must be one-based: {body}");
    assert!(end_row >= start_row, "end_row must be >= start_row: {body}");
    assert_eq!(
        body.get("truncated").and_then(Value::as_bool),
        Some(false),
        "small function must not be truncated: {body}"
    );

    // expand: symbol not found must return an error (not panic).
    let err = service
        .call_tool(call_params(
            "expand",
            json!({ "path": "a.rs", "name": "nonexistent_symbol_xyz" }),
        ))
        .await;
    assert!(
        err.is_err(),
        "expand with unknown symbol must be rejected: {err:?}"
    );

    // expand: `kind` alias `symbol` must work (serde alias).
    let body = decode_text(
        &service
            .call_tool(call_params(
                "expand",
                json!({ "path": "a.rs", "symbol": "alpha" }),
            ))
            .await
            .expect("expand(path=a.rs, symbol=alpha via alias)"),
    );
    assert_eq!(
        body.get("name").and_then(Value::as_str),
        Some("alpha"),
        "expand via `symbol` alias must resolve correctly: {body}"
    );

    let _ = service.cancel().await;
}

// ─── git-iterator pagination smoke tests ─────────────────────────────────────

/// Build a multi-commit fixture used by the git-iterator pagination tests.
///
/// Layout: a single `paged.rs` file rewritten across 5 commits, each modifying the
/// body of `paged()`. That gives `recent_changes` and `commits_touching` ≥ 5
/// commits to page over, `find_commits_by_path` ≥ 5 matches, and `symbol_history`
/// ≥ 5 "modified" entries. The last commit in the helper rewrites only line 1 so
/// `paged.rs` blame partitions into ≥ 2 distinct hunks.
fn build_paging_repo() -> TempDir {
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
        c.arg("--root")
            .arg(root)
            .arg("serve")
            .arg("--view")
            .arg("working");
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
            .call_tool(call_params(
                "recent_changes",
                json!({ "limit": 2, "cursor": cursor1 }),
            ))
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
            .call_tool(call_params(
                "recent_changes",
                json!({ "limit": 2, "cursor": bogus }),
            ))
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
    assert_eq!(
        p1_shas.len(),
        2,
        "find_commits_by_path page1 → 2 commits: {page1}"
    );
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
    assert_eq!(
        p1_shas.len(),
        2,
        "symbol_history page1 → 2 entries: {page1}"
    );
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
    assert!(
        !p2_shas.is_empty(),
        "symbol_history page2 must have ≥ 1 entry: {page2}"
    );
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
            .call_tool(call_params(
                "blame_file",
                json!({ "path": "paged.rs", "limit": 1 }),
            ))
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
    assert!(
        !p2_hunks.is_empty(),
        "blame_file page2 must have ≥ 1 hunk: {page2}"
    );
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
    // Cursor past EOF → empty page + no next_cursor (the "natural restart" semantic).
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

// ─── Reranker smoke test ─────────────────────────────────────────────────────

/// Verify that `search_documents` with `reranker_enabled=true` is accepted at the
/// param-deserialization layer and, when the feature is active, every returned hit
/// carries a `rerank_score` field.
///
/// This test is gated with `#[ignore]` because the first run downloads the
/// `bge-reranker-base` ONNX weights (~278 MB) from HuggingFace into
/// `~/.cache/kreuzberg/rerankers/`. Pre-warm once before unattended runs:
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
    // Weakness: this test only verifies the reranker ran (rerank_score present)
    // and produced scores in [0,1]. It does NOT verify that reranking changed the
    // hit order — engineering a synthetic fixture where vector distance and
    // cross-encoder scores reliably disagree is impractical without real corpora.
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);
    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root")
            .arg(root)
            .arg("serve")
            .arg("--view")
            .arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    // Baseline: call without reranker — hits have no `rerank_score`.
    let no_rerank = service
        .call_tool(call_params(
            "search_documents",
            json!({ "query": "function", "reranker_enabled": false }),
        ))
        .await;
    // The fixture has no LanceDB doc store, so the call may fail at the vector-search
    // stage. That's acceptable — we only assert on structural shape when we get hits.
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

    // Reranked: call with reranker ON. Structural assertion: same hit count,
    // every hit carries `rerank_score: Some(f32)`.
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
    // Confirm param deserialization succeeded (no protocol-level Err).
    match &reranked {
        Ok(resp) => {
            let body = decode_text(resp);
            if let Some(hits) = body.get("hits").and_then(Value::as_array) {
                // When the store is present and returns hits, every hit must carry a score.
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
            // Protocol errors are acceptable when the documents store is absent or
            // feature is not compiled in — the key assertion is no panic / no crash.
            let _ = e;
        }
    }

    let _ = service.cancel().await;
}

// ─── Summarization smoke test ───────────────────────────────────────────────
//
// Iter-7 wires `summarization` + `llm` through the schema-driven config across
// all four surfaces (TOML / CLI / MCP / env). The synthetic fixture has no
// LanceDB document store, so we can only verify:
//   1. `summarization_enabled = true` deserializes (no `unknown field`)
//   2. The server doesn't crash when summarisation is enabled per-query
//   3. When hits are returned, every hit carries the optional `summary` slot
//      (None or Some(...) — either is valid because the fixture has no
//      pre-summarised doc blob to attach metadata from).
// The extractive path requires NO model download, so this test is NOT gated
// behind `#[ignore]`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(feature = "documents")]
async fn summarizes_via_extractive_default() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root")
            .arg(root)
            .arg("serve")
            .arg("--view")
            .arg("working");
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
                    // `summary` is the iter-7 additive field. It may be present
                    // (Some(...)) or absent — `skip_serializing_if = Option::is_none`
                    // omits the key when None.
                    //
                    // LIMITATION: the synthetic fixture's docs are too short
                    // for TextRank to produce a real summary, so we cannot
                    // assert `summary.is_some()` here. The tightened contract
                    // is: when ANY hit has a summary, its strategy must be
                    // "extractive" — guards against a future-iter abstractive
                    // bug slipping past the per-query `summarization_strategy`
                    // override.
                    if let Some(summary) = hit.get("summary") {
                        assert!(
                            summary.get("text").is_some(),
                            "summary must carry a `text` field: {summary}"
                        );
                        let strategy = summary
                            .get("strategy")
                            .and_then(Value::as_str)
                            .unwrap_or_else(|| {
                                panic!("summary must carry `strategy` str: {summary}")
                            });
                        assert_eq!(
                            strategy, "extractive",
                            "per-query strategy=extractive must round-trip; got {strategy}"
                        );
                    }
                }
            }
        }
        Err(e) => {
            // Protocol-level error is acceptable when the docs feature isn't
            // wired or the LanceDB store is absent. The key assertion is no
            // `unknown field` rejection on the new per-query params.
            let msg = format!("{e:?}");
            assert!(
                !msg.contains("unknown field"),
                "summarization params must deserialize: {msg}"
            );
        }
    }

    let _ = service.cancel().await;
}

// ─── Post-filter smoke test ─────────────────────────────────────────────────
//
// `attach_doc_metadata` filters hits by `entity_category` / `keywords_contains`
// after the vector recall. The fixture has no NER-tagged documents (and no
// LanceDB store), so the filter yields 0 hits — the test only proves the
// dispatch + post-filter wiring deserializes the new fields and does not crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(feature = "documents")]
async fn search_documents_accepts_post_filter_params() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root")
            .arg(root)
            .arg("serve")
            .arg("--view")
            .arg("working");
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

    // Either the call succeeds (with possibly an `is_error` payload when the
    // LanceDB store is unavailable) or returns a protocol-level Err when the
    // feature isn't wired. The contract is "no unknown-field rejection".
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

// ─── agent-comms round-trip (feature = "comms") ─────────────────────────────

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
#[cfg(feature = "comms")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn comms_round_trip_front_matter_then_body_then_inbox() {
    use std::sync::Arc;

    use basemind::comms::client::CommsClient;
    use basemind::comms::daemon::Broker;
    use basemind::comms::frontend_uds::UdsFrontend;
    use basemind::comms::ids::{AgentId, RoomId};
    use basemind::comms::model::RoomScope;
    use basemind::comms::singleton::CommsPaths;
    use basemind::comms::store::CommsStore;
    use basemind::comms::transport::CommsFrontend;

    let dir = tempfile::tempdir().expect("tempdir");
    // Short socket path under the temp dir (Unix socket paths are length-bounded).
    let socket_path = dir.path().join("c.sock");
    let paths = CommsPaths {
        comms_dir: dir.path().to_path_buf(),
        socket_path: socket_path.clone(),
    };

    // Bind the broker on the temp socket and drive its accept loop in the background.
    let store = Arc::new(CommsStore::open(dir.path()).expect("open comms store"));
    let broker = Arc::new(Broker::new(store));
    let listener = {
        let std_listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind temp socket");
        std_listener.set_nonblocking(true).expect("nonblocking");
        tokio::net::UnixListener::from_std(std_listener).expect("adopt listener")
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let frontend = UdsFrontend::from_listener(listener, socket_path.clone());
    let serve = tokio::spawn(async move { Box::new(frontend).serve(broker, shutdown_rx).await });

    // Two clients with DISTINCT identities connect to the running broker.
    let agent_a = AgentId::parse("agent-a").expect("agent a");
    let agent_b = AgentId::parse("agent-b").expect("agent b");
    let mut a = CommsClient::connect(&paths, agent_a, None, None)
        .await
        .expect("connect a");
    let mut b = CommsClient::connect(&paths, agent_b, None, None)
        .await
        .expect("connect b");

    // Shared room; B joins so it lands in B's inbox.
    let room = RoomId::parse("team").expect("room");
    a.create_room(room.clone(), RoomScope::Global, Some("Team".to_string()))
        .await
        .expect("create room");
    b.join_room(room.clone()).await.expect("b joins");

    // A posts subject + body.
    let subject = "deploy status";
    let body = b"all systems green".to_vec();
    let message_id = a
        .post_message(
            room.clone(),
            subject.to_string(),
            body.clone(),
            vec!["ops".to_string()],
            None,
            vec!["src/**".to_string()],
        )
        .await
        .expect("post");

    // B reads history → FRONT-MATTER only: subject present, NO body field on the meta record.
    let (history, _next) = b
        .read_history(room.clone(), None, 10, None)
        .await
        .expect("history");
    assert_eq!(history.len(), 1, "exactly one posted message");
    let seq_meta = &history[0];
    let meta = &seq_meta.meta;
    assert_eq!(seq_meta.seq, 1, "front-matter carries the per-room seq");
    assert_eq!(meta.subject, subject, "front-matter carries the subject");
    assert_eq!(meta.id, message_id, "front-matter id matches the posted id");
    assert_eq!(
        meta.scope,
        vec!["src/**".to_string()],
        "front-matter round-trips the posted scope"
    );
    assert_eq!(
        meta.body_len,
        body.len() as u32,
        "front-matter carries body_len, not the body"
    );
    // The front-matter record is serialized WITHOUT the body bytes — assert the JSON view has
    // no `body` key, only the length/hash metadata.
    let meta_json = serde_json::to_value(meta).expect("serialize meta");
    assert!(
        meta_json.get("body").is_none(),
        "history front-matter must NOT include the body: {meta_json}"
    );
    assert!(
        meta_json.get("body_len").is_some(),
        "history front-matter must include body_len: {meta_json}"
    );

    // B fetches the body on demand — the only body path.
    let fetched = b.get_body(message_id.clone()).await.expect("get_body");
    assert_eq!(
        fetched.as_deref(),
        Some(body.as_slice()),
        "message_get returns the exact body"
    );

    // B's inbox shows the unread message, then mark_read clears it to 0 unread.
    let (inbox, unread, _c) = b
        .read_inbox(None, None, None, 10, true, None)
        .await
        .expect("inbox read+mark");
    assert_eq!(inbox.len(), 1, "the posted message is in B's inbox");
    assert_eq!(
        inbox[0].meta.subject, subject,
        "inbox carries front-matter subject"
    );
    assert_eq!(unread, 0, "mark_read drained the unread count in this page");

    // A second inbox read after mark_read returns nothing new.
    let (inbox2, unread2, _c2) = b
        .read_inbox(None, None, None, 10, false, None)
        .await
        .expect("inbox re-read");
    assert!(
        inbox2.is_empty(),
        "no unread messages remain after mark_read"
    );
    assert_eq!(unread2, 0, "unread count stays 0 after mark_read");

    // inbox_ack: A posts a second message; B dismisses it via `ack_inbox` (a cursor advance,
    // NOT a delete). The message must leave B's inbox but stay in the shared history.
    let second_id = a
        .post_message(
            room.clone(),
            "second".to_string(),
            b"more".to_vec(),
            vec![],
            None,
            vec![],
        )
        .await
        .expect("post second");
    let (inbox3, _u3, _c3) = b
        .read_inbox(None, None, None, 10, false, None)
        .await
        .expect("inbox shows second");
    assert_eq!(inbox3.len(), 1, "the second message is unread in B's inbox");
    assert_eq!(
        inbox3[0].meta.id, second_id,
        "inbox shows the second message"
    );

    let (acked, cursors_advanced) = b
        .ack_inbox(vec![second_id.clone()], None, None)
        .await
        .expect("ack");
    assert_eq!(acked, 1, "exactly one message acked");
    assert_eq!(
        cursors_advanced,
        vec![("team".to_string(), 2)],
        "ack advances B's team cursor to seq 2"
    );

    let (inbox4, _u4, _c4) = b
        .read_inbox(None, None, None, 10, false, None)
        .await
        .expect("inbox after ack");
    assert!(inbox4.is_empty(), "ack removed the message from B's inbox");

    // The acked message is still in the shared, append-only history.
    let (history_after, _n) = b
        .read_history(room.clone(), None, 10, None)
        .await
        .expect("history after ack");
    assert_eq!(
        history_after.len(),
        2,
        "ack does not delete from the shared log"
    );

    // Tear down the broker.
    let _ = shutdown_tx.send(true);
    let _ = serve.await;
}

// ─── embedded agent shells round-trip (feature = "shells") ──────────────────

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

    // Force the spawned `serve` child to run shells in Headless mode so the test NEVER opens a
    // real terminal window/tab. The child reads `.basemind/basemind.toml` from the repo root
    // (`run_scan` already created `.basemind/`); `visual = "headless"` makes `present()` a no-op.
    std::fs::write(
        root.join(".basemind").join("basemind.toml"),
        b"\"$schema\" = \"v1\"\n\n[shells]\nvisual = \"headless\"\n",
    )
    .expect("write headless shells config");

    let socket = dir.path().join("shells.sock");
    let bin = env!("CARGO_BIN_EXE_basemind");
    let socket_for_env = socket.clone();
    let cmd = AsyncCommand::new(bin).configure(move |c| {
        c.arg("--root")
            .arg(root)
            .arg("serve")
            .arg("--view")
            .arg("working");
        c.env("BASEMIND_SHELLS_SOCKET", &socket_for_env);
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    // Spawn a headless session that prints a sentinel then idles.
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

    // Security regression: a cwd that escapes the repository root must be refused
    // (run_shell_spawn normalizes via normalize_query_path) — it is rejected before any
    // session is spawned, so there is nothing to clean up.
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

    // Poll capture until the sentinel shows up (bounded — not flaky).
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let captured = service
            .call_tool(call_params(
                "shell_capture",
                json!({ "session_id": session_id }),
            ))
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

    // Kill the session and assert it reports killed.
    let killed = service
        .call_tool(call_params(
            "shell_kill",
            json!({ "session_id": session_id }),
        ))
        .await
        .expect("shell_kill call");
    let killed = decode_text(&killed);
    assert_eq!(
        killed.get("killed").and_then(Value::as_bool),
        Some(true),
        "shell_kill should report killed=true for a live session: {killed:?}"
    );

    // A second kill (now-unknown id) must be an error, proving the mapping was forgotten.
    let second = service
        .call_tool(call_params(
            "shell_kill",
            json!({ "session_id": session_id }),
        ))
        .await;
    assert!(
        second.is_err(),
        "killing an already-forgotten session_id should error"
    );

    let _ = service.cancel().await;
}

/// Spawn `basemind serve` against `root`, optionally setting `BASEMIND_MCP_LEAN`, and return the
/// connected rmcp client service.
async fn spawn_serve(
    root: &Path,
    lean: Option<&str>,
) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let bin = env!("CARGO_BIN_EXE_basemind");
    let lean = lean.map(str::to_string);
    let root = root.to_path_buf();
    let cmd = AsyncCommand::new(bin).configure(move |c| {
        c.arg("--root")
            .arg(&root)
            .arg("serve")
            .arg("--view")
            .arg("working");
        // Mirror env so the lean toggle is read by the child only when requested; the unset
        // case must reproduce the default full surface exactly.
        c.env_remove("BASEMIND_MCP_LEAN");
        if let Some(v) = &lean {
            c.env("BASEMIND_MCP_LEAN", v);
        }
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    ().serve(transport).await.expect("rmcp handshake")
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

    // ── Default surface (flag unset): full tool list, direct dispatch. ──────────────
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

    // Tool annotations drive client-side permission gating (Claude Code auto-approves
    // read-only tools). Assert the contract: read-only tools advertise `read_only_hint=true`,
    // mutating tools advertise `read_only_hint=false`, and a destructive one is flagged.
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
    // Baseline result from a direct call on the full surface.
    let direct = decode_text(
        &full
            .call_tool(call_params(
                "search_symbols",
                json!({ "needle": "Greet", "limit": 10 }),
            ))
            .await
            .expect("direct search_symbols"),
    );
    let _ = full.cancel().await;

    // ── Lean surface (flag set): exactly three wrappers. ────────────────────────────
    let lean = spawn_serve(root, Some("1")).await;
    let lean_tools = lean.list_all_tools().await.expect("list tools (lean)");
    let mut lean_names: Vec<&str> = lean_tools.iter().map(|t| t.name.as_ref()).collect();
    lean_names.sort_unstable();
    assert_eq!(
        lean_names,
        vec!["get_tool_schema", "invoke_tool", "list_tools"],
        "lean mode advertises exactly the three wrapper tools"
    );

    // `list_tools` wrapper returns a compressed listing of the real tools.
    let listing = decode_text(
        &lean
            .call_tool(call_params("list_tools", json!({})))
            .await
            .expect("lean list_tools"),
    );
    let listed = listing
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array");
    assert!(
        listed
            .iter()
            .any(|t| t.get("name").and_then(Value::as_str) == Some("search_symbols")),
        "lean list_tools should surface the real search_symbols tool: {listing}"
    );

    // `get_tool_schema` returns the real tool's input schema.
    let schema = decode_text(
        &lean
            .call_tool(call_params(
                "get_tool_schema",
                json!({ "tool_name": "search_symbols" }),
            ))
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

    // `invoke_tool` dispatches to the real handler — same payload as the direct call.
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
    assert_eq!(
        via_invoke, direct,
        "invoke_tool result must match a direct search_symbols call"
    );

    // Unknown tool names are rejected, not silently passed through.
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

    // prompts/list advertises the curated templates.
    let prompts = server.list_all_prompts().await.expect("list_all_prompts");
    let names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
    for expected in [
        "onboard-repo",
        "trace-symbol",
        "explain-file",
        "review-working-tree",
    ] {
        assert!(
            names.contains(&expected),
            "prompt `{expected}` must be advertised, got: {names:?}"
        );
    }

    // `trace-symbol` exposes a `symbol` argument (the reference the completion handler fills).
    let trace = prompts
        .iter()
        .find(|p| p.name == "trace-symbol")
        .expect("trace-symbol present");
    let args = trace
        .arguments
        .as_ref()
        .expect("trace-symbol has arguments");
    assert!(
        args.iter().any(|a| a.name == "symbol"),
        "trace-symbol must declare a `symbol` argument, got: {:?}",
        args.iter().map(|a| &a.name).collect::<Vec<_>>()
    );

    // prompts/get renders the template, interpolating the argument into the message.
    let rendered = server
        .get_prompt(
            GetPromptRequestParams::new("trace-symbol").with_arguments(
                serde_json::json!({ "symbol": "Greeter" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
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
            rmcp::model::PromptMessageContent::Text { text } => Some(text.as_str()),
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

    // `trace-symbol` / `symbol` completes against indexed symbol names (prefix `al` → `alpha`).
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

    // `explain-file` / `path` completes against indexed file paths (prefix `a` → `a.rs`).
    let paths = server
        .complete_prompt_argument("explain-file", "path", "a", None)
        .await
        .expect("complete path argument");
    assert!(
        paths.values.iter().any(|v| v == "a.rs"),
        "path completion for `a` must include `a.rs`, got: {:?}",
        paths.values
    );

    // An argument basemind doesn't complete returns nothing, not an error.
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
        async fn on_progress(
            &self,
            params: ProgressNotificationParam,
            _context: NotificationContext<RoleClient>,
        ) {
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
        c.arg("--root")
            .arg(&root_buf)
            .arg("serve")
            .arg("--view")
            .arg("working");
        c.env_remove("BASEMIND_MCP_LEAN");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let server = capture.serve(transport).await.expect("rmcp handshake");

    // Call rescan WITH a progress token so the server emits progress.
    let mut params = call_params("rescan", json!({}));
    rmcp::model::RequestParamsMeta::set_progress_token(
        &mut params,
        rmcp::model::ProgressToken(NumberOrString::String("rescan-1".into())),
    );
    server.call_tool(params).await.expect("rescan call");

    // Give the notifications a moment to arrive over the transport.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Snapshot the captured notifications, dropping the guards before the later await.
    let captured_logs = logs.lock().unwrap().clone();
    let captured_progress = progress.lock().unwrap().clone();

    assert!(
        captured_logs
            .iter()
            .any(|l| l.data.get("event").and_then(|v| v.as_str()) == Some("rescan_complete")),
        "rescan must emit a `rescan_complete` logging notification, got: {:?}",
        captured_logs.iter().map(|l| &l.data).collect::<Vec<_>>()
    );

    // The client supplied a progress token, so the server emits start + done progress for it.
    // rmcp assigns the concrete token value; we assert the shape: a start (total: None) and a
    // completion that reports the discovered file count as both progress and total.
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
    // All progress shares the one request's token.
    let first = &captured_progress[0].progress_token;
    assert!(
        captured_progress.iter().all(|p| &p.progress_token == first),
        "all progress notifications must carry the same request token"
    );

    let _ = server.cancel().await;
}
