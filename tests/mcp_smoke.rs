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
    // c.rs calls alpha() three times so the reference index has something to chew on.
    std::fs::write(
        root.join("c.rs"),
        b"pub fn caller() { alpha(); alpha(); other(); alpha(); }\n",
    )
    .unwrap();
    git(root, &["add", "a.rs", "b.ts", "c.rs"]);
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

    // list_files pagination (Phase 5): fixture has 3 files; limit=2 paginates.
    let page1 = decode_text(
        &service
            .call_tool(call_params("list_files", json!({ "limit": 2 })))
            .await
            .expect("list_files page1"),
    );
    let page1_files = page1
        .get("files")
        .and_then(Value::as_array)
        .expect("page1 files");
    assert_eq!(page1_files.len(), 2, "list_files limit=2 → 2 files");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("list_files first page must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "list_files",
                json!({ "limit": 2, "cursor": cursor1 }),
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

    let _ = service.cancel().await;
}
