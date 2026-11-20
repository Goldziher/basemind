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
                basemind::retrieval_wiring::backend(),
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
    basemind::wire::decode(&raw).unwrap_or(Value::Null)
}

/// The hits of a response as rows, each with its `path`. `code_refs` lists hits under their file
/// (`{"<path>": [hit, ...]}`); the path is put back on each. `code_grep` lists `"<line>: <text>"`
/// rows under their file and enclosing symbol (`{"<path>": {"<symbol>": [row, ...]}}`); each match
/// becomes `{path, line, text, symbol?, column?}`, one per column a row names, and context rows are
/// left out. A plain list (`code_search`, `docs_search`) is returned as it is.
fn hit_rows(body: &Value) -> Vec<Value> {
    match body.get("hits") {
        Some(Value::Object(files)) => files
            .iter()
            .flat_map(|(path, rows)| match rows {
                Value::Object(groups) => groups
                    .iter()
                    .flat_map(|(symbol, rows)| {
                        rows.as_array()
                            .expect("a symbol's rows")
                            .iter()
                            .flat_map(move |row| grep_row(path, symbol, row.as_str().expect("a row")))
                    })
                    .collect::<Vec<_>>(),
                rows => rows
                    .as_array()
                    .expect("a file's hits")
                    .iter()
                    .map(|row| {
                        let mut row = row.clone();
                        row["path"] = json!(path);
                        row
                    })
                    .collect(),
            })
            .collect(),
        Some(Value::Array(rows)) => rows.clone(),
        _ => panic!("no hits: {body}"),
    }
}

/// The matches of one `code_grep` row, `"<line>[:<column>,...]: <text>"`; none for a context row.
fn grep_row(path: &str, symbol: &str, row: &str) -> Vec<Value> {
    let digits = row.bytes().take_while(u8::is_ascii_digit).count();
    if row.as_bytes().get(digits) != Some(&b':') {
        return Vec::new();
    }
    let line: u64 = row[..digits].parse().expect("a line");
    let rest = &row[digits + 1..];
    let (columns, text): (Vec<u64>, &str) = match rest.split_once(": ") {
        Some((columns, text)) if !columns.is_empty() && columns.bytes().all(|b| b.is_ascii_digit() || b == b',') => {
            (columns.split(',').map(|c| c.parse().expect("a column")).collect(), text)
        }
        _ => (Vec::new(), rest.strip_prefix(' ').unwrap_or(rest)),
    };
    let mut hit = json!({ "path": path, "line": line, "text": text });
    if !symbol.is_empty() {
        hit["symbol"] = json!(symbol);
    }
    if columns.is_empty() {
        return vec![hit];
    }
    columns
        .into_iter()
        .map(|column| {
            let mut hit = hit.clone();
            hit["column"] = json!(column);
            hit
        })
        .collect()
}

/// A `code_symbols` answer's rows as `(path, row)`, `row` being what follows the path
/// (`"<line>: <signature>"`), best first.
fn symbol_rows(body: &Value) -> Vec<(String, String)> {
    body.get("results")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("results: {body}"))
        .iter()
        .map(|row| {
            let row = row.as_str().expect("a row is a string");
            let (path, rest) = row.split_once(':').expect("`<path>:<line>: ...`");
            (path.to_string(), rest.to_string())
        })
        .collect()
}

/// `forged` in place of the tool's own cursor inside `real`, a `next_cursor` the server issued. The
/// result is still bound to the query that issued `real`, so the tool body sees `forged`.
fn rebound(real: &str, forged: &str) -> String {
    let (binding, _) = real
        .split_once('.')
        .expect("a cursor the server issued is bound to its query");
    format!("{binding}.{forged}")
}

/// The message of a verb-tool failure. A verb tool reports every failure as an `isError` result
/// whose first text block is the plain message, never as a JSON-RPC error.
fn error_text(result: &CallToolResult) -> String {
    use rmcp::model::ContentBlock;
    assert_eq!(
        result.is_error,
        Some(true),
        "expected an isError tool result: {result:?}"
    );
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
        instructions.contains("before shell `rg`/`grep`/`find`/`git`"),
        "server instructions should state the prefer-basemind-over-grep directive: {instructions}"
    );
    assert!(
        instructions.contains("fraction of the tokens"),
        "server instructions should state the context-economy rationale: {instructions}"
    );
    assert!(
        instructions.contains("code_outline") && instructions.contains("git_log"),
        "server instructions should name the verb tools: {instructions}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("index_status", json!({})))
            .await
            .expect("index_status"),
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
        .call_tool(call_params("code_outline", json!({ "path": "a.rs" })))
        .await
        .expect("code_outline");
    assert_structured_matches_text(&outline_result);
    let body = decode_text(&outline_result);
    let symbols = body.get("symbols").and_then(Value::as_array).expect("symbols");
    let alpha = symbols
        .iter()
        .find(|s| s.get("name").and_then(Value::as_str) == Some("alpha"))
        .expect("alpha in the outline");
    assert_eq!(
        alpha.get("line").and_then(Value::as_u64),
        Some(1),
        "outline lines are 1-based: {body}"
    );
    assert!(
        symbols.iter().all(|s| {
            let line = s.get("line").and_then(Value::as_u64).unwrap_or(0);
            line >= 1 && s.get("end_line").and_then(Value::as_u64).unwrap_or(0) >= line
        }),
        "every outline symbol must carry a 1-based inclusive `line`..`end_line` span: {body}"
    );
    assert!(
        symbols.iter().all(|s| s.get("start_row").is_none()),
        "the outline no longer reports 0-based rows: {body}"
    );
    assert!(
        body.get("calls").is_none() && body.get("docs").is_none(),
        "the outline carries symbols and imports only: {body}"
    );
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
                "code_symbols",
                json!({ "name": "Greet", "kind": "function", "limit": 10 }),
            ))
            .await
            .expect("code_symbols"),
    );
    let results = symbol_rows(&body);
    assert_eq!(
        results.len(),
        1,
        "Stage 2 arrow-fn const should be kind=function: {body}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("code_symbols", json!({ "name": "draw", "limit": 50 })))
            .await
            .expect("code_symbols(draw)"),
    );
    assert!(!symbol_rows(&body).is_empty(), "expected draw hits: {body:?}");
    assert!(
        body.get("truncated").is_none(),
        "no sweep hit its cap, and a false flag is left out: {body}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("git_log", json!({ "limit": 5 })))
            .await
            .expect("git_log"),
    );
    let commits = body.get("commits").and_then(Value::as_array).expect("commits");
    assert_eq!(commits.len(), 2, "two commits expected");
    assert!(
        body.get("truncated").is_none() || body.get("truncated") == Some(&Value::Bool(false)),
        "non-shallow clone should not surface truncated=true"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("git_log", json!({ "query": "tweak", "limit": 10 })))
            .await
            .expect("git_log search"),
    );
    let hits = body.get("commits").and_then(Value::as_array).expect("commits");
    assert_eq!(hits.len(), 1, "one commit summary contains 'tweak', got {hits:?}");
    assert!(
        hits[0]
            .as_str()
            .and_then(|row| row.split_once(' '))
            .is_some_and(|(_, summary)| summary.contains("tweak")),
        "hit summary should contain the query token"
    );
    let body = decode_text(
        &service
            .call_tool(call_params("git_log", json!({ "author": "tweak" })))
            .await
            .expect("git_log search author scope"),
    );
    assert_eq!(
        body.get("commits").and_then(Value::as_array).map(Vec::len),
        Some(0),
        "'tweak' is a message token, not an author token"
    );
    let body = decode_text(
        &service
            .call_tool(call_params("git_log", json!({ "query": "tweak", "author": "t" })))
            .await
            .expect("git_log search AND same-commit"),
    );
    assert_eq!(
        body.get("commits").and_then(Value::as_array).map(Vec::len),
        Some(1),
        "the 'tweak alpha' commit was written by `t`"
    );
    let body = decode_text(
        &service
            .call_tool(call_params("git_log", json!({ "query": "tweak", "author": "alpha" })))
            .await
            .expect("git_log search, author term only in the message"),
    );
    assert_eq!(
        body.get("commits").and_then(Value::as_array).map(Vec::len),
        Some(0),
        "'alpha' is in the message of that commit, not in its author"
    );
    let body = decode_text(
        &service
            .call_tool(call_params("git_log", json!({ "query": "init", "author": "tweak" })))
            .await
            .expect("git_log search AND cross-commit"),
    );
    assert_eq!(
        body.get("commits").and_then(Value::as_array).map(Vec::len),
        Some(0),
        "'init' (c1) AND 'tweak' (c2) share no commit"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "git_log",
                json!({ "path": "a.rs", "symbol": "alpha", "limit": 10 }),
            ))
            .await
            .expect("git_log symbol"),
    );
    let history = body.get("history").and_then(Value::as_array).expect("history");
    let modifieds = history
        .iter()
        .filter(|row| {
            let row = row.as_str().expect("a history row is a string");
            !row.contains(" [introduced] ") && !row.contains(" [removed] ")
        })
        .count();
    assert!(
        modifieds >= 1,
        "expected ≥ 1 'modified' entry for alpha after the tweak commit: {history:?}"
    );
    assert!(
        body.get("hash_mode").is_none(),
        "the fingerprint mode is not echoed: {body}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("code_refs", json!({ "name": "alpha", "limit": 100 })))
            .await
            .expect("code_refs"),
    );
    let hits = hit_rows(&body);
    assert_eq!(hits.len(), 3, "expected 3 alpha() call sites: {body}");
    assert!(
        hits.iter().all(|h| h.get("callee").is_none()),
        "a callee that is the name asked for is not repeated on every hit: {body}"
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
    // Rust has no resolution engine, and `alpha` is defined: every row is `name_only`, the default
    // a page leaves unwritten, so nothing about resolution is written at all.
    assert!(
        hits.iter().all(|h| h.get("resolution").is_none())
            && body.get("resolution_default").is_none()
            && body.get("resolution_counts").is_none(),
        "a row equal to the default leaves it out: {body}"
    );
    let other = decode_text(
        &service
            .call_tool(call_params("code_refs", json!({ "name": "other" })))
            .await
            .expect("code_refs(other)"),
    );
    assert_eq!(
        other["resolution_default"], "external",
        "`other` is called in c.rs and defined nowhere: {other}"
    );
    let filtered = decode_text(
        &service
            .call_tool(call_params(
                "code_refs",
                json!({ "name": "alpha", "resolution": "same_file,imported" }),
            ))
            .await
            .expect("code_refs filtered"),
    );
    assert!(hit_rows(&filtered).is_empty(), "no alpha() site is proven: {filtered}");
    assert_eq!(
        filtered["resolution_counts"],
        json!({ "name_only": 3 }),
        "the counts say what the filter dropped: {filtered}"
    );

    let page1 = decode_text(
        &service
            .call_tool(call_params("code_refs", json!({ "name": "alpha", "limit": 2 })))
            .await
            .expect("code_refs page1"),
    );
    let page1_hits = hit_rows(&page1);
    assert_eq!(page1_hits.len(), 2, "limit=2 → 2 hits on first page");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("first page must carry a next_cursor when more remain")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "code_refs",
                json!({ "name": "alpha", "limit": 2, "cursor": cursor1 }),
            ))
            .await
            .expect("code_refs page2"),
    );
    let page2_hits = hit_rows(&page2);
    assert_eq!(page2_hits.len(), 1, "remaining single hit on second page");
    assert!(
        page2.get("next_cursor").is_none(),
        "second page must NOT carry a next_cursor: {page2}"
    );
    // All three calls are on one line of c.rs. A call site writes its column when another call of the
    // whole result shares its line, so every page tells its hits apart from the other pages' hits.
    let pos = |h: &Value| -> (u64, Option<u64>) {
        (
            h.get("line").and_then(Value::as_u64).unwrap_or(0),
            h.get("column").and_then(Value::as_u64),
        )
    };
    let p1_pos: Vec<(u64, Option<u64>)> = page1_hits.iter().map(pos).collect();
    assert!(
        p1_pos.iter().all(|(_, column)| column.is_some()) && p1_pos[0] != p1_pos[1],
        "two hits on one line carry distinct columns: {page1}"
    );
    let p2_pos: Vec<(u64, Option<u64>)> = page2_hits.iter().map(pos).collect();
    assert!(
        p2_pos.len() == 1 && p2_pos[0].0 == p1_pos[0].0 && p2_pos[0].1.is_some() && !p1_pos.contains(&p2_pos[0]),
        "the hit left for page 2 is the third on that line and keeps its own column: {page2}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("code_refs", json!({ "path": "a.rs", "name": "alpha" })))
            .await
            .expect("code_refs with `path`"),
    );
    let def = body.get("definition").expect("definition echoed");
    assert_eq!(
        def.get("name").and_then(Value::as_str),
        Some("alpha"),
        "definition should resolve to alpha"
    );
    let hits = hit_rows(&body);
    assert_eq!(hits.len(), 3, "code_refs with `path` should see the same 3 sites");

    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "code_refs",
                json!({ "path": "a.rs", "name": "alpha", "limit": 2 }),
            ))
            .await
            .expect("code_refs with `path` page1"),
    );
    let page1_hits = hit_rows(&page1);
    assert_eq!(page1_hits.len(), 2, "code_refs with `path` limit=2 → 2 hits");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("code_refs with `path` first page must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "code_refs",
                json!({ "path": "a.rs",
                    "name": "alpha",
                    "limit": 2,
                    "cursor": cursor1,
                }),
            ))
            .await
            .expect("code_refs with `path` page2"),
    );
    let page2_hits = hit_rows(&page2);
    assert_eq!(page2_hits.len(), 1, "code_refs with `path` tail page → 1 hit");
    assert!(
        page2.get("next_cursor").is_none(),
        "code_refs with `path` second page must NOT have next_cursor: {page2}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "code_refs",
                json!({ "name": "alpha", "relation": "definition", "path": "a.rs", "line": 1 }),
            ))
            .await
            .expect("code_refs definition"),
    );
    assert_eq!(
        body.get("path").and_then(Value::as_str),
        Some("a.rs"),
        "code_refs definition must echo the queried path: {body}"
    );
    assert_eq!(
        body.get("line").and_then(Value::as_u64),
        Some(1),
        "code_refs definition must echo the normalized 1-based line: {body}"
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

    let page1 = decode_text(
        &service
            .call_tool(call_params("code_symbols", json!({ "name": "a", "limit": 1 })))
            .await
            .expect("code_symbols page1"),
    );
    let page1_results = symbol_rows(&page1);
    assert_eq!(page1_results.len(), 1, "code_symbols limit=1 → 1 result");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("first page must carry next_cursor when more remain")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "code_symbols",
                json!({ "name": "a", "limit": 1, "cursor": cursor1 }),
            ))
            .await
            .expect("code_symbols page2"),
    );
    let page2_results = symbol_rows(&page2);
    assert_eq!(page2_results.len(), 1, "page2 must also have 1 result");
    let key1 = &page1_results[0];
    let key2 = &page2_results[0];
    assert_ne!(key1, key2, "page2 must not repeat page1's entry");

    let unbudgeted = decode_text(
        &service
            .call_tool(call_params("code_symbols", json!({ "name": "a", "limit": 100 })))
            .await
            .expect("code_symbols unbudgeted"),
    );
    let unbudgeted_len = symbol_rows(&unbudgeted).len();
    assert!(
        unbudgeted_len >= 2,
        "fixture must have ≥2 'a' symbols to exercise budgeting, got {unbudgeted_len}"
    );
    let budgeted = decode_text(
        &service
            .call_tool(call_params(
                "code_symbols",
                json!({ "name": "a", "limit": 100, "max_tokens": 1 }),
            ))
            .await
            .expect("code_symbols budgeted"),
    );
    let budgeted_results = symbol_rows(&budgeted);
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
            .call_tool(call_params("find_files", json!({ "limit": 4 })))
            .await
            .expect("find_files page1"),
    );
    let page1_files = page1.get("files").and_then(Value::as_array).expect("page1 files");
    assert_eq!(page1_files.len(), 4, "find_files limit=4 → 4 files");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("find_files first page must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params("find_files", json!({ "limit": 4, "cursor": cursor1 })))
            .await
            .expect("find_files page2"),
    );
    let page2_files = page2.get("files").and_then(Value::as_array).expect("page2 files");
    assert_eq!(page2_files.len(), 3, "find_files page2 → 3 remaining files: {page2}");
    assert!(
        page2.get("next_cursor").is_none(),
        "find_files page2 must NOT carry next_cursor"
    );
    let p1_paths: Vec<&str> = page1_files.iter().filter_map(Value::as_str).collect();
    let p2_paths: Vec<&str> = page2_files.iter().filter_map(Value::as_str).collect();
    assert!(
        p2_paths.iter().all(|p| !p1_paths.contains(p)),
        "find_files pages must not overlap: {p1_paths:?} vs {p2_paths:?}"
    );

    let page1 = decode_text(
        &service
            .call_tool(call_params("code_symbols", json!({ "name": "a", "limit": 1 })))
            .await
            .expect("code_symbols pre-rescan"),
    );
    let stale_cursor = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("pre-rescan cursor")
        .to_string();
    let _ = service
        .call_tool(call_params("index_rescan", json!({})))
        .await
        .expect("rescan");
    let stale_response = refused(
        &service,
        "code_symbols",
        json!({ "name": "a", "limit": 1, "cursor": stale_cursor }),
    )
    .await;
    assert!(
        stale_response.contains("changed since this cursor was issued"),
        "rescan must invalidate in-memory code_symbols cursors: {stale_response}"
    );

    let page1 = decode_text(
        &service
            .call_tool(call_params("find_files", json!({ "limit": 1 })))
            .await
            .expect("find_files pre-rescan"),
    );
    let stale_cursor = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("find_files pre-rescan cursor")
        .to_string();
    let _ = service
        .call_tool(call_params("index_rescan", json!({})))
        .await
        .expect("rescan");
    let stale_response = refused(&service, "find_files", json!({ "limit": 1, "cursor": stale_cursor })).await;
    assert!(
        stale_response.contains("changed since this cursor was issued"),
        "rescan must invalidate in-memory find_files cursors: {stale_response}"
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
        files[0].as_str(),
        Some("cyc1.rs"),
        "find_files('cy1') must rank cyc1.rs, listed by its path alone: {body}"
    );
    assert!(
        body.get("total").is_none(),
        "find_files('cy1') lists every match, so there is no count: {body}"
    );
    assert!(
        body.get("truncated").is_none() && body.get("next_cursor").is_none(),
        "find_files('cy1') is one whole page, and a listing has no `truncated`: {body}"
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
    assert!(
        body.get("total").is_none(),
        "find_files(no match) lists every match, so there is no count: {body}"
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
        files.first().and_then(Value::as_str),
        Some("d.py"),
        "find_files('d') should rank d.py first (only path starting with d): {body}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("code_refs", json!({ "name": "no_such_callee_anywhere" })))
            .await
            .expect("code_refs(missing)"),
    );
    let hits = hit_rows(&body);
    assert!(hits.is_empty(), "unknown callee should yield no hits");

    let body = decode_text(
        &service
            .call_tool(call_params("git_blame", json!({ "path": "a.rs" })))
            .await
            .expect("git_blame"),
    );
    let hunks = body.get("hunks").and_then(Value::as_array).expect("hunks");
    assert!(!hunks.is_empty(), "blame should return hunks on a real file");

    let body = decode_text(
        &service
            .call_tool(call_params("code_grep", json!({ "pattern": "pub fn" })))
            .await
            .expect("code_grep"),
    );
    let grep_hits = hit_rows(&body);
    assert!(
        !grep_hits.is_empty(),
        "code_grep for 'pub fn' should find hits in the fixture"
    );
    assert!(
        grep_hits
            .iter()
            .all(|h| h.get("line").and_then(Value::as_u64).unwrap_or(0) >= 1),
        "every grep hit must carry a 1-based line"
    );
    assert!(
        body.get("total").is_none(),
        "a page without a cursor lists every hit: {body}"
    );
    let total_matches = grep_hits.len() as u64;
    assert!(
        total_matches >= 3,
        "fixture has alpha + doit + caller = 3+ 'pub fn' occurrences, got {total_matches}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("code_grep", json!({ "pattern": "pub fn", "limit": 1 })))
            .await
            .expect("code_grep(limit=1)"),
    );
    let hits_with_limit = hit_rows(&body);
    assert_eq!(hits_with_limit.len(), 1, "limit=1 should return exactly 1 hit");
    assert!(
        body.get("next_cursor").is_some_and(Value::is_string),
        "limit=1 with multiple matches pages: {body}"
    );
    assert!(
        body.get("truncated").is_none(),
        "a `limit` cut is paging, not truncation, and a false flag is left out: {body}"
    );
    assert!(body.get("truncated_reason").is_none(), "{body}");
    let capped_total = body.get("total").and_then(Value::as_u64).expect("total");
    assert_eq!(
        capped_total, total_matches,
        "`limit` caps hits, never the scan: the match count is the same as the unlimited call"
    );

    let invalid = service
        .call_tool(call_params(
            "code_grep",
            json!({ "pattern": "[invalid_regex(", "regex": true }),
        ))
        .await
        .expect("code_grep(invalid regex)");
    let message = error_text(&invalid);
    assert!(
        message.contains("invalid regex") && message.contains("Drop `regex`"),
        "an invalid regex must be reported with its fix: {message}"
    );
    let literal = decode_text(
        &service
            .call_tool(call_params("code_grep", json!({ "pattern": "[invalid_regex(" })))
            .await
            .expect("code_grep(literal)"),
    );
    assert_eq!(
        literal.get("hits"),
        Some(&json!({})),
        "without `regex` the same text is a literal search, not an error: {literal}"
    );

    let _ = service
        .call_tool(call_params(
            "memory_save",
            json!({ "key": "smoke_key", "value": "hello" }),
        ))
        .await;
    let read = service
        .call_tool(call_params("memory_search", json!({ "key": "smoke_key" })))
        .await
        .expect("memory_search key");
    let missing = service
        .call_tool(call_params("memory_search", json!({ "key": "decision/nope" })))
        .await
        .expect("memory_search missing key");
    #[cfg(feature = "memory")]
    {
        assert_eq!(
            read.structured_content.as_ref().map(|note| &note["value"]),
            Some(&json!("hello"))
        );
        assert_eq!(
            missing.structured_content,
            Some(json!({ "key": "decision/nope", "found": false })),
            "a missing note is an object, never `null`: MCP types structuredContent as an object"
        );
        assert_structured_matches_text(&missing);
    }
    #[cfg(not(feature = "memory"))]
    let _ = (read, missing);
    let listing = service
        .call_tool(call_params("memory_search", json!({})))
        .await
        .expect("memory_search");
    #[cfg(feature = "memory")]
    let _ = listing;
    #[cfg(not(feature = "memory"))]
    {
        let message = error_text(&listing);
        assert!(
            message.contains("`memory_search` needs the `memory` Cargo feature"),
            "memory_search without the memory feature must name the missing feature: {message}"
        );
    }

    #[cfg(not(feature = "code-search"))]
    {
        let result = service
            .call_tool(call_params("code_search", json!({ "query": "hello" })))
            .await
            .expect("code_search");
        let message = error_text(&result);
        assert!(
            message.contains("`code_search` needs the `code-search` Cargo feature"),
            "code_search without the code-search feature must name the missing feature: {message}"
        );
    }
    #[cfg(feature = "code-search")]
    {
        for args in [
            json!({ "query": "hello" }),
            json!({ "query": "hello", "lane": "keyword" }),
            json!({ "query": "hello", "lane": "hybrid" }),
        ] {
            let result = service
                .call_tool(call_params("code_search", args.clone()))
                .await
                .expect("code_search");
            // No embedding model on the test host is a legitimate isError; a success must be well formed.
            if result.is_error != Some(true) {
                let body = decode_text(&result);
                assert_eq!(
                    body.get("query").and_then(Value::as_str),
                    Some("hello"),
                    "code_search {args} must echo the input query field: {body}"
                );
                assert!(
                    body.get("hits").and_then(Value::as_array).is_some(),
                    "code_search {args} response must carry a hits array (may be empty): {body}"
                );
            }
        }

        let bad_lane = service
            .call_tool(call_params("code_search", json!({ "query": "hello", "lane": "bogus" })))
            .await
            .expect("code_search(bogus lane)");
        let message = error_text(&bad_lane);
        assert!(
            message.contains("`lane` must be one of `hybrid`, `semantic`, `keyword`") && message.contains("bogus"),
            "code_search must reject an unknown lane: {message}"
        );
    }

    let docs = service
        .call_tool(call_params("docs_search", json!({ "query": "hello" })))
        .await
        .expect("docs_search");
    #[cfg(feature = "documents")]
    let _ = docs;
    #[cfg(not(feature = "documents"))]
    {
        let message = error_text(&docs);
        assert!(
            message.contains("`docs_search` needs the `documents` Cargo feature"),
            "docs_search without the documents feature must name the missing feature: {message}"
        );
    }

    #[cfg(feature = "memory")]
    {
        for i in 0..3 {
            // The note is durable before `memory_save` embeds it. Where no embedding model is
            // available the save still succeeds and says the note is not embedded: an error here
            // would report a write that happened as one that did not.
            let saved = service
                .call_tool(call_params(
                    "memory_save",
                    json!({ "key": format!("paging_key_{i}"),
                        "value": format!("v{i}"),
                    }),
                ))
                .await
                .expect("memory_save");
            assert_ne!(saved.is_error, Some(true), "a saved note is a success: {saved:?}");
            let saved = decode_text(&saved);
            match saved.get("embedded").and_then(Value::as_bool) {
                Some(true) => assert!(saved.get("notice").is_none(), "{saved}"),
                Some(false) => assert!(
                    saved
                        .get("notice")
                        .and_then(Value::as_str)
                        .is_some_and(|notice| notice.contains("the note is saved")),
                    "a note that could not be embedded says so: {saved}"
                ),
                None => panic!("memory_save must report `embedded`: {saved}"),
            }
        }
        let page1 = decode_text(
            &service
                .call_tool(call_params(
                    "memory_search",
                    json!({ "prefix": "paging_key_", "limit": 2 }),
                ))
                .await
                .expect("memory_search page1"),
        );
        let page1_entries = page1.get("entries").and_then(Value::as_array).expect("page1 entries");
        assert_eq!(page1_entries.len(), 2, "memory_search limit=2 → 2 entries");
        let cursor1 = page1
            .get("next_cursor")
            .and_then(Value::as_str)
            .expect("memory_search first page must carry next_cursor")
            .to_string();
        assert!(
            page1.get("truncated").is_none() && page1.get("truncated_reason").is_none(),
            "a `limit` cut is paging, not truncation, and a false flag is left out: {page1}"
        );
        assert_eq!(page1.get("total").and_then(Value::as_u64), Some(3), "{page1}");
        let page2 = decode_text(
            &service
                .call_tool(call_params(
                    "memory_search",
                    json!({ "prefix": "paging_key_",
                        "limit": 2,
                        "cursor": cursor1,
                    }),
                ))
                .await
                .expect("memory_search page2"),
        );
        let page2_entries = page2.get("entries").and_then(Value::as_array).expect("page2 entries");
        assert_eq!(page2_entries.len(), 1, "memory_search page2 → 1 remaining");
        assert!(
            page2.get("next_cursor").is_none(),
            "memory_search page2 must NOT carry next_cursor: {page2}"
        );
    }

    let body = decode_text(
        &service
            .call_tool(call_params("index_rescan", json!({})))
            .await
            .expect("rescan"),
    );
    let scanned = body.get("scanned").and_then(Value::as_u64).expect("scanned");
    assert!(scanned > 0, "rescan should walk at least the fixture files");

    // `full` did nothing `{}` does not do, and silently discarded `paths` beside it. It is not an
    // argument: a call that passes it is told so instead of being run as something else.
    let refused = service
        .call_tool(call_params(
            "index_rescan",
            json!({ "full": true, "paths": ["does-not-exist.rs"] }),
        ))
        .await
        .expect("a tool result");
    assert_eq!(refused.is_error, Some(true), "{refused:?}");
    let message = error_text(&refused);
    assert!(
        message.contains("full") && message.contains("paths"),
        "the refusal names the unknown field and the accepted one: {message}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("index_rescan", json!({ "paths": ["a.rs"] })))
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

    let body = decode_text(
        &service
            .call_tool(call_params(
                "code_refs",
                json!({ "relation": "implementations", "name": "Drawable", "limit": 100 }),
            ))
            .await
            .expect("code_refs implementations(Drawable)"),
    );
    let hits = hit_rows(&body);
    let impl_types: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.get("impl_type").and_then(Value::as_str))
        .collect();
    assert!(
        impl_types.contains(&"Beta"),
        "code_refs implementations(Drawable) must include Beta from a.rs: {impl_types:?}"
    );
    assert!(
        impl_types.contains(&"Rectangle"),
        "code_refs implementations(Drawable) must include Rectangle from b.ts: {impl_types:?}"
    );
    assert!(
        hits.iter()
            .all(|h| h.get("line").and_then(Value::as_u64).unwrap_or(0) >= 1),
        "every code_refs implementations hit must carry a 1-based line"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "code_refs",
                json!({ "relation": "implementations", "name": "Foo", "limit": 100 }),
            ))
            .await
            .expect("code_refs implementations(Foo)"),
    );
    let hits = hit_rows(&body);
    let impl_types: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.get("impl_type").and_then(Value::as_str))
        .collect();
    assert!(
        impl_types.contains(&"Bar"),
        "code_refs implementations(Foo) must include Bar from d.py: {impl_types:?}"
    );

    let impl_page1 = decode_text(
        &service
            .call_tool(call_params(
                "code_refs",
                json!({ "relation": "implementations", "name": "Drawable", "limit": 1 }),
            ))
            .await
            .expect("code_refs implementations page1"),
    );
    let impl_page1_hits = hit_rows(&impl_page1);
    assert_eq!(
        impl_page1_hits.len(),
        1,
        "limit=1 must return exactly 1 implementation hit"
    );
    let impl_cursor1 = impl_page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("code_refs implementations first page must carry next_cursor when ≥2 implementors exist")
        .to_string();
    let impl_page2 = decode_text(
        &service
            .call_tool(call_params(
                "code_refs",
                json!({ "relation": "implementations", "name": "Drawable", "limit": 1, "cursor": impl_cursor1 }),
            ))
            .await
            .expect("code_refs implementations page2"),
    );
    let impl_page2_hits = hit_rows(&impl_page2);
    assert_eq!(
        impl_page2_hits.len(),
        1,
        "code_refs implementations page2 must return the remaining hit"
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
        "code_refs implementations pages must not overlap"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "code_refs",
                json!({ "name": "lph", "match": "substring", "limit": 100 }),
            ))
            .await
            .expect("code_refs(substring)"),
    );
    let hits = hit_rows(&body);
    assert_eq!(
        hits.len(),
        3,
        "code_refs(\"lph\") must return the 3 alpha() call sites via substring: {body}"
    );
    assert!(
        hits.iter()
            .all(|h| h.get("callee").and_then(Value::as_str) == Some("alpha")),
        "every substring hit must carry the full callee=\"alpha\", not the substring"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "code_refs",
                json!({ "relation": "implementations", "name": "raw", "match": "substring", "limit": 100 }),
            ))
            .await
            .expect("code_refs implementations(substring)"),
    );
    let hits = hit_rows(&body);
    let impl_types: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.get("impl_type").and_then(Value::as_str))
        .collect();
    assert!(
        impl_types.contains(&"Beta"),
        "code_refs implementations(\"raw\") must include Beta via substring on \"Drawable\": {impl_types:?}"
    );
    assert!(
        impl_types.contains(&"Rectangle"),
        "code_refs implementations(\"raw\") must include Rectangle via substring on \"Drawable\": {impl_types:?}"
    );
    assert!(body.get("trait_name").is_none(), "the call is not echoed back: {body}");
    assert!(
        hits.iter()
            .all(|h| h.get("trait_name").and_then(Value::as_str) == Some("Drawable")),
        "a substring match names the trait each hit implements: {body}"
    );

    let message = error_text(
        &service
            .call_tool(call_params("code_symbols", json!({ "name": "", "limit": 100 })))
            .await
            .expect("code_symbols(empty)"),
    );
    assert!(
        message.contains("`name` is empty"),
        "code_symbols with an empty name is refused, as code_grep with an empty pattern is: {message}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params("code_read", json!({ "path": "a.rs", "symbol": "alpha" })))
            .await
            .expect("code_read(path=a.rs, symbol=alpha)"),
    );
    assert_eq!(
        body.get("name").and_then(Value::as_str),
        Some("alpha"),
        "code_read must echo the resolved name: {body}"
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
        "expanded body must include the function body literal : {expand_body:?}"
    );
    let line = body.get("line").and_then(Value::as_u64).expect("line");
    let end_line = body.get("end_line").and_then(Value::as_u64).expect("end_line");
    assert_eq!(line, 1, "alpha is defined on the first line (1-based): {body}");
    assert!(end_line >= line, "end_line must be >= line: {body}");
    assert!(
        body.get("truncated").is_none(),
        "a small function is not truncated, and a false `truncated` is left out: {body}"
    );

    let missing = service
        .call_tool(call_params(
            "code_read",
            json!({ "path": "a.rs", "symbol": "nonexistent_symbol_xyz" }),
        ))
        .await
        .expect("code_read(unknown symbol)");
    let message = error_text(&missing);
    assert!(
        message.contains("no symbol `nonexistent_symbol_xyz` in `a.rs`") && message.contains("alpha (line 1)"),
        "an unknown symbol must be refused naming the symbols the file does have: {message}"
    );

    let body = decode_text(
        &service
            .call_tool(call_params(
                "code_read",
                json!({ "path": "a.rs", "line": 1, "end_line": 2 }),
            ))
            .await
            .expect("code_read(line span)"),
    );
    assert_eq!(
        (
            body.get("line").and_then(Value::as_u64),
            body.get("end_line").and_then(Value::as_u64),
            body.get("total_lines").and_then(Value::as_u64),
        ),
        (Some(1), Some(2), Some(7)),
        "a span read echoes its 1-based inclusive span and the file's line count: {body}"
    );
    let span = body.get("body").and_then(Value::as_str).expect("span body");
    assert!(
        span.contains("pub fn alpha") && span.contains("pub struct Beta") && !span.contains("impl Beta"),
        "a span read returns exactly the requested lines: {span:?}"
    );

    let _ = service.cancel().await;
}

/// Build a multi-commit fixture used by the git-iterator pagination tests.
///
/// Layout: a single `paged.rs` file rewritten across 5 commits, each modifying the
/// body of `paged()`. That gives `git_log` ≥ 5 commits to page over, with or without a
/// `path`, and ≥ 5 "modified" entries for the symbol history. The last commit in the helper rewrites only line 1 so
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

/// The shas of a list of `"<sha> <summary>"` rows.
fn row_shas(rows: Option<&Value>) -> Vec<String> {
    rows.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|row| row.as_str()?.split_once(' ').map(|(sha, _)| sha.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn commit_shas(body: &Value) -> Vec<String> {
    row_shas(body.get("commits"))
}

/// The first line of each blame hunk row, `"<line>[-<end_line>] <sha>"`.
fn blame_starts(hunks: &[Value]) -> Vec<u64> {
    hunks
        .iter()
        .filter_map(|row| row.as_str()?.split([' ', '-']).next()?.parse().ok())
        .collect()
}

/// `git rev-parse HEAD` of `repo`.
fn head_sha(repo: &Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .expect("git in PATH");
    assert!(out.status.success(), "git rev-parse HEAD failed");
    String::from_utf8(out.stdout).expect("utf8").trim().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_recent_paginates_with_stable_cursor() {
    let (_dir, service) = spawn_paging_server().await;
    wait_for_git_history(&service).await;
    let page1 = decode_text(
        &service
            .call_tool(call_params("git_log", json!({ "limit": 2 })))
            .await
            .expect("git_log page1"),
    );
    let p1_shas = commit_shas(&page1);
    assert_eq!(p1_shas.len(), 2, "git_log limit=2 → 2 commits");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("git_log page1 must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params("git_log", json!({ "limit": 2, "cursor": &cursor1 })))
            .await
            .expect("git_log page2"),
    );
    let p2_shas = commit_shas(&page2);
    assert_eq!(p2_shas.len(), 2, "git_log page2 → 2 more commits");
    assert!(
        p2_shas.iter().all(|s| !p1_shas.contains(s)),
        "git_log pages must not overlap: {p1_shas:?} vs {p2_shas:?}"
    );
    let bogus = rebound(&cursor1, &basemind::testing::encode_in_memory_cursor(0, 0xDEAD_BEEF));
    let stale = refused(&service, "git_log", json!({ "limit": 2, "cursor": bogus })).await;
    assert!(
        stale.contains("changed since this cursor was issued"),
        "a cursor from another HEAD is an error, not an empty page: {stale}"
    );
    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_touching_paginates_with_stable_cursor() {
    let (_dir, service) = spawn_paging_server().await;
    wait_for_git_history(&service).await;
    let page1 = decode_text(
        &service
            .call_tool(call_params("git_log", json!({ "path": "paged.rs", "limit": 2 })))
            .await
            .expect("git_log path page1"),
    );
    let p1_shas = commit_shas(&page1);
    assert_eq!(p1_shas.len(), 2, "git_log path page1 → 2 commits");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("git_log path must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "git_log",
                json!({ "path": "paged.rs", "limit": 2, "cursor": &cursor1 }),
            ))
            .await
            .expect("git_log path page2"),
    );
    let p2_shas = commit_shas(&page2);
    assert_eq!(p2_shas.len(), 2, "git_log path page2 → 2 more commits");
    assert!(
        p2_shas.iter().all(|s| !p1_shas.contains(s)),
        "git_log path pages must not overlap: {p1_shas:?} vs {p2_shas:?}"
    );
    let bogus = rebound(&cursor1, &basemind::testing::encode_in_memory_cursor(0, 0xDEAD_BEEF));
    let stale = refused(
        &service,
        "git_log",
        json!({ "path": "paged.rs", "limit": 2, "cursor": bogus }),
    )
    .await;
    assert!(
        stale.contains("changed since this cursor was issued"),
        "a cursor from another HEAD is an error, not an empty page: {stale}"
    );
    let _ = service.cancel().await;
}

/// Wait (bounded) until `git_log` answers from the git-history index, which the server builds after
/// it starts. A `git_log` cursor counts in the list it was issued on, and one issued on the live walk
/// is refused once the index answers, so a test of stable paging pages after that point. An index
/// that never comes up fails the test: the server is meant to build it, unless `BASEMIND_GH_INDEX=0`
/// turns it off, and then every page is the live walk.
async fn wait_for_git_history(service: &rmcp::service::RunningService<rmcp::RoleClient, ()>) {
    if std::env::var("BASEMIND_GH_INDEX").is_ok_and(|value| value == "0") {
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        let probe = decode_text(
            &service
                .call_tool(call_params("git_log", json!({ "query": "step" })))
                .await
                .expect("git_log query"),
        );
        if probe.get("truncated").is_none() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("the git-history index did not come up within 60 s of `serve` starting");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_symbol_history_paginates_with_stable_cursor() {
    let (_dir, service) = spawn_paging_server().await;
    wait_for_git_history(&service).await;
    let history_shas = |body: &Value| -> Vec<String> { row_shas(body.get("history")) };
    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "git_log",
                json!({ "path": "paged.rs", "symbol": "paged", "limit": 2 }),
            ))
            .await
            .expect("git_log symbol page1"),
    );
    let p1_shas = history_shas(&page1);
    assert_eq!(p1_shas.len(), 2, "git_log symbol page1 → 2 entries: {page1}");
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("git_log symbol must carry next_cursor")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "git_log",
                json!({
                    "path": "paged.rs",
                    "symbol": "paged",
                    "limit": 2,
                    "cursor": &cursor1,
                }),
            ))
            .await
            .expect("git_log symbol page2"),
    );
    let p2_shas = history_shas(&page2);
    assert!(!p2_shas.is_empty(), "git_log symbol page2 must have ≥ 1 entry: {page2}");
    assert!(
        p2_shas.iter().all(|s| !p1_shas.contains(s)),
        "git_log symbol pages must not overlap: {p1_shas:?} vs {p2_shas:?}"
    );
    let bogus = rebound(&cursor1, &basemind::testing::encode_in_memory_cursor(0, 0xDEAD_BEEF));
    let stale = refused(
        &service,
        "git_log",
        json!({
            "path": "paged.rs",
            "symbol": "paged",
            "limit": 2,
            "cursor": bogus,
        }),
    )
    .await;
    assert!(
        stale.contains("changed since this cursor was issued"),
        "a cursor from another HEAD is an error, not an empty page: {stale}"
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
    let _ = service.call_tool(call_params("index_rescan", json!({}))).await;
    let page1 = decode_text(
        &service
            .call_tool(call_params("git_blame", json!({ "path": "paged.rs", "limit": 1 })))
            .await
            .expect("git_blame page1"),
    );
    let p1_hunks = page1
        .get("hunks")
        .and_then(Value::as_array)
        .expect("git_blame page1 hunks");
    assert_eq!(p1_hunks.len(), 1, "git_blame limit=1 → 1 hunk: {page1}");
    let p1_start = blame_starts(p1_hunks);
    let cursor1 = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("git_blame must carry next_cursor when more hunks remain")
        .to_string();
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "git_blame",
                json!({ "path": "paged.rs", "limit": 1, "cursor": &cursor1 }),
            ))
            .await
            .expect("git_blame page2"),
    );
    let p2_hunks = page2
        .get("hunks")
        .and_then(Value::as_array)
        .expect("git_blame page2 hunks");
    assert!(!p2_hunks.is_empty(), "git_blame page2 must have ≥ 1 hunk: {page2}");
    let p2_start = blame_starts(p2_hunks);
    assert_eq!(
        p1_start.len(),
        p1_hunks.len(),
        "every page1 hunk carries a `line`: {page1}"
    );
    assert_eq!(
        p2_start.len(),
        p2_hunks.len(),
        "every page2 hunk carries a `line`: {page2}"
    );
    assert!(
        p2_start.iter().all(|s| !p1_start.contains(s)),
        "git_blame pages must not overlap by line: {p1_start:?} vs {p2_start:?}"
    );

    let suspect = head_sha(dir.path());
    let revision = u32::from_str_radix(&suspect[..8], 16).expect("hex sha");
    let past_the_end = rebound(&cursor1, &basemind::testing::encode_in_memory_cursor(9_999, revision));
    let page_empty = decode_text(
        &service
            .call_tool(call_params(
                "git_blame",
                json!({ "path": "paged.rs", "limit": 1, "cursor": past_the_end }),
            ))
            .await
            .expect("git_blame cursor past end"),
    );
    assert_eq!(
        page_empty.get("hunks").and_then(Value::as_array).map(Vec::len),
        Some(0),
        "a cursor past the last hunk is an empty page: {page_empty}"
    );
    assert!(
        page_empty.get("next_cursor").is_none(),
        "an exhausted blame carries no next_cursor: {page_empty}"
    );
    let other_revision = rebound(&cursor1, &basemind::testing::encode_in_memory_cursor(1, revision ^ 1));
    let stale = refused(
        &service,
        "git_blame",
        json!({ "path": "paged.rs", "limit": 1, "cursor": other_revision }),
    )
    .await;
    assert!(
        stale.contains("changed since this cursor was issued"),
        "a blame cursor from another revision is refused: {stale}"
    );
    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_blame_symbol_paginates_by_start_line() {
    let (dir, service) = spawn_paging_server().await;
    split_blame_lines(dir.path());
    let _ = service.call_tool(call_params("index_rescan", json!({}))).await;
    let page1 = decode_text(
        &service
            .call_tool(call_params(
                "git_blame",
                json!({ "path": "paged.rs", "symbol": "paged", "limit": 1 }),
            ))
            .await
            .expect("git_blame symbol page1"),
    );
    let p1_hunks = page1
        .get("hunks")
        .and_then(Value::as_array)
        .expect("git_blame symbol page1 hunks");
    assert_eq!(p1_hunks.len(), 1, "git_blame symbol limit=1 → 1 hunk: {page1}");
    let p1_start = *blame_starts(p1_hunks)
        .first()
        .expect("git_blame symbol page1 start_line");
    assert!(
        p1_start >= 1,
        "git_blame symbol start_line should be 1-based, got {p1_start}"
    );
    assert!(
        page1.get("next_cursor").is_none(),
        "the symbol's one hunk is the whole answer, so there is no next page: {page1}"
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
    std::fs::write(
        root.join("guide.md"),
        "# Functions\n\nEvery function in this project returns a value and logs how long it took.\n",
    )
    .unwrap();

    /// The hits and the `rerank` report (`ran`, and the `reason` it did not) of one search.
    async fn hits_for(root: &Path, config: &str) -> (Vec<Value>, Value) {
        std::fs::write(root.join("basemind.toml"), config).unwrap();
        run_scan(root);
        let transport = basemind::mcp::serve_in_memory(root, "working")
            .await
            .expect("in-memory serve");
        let service = ().serve(transport).await.expect("rmcp handshake");
        let response = service
            .call_tool(call_params("docs_search", json!({ "query": "function" })))
            .await
            .expect("docs_search answers");
        assert_ne!(response.is_error, Some(true), "{}", error_text(&response));
        let body = decode_text(&response);
        let hits = body["hits"].as_array().cloned().unwrap_or_default();
        let _ = service.cancel().await;
        assert!(!hits.is_empty(), "the indexed guide must be found");
        (hits, body["rerank"].clone())
    }

    // `$schema` is required: without it basemind.toml does not parse and the defaults (reranker
    // off) apply with only a logged warning.
    let documents = "\"$schema\" = \"v1\"\n\n[documents]\nenabled = true\n";
    let (hits, rerank) = hits_for(root, documents).await;
    assert_eq!(rerank["ran"], json!(false), "reranker off: {rerank}");
    for hit in hits {
        assert!(hit.get("rerank_score").is_none(), "reranker off: {hit}");
    }
    // The reranker is driven by `[documents.reranker]` in basemind.toml, not by the query.
    let reranked = format!("{documents}\n[documents.reranker]\nenabled = true\npreset = \"bge-reranker-base\"\n");
    let (hits, rerank) = hits_for(root, &reranked).await;
    assert_eq!(rerank["ran"], json!(true), "reranker on did not run: {rerank}");
    for hit in hits {
        let score = hit["rerank_score"]
            .as_f64()
            .unwrap_or_else(|| panic!("reranker on: {hit}"));
        assert!(
            (0.0..=1.0).contains(&score),
            "rerank_score must be in [0, 1], got {score}"
        );
    }
}

// behind `#[ignore]`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(feature = "documents")]
async fn summarizes_via_extractive_default() {
    let dir = build_repo();
    let root = dir.path();
    std::fs::write(
        root.join("basemind.toml"),
        "[documents.summarization]\nenabled = true\nstrategy = \"extractive\"\nmax_tokens = 100\n",
    )
    .unwrap();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let result = service
        .call_tool(call_params(
            "docs_search",
            json!({ "query": "test",
                "limit": 5,
            }),
        ))
        .await;

    match &result {
        Ok(resp) => {
            if resp.is_error == Some(true) {
                let message = error_text(resp);
                assert!(
                    !message.contains("unknown field"),
                    "summarization params must be accepted: {message}"
                );
            }
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
        Err(e) => panic!("docs_search must answer with a tool result: {e:?}"),
    }

    let _ = service.cancel().await;
}

/// A `docs_search` test checks nothing without an embedding model, so being unable to load one is
/// a failure, not a pass. Where no model can be loaded (an offline machine), setting
/// `BASEMIND_TEST_ALLOW_NO_EMBEDDER` turns it into a reported skip.
#[cfg(feature = "documents")]
fn skip_without_embedder(what: &str) {
    assert!(
        std::env::var_os("BASEMIND_TEST_ALLOW_NO_EMBEDDER").is_some(),
        "{what}. Nothing was checked. Set BASEMIND_TEST_ALLOW_NO_EMBEDDER=1 to skip this test where no \
         embedding model can be loaded."
    );
    eprintln!("SKIP (BASEMIND_TEST_ALLOW_NO_EMBEDDER): {what}");
}

/// `docs_search` runs two lanes under one filter applied before ranking (ADR-0029): the lexical
/// lane answers without an embedding model, identical content at several paths is one hit naming
/// the others, every hit carries a citation, and a filter no document passes is an empty answer,
/// not a cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(feature = "documents")]
async fn docs_search_filters_before_ranking_and_cites_its_hits() {
    let dir = build_repo();
    let root = dir.path();
    for name in ["one", "two", "three", "four"] {
        std::fs::write(
            root.join(format!("{name}.md")),
            "# Harbour\n\nThe harbour pilots guide ships into the harbour. The harbour tides set the harbour \
             timetable, and the harbour master signs the harbour log.\n",
        )
        .unwrap();
    }
    std::fs::write(
        root.join("airship.md"),
        "# Zeppelin\n\nA zeppelin is an airship. The zeppelin hangar holds one zeppelin, and the zeppelin crew \
         moors the zeppelin at the mast beside the harbour.\n",
    )
    .unwrap();
    let mut cfg = basemind::config::default_for_root(root);
    cfg.documents.enabled = true;
    cfg.documents.keywords.enabled = true;
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
            basemind::scanner::scan(
                root,
                &mut store,
                &cfg,
                basemind::scanner::ScanSource::WorkingTree,
                basemind::scanner::EmbedMode::Inline,
                basemind::retrieval_wiring::backend(),
            )
            .expect("scan");
        });
    });

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");
    let call = |arguments: Value| service.call_tool(call_params("docs_search", arguments));

    let unfiltered = call(json!({ "query": "harbour pilots", "limit": 10 }))
        .await
        .expect("docs_search");
    assert_ne!(unfiltered.is_error, Some(true), "{}", error_text(&unfiltered));
    let body = decode_text(&unfiltered);
    let lanes_ran = body["lanes_ran"].as_array().expect("lanes_ran");
    assert!(
        lanes_ran.contains(&json!("lexical")),
        "the lexical lane needs no model: {body}"
    );
    for unavailable in body["lanes_unavailable"].as_array().expect("lanes_unavailable") {
        assert!(
            unavailable["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "{unavailable}"
        );
    }
    let hits = body["hits"].as_array().expect("hits");
    assert_eq!(hits.len(), 2, "four copies of one content are one hit: {body}");
    let harbour = &hits[0];
    assert_eq!(harbour["path"], "four.md", "the first path in order: {harbour}");
    assert_eq!(harbour["also_at"], json!(["one.md", "three.md", "two.md"]), "{harbour}");
    assert_eq!(harbour["cite"]["line_start"], 1, "{harbour}");
    assert!(
        harbour["cite"]["page_start"].is_null(),
        "a Markdown file has no pages: {harbour}"
    );
    assert!(
        harbour["chunk"]["doc"].as_str().is_some_and(|doc| doc.len() == 32),
        "{harbour}"
    );
    assert!(
        harbour.get("keywords").is_none(),
        "per-hit document metadata is gone: {harbour}"
    );

    let by_path = decode_text(
        &call(json!({ "query": "harbour", "path": "air*.md" }))
            .await
            .expect("docs_search"),
    );
    let paths: Vec<&str> = by_path["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|hit| hit["path"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(paths, ["airship.md"], "the filter runs before the cut: {by_path}");

    for empty in [
        json!({ "query": "harbour", "keywords": "no-document-has-this-keyword" }),
        json!({ "query": "harbour", "format": "pdf" }),
        json!({ "query": "harbour", "mime_type": "application/pdf" }),
    ] {
        let answer = decode_text(&call(empty.clone()).await.expect("docs_search"));
        assert_eq!(answer["hits"], json!([]), "{empty}: {answer}");
        assert!(
            answer.get("truncated").is_none(),
            "an empty filter is not a cut: {answer}"
        );
    }

    let bad_format = call(json!({ "query": "harbour", "format": "slides" }))
        .await
        .expect("docs_search");
    assert_eq!(bad_format.is_error, Some(true));
    assert!(
        error_text(&bad_format).contains("markdown"),
        "{}",
        error_text(&bad_format)
    );
    let scope = call(json!({ "query": "harbour", "scope": "repo" }))
        .await
        .expect("docs_search");
    assert_eq!(scope.is_error, Some(true), "`scope` is gone");

    if lanes_ran.contains(&json!("vector")) {
        assert_eq!(body["vector_mode"], "exact", "{body}");
        assert!(harbour["lanes"]["vector"].as_u64().is_some(), "{harbour}");
    } else {
        skip_without_embedder(&format!("the vector lane did not run: {}", body["lanes_unavailable"]));
    }
    let _ = service.cancel().await;
}

/// MCP types `structuredContent` as an object. A host that validates the result schema rejects any
/// other value, so every answer of every verb tool must be one, including "nothing found" answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_answer_carries_an_object_as_structured_content() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;
    let _ = service
        .call_tool(call_params("memory_save", json!({ "key": "k/one", "value": "v" })))
        .await;
    let calls = [
        ("code_outline", json!({ "path": "a.rs" })),
        ("code_symbols", json!({ "name": "alpha" })),
        ("code_symbols", json!({ "name": "no_such_symbol_anywhere" })),
        ("code_read", json!({ "path": "a.rs", "symbol": "alpha" })),
        ("code_grep", json!({ "pattern": "alpha" })),
        ("code_grep", json!({ "pattern": "no such text anywhere" })),
        ("code_search", json!({ "query": "alpha" })),
        ("code_refs", json!({ "name": "alpha" })),
        (
            "code_refs",
            json!({ "name": "Drawable", "relation": "implementations" }),
        ),
        ("find_files", json!({ "query": "a.rs" })),
        ("docs_search", json!({ "query": "alpha" })),
        ("git_log", json!({})),
        ("git_blame", json!({ "path": "a.rs" })),
        ("git_diff", json!({})),
        ("memory_search", json!({ "key": "k/one" })),
        ("memory_search", json!({ "key": "k/none" })),
        ("memory_search", json!({ "prefix": "k/" })),
        ("memory_search", json!({ "query": "v" })),
        ("index_status", json!({})),
        ("index_rescan", json!({ "paths": ["a.rs"] })),
    ];
    let mut answered = 0;
    for (tool, args) in calls {
        let result = service
            .call_tool(call_params(tool, args.clone()))
            .await
            .unwrap_or_else(|error| panic!("{tool} {args}: {error}"));
        if result.is_error == Some(true) {
            continue;
        }
        answered += 1;
        assert!(
            result.structured_content.as_ref().is_some_and(Value::is_object),
            "{tool} {args}: structuredContent must be an object: {:?}",
            result.structured_content
        );
    }
    assert!(
        answered >= 14,
        "the sweep must reach the tools every build serves: {answered}"
    );
    let _ = service.cancel().await;
}

/// Spawn `basemind serve` against `root` and return the connected rmcp client service.
async fn spawn_serve(root: &Path) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    ().serve(transport).await.expect("rmcp handshake")
}

/// Security regression: `index_rescan` must reject a `paths` entry that escapes the repository root
/// via `..` traversal. It takes raw strings (not `RelPath`), and before the fix
/// `state.root.join("../../etc/passwd")` fed a traversal path into the scanner — which, with
/// `scan.respect_gitignore = false`, read and indexed a file outside the repo. A valid in-repo
/// path must still be accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_rejects_paths_escaping_the_repo_root() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    let escaping = service
        .call_tool(call_params(
            "index_rescan",
            json!({ "paths": ["../../../../../../etc/passwd"] }),
        ))
        .await
        .expect("index_rescan(escaping path)");
    let message = error_text(&escaping);
    assert!(
        message.contains("outside the repository root"),
        "index_rescan must reject a path that escapes the repo root: {message}"
    );

    let ok = service
        .call_tool(call_params("index_rescan", json!({ "paths": ["a.rs"] })))
        .await
        .expect("index_rescan(in-repo path)");
    assert_ne!(
        ok.is_error,
        Some(true),
        "index_rescan must accept a valid in-repo path, got: {ok:?}"
    );

    let _ = service.cancel().await;
}

/// `serve` auto-scans an empty index on boot; `index_status` must report that indexing state separately
/// from query latency. Starting serve on a FRESH (unscanned) repo triggers the boot scan; polling
/// `index_status` must converge to `indexing: false` with an `index_build_ms` recording the build cost.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_auto_scan_reports_index_build_ms_on_status() {
    let dir = build_repo();
    let root = dir.path();
    let service = spawn_serve(root).await;

    // ~keep Poll for up to 30s: on a loaded Windows runner the boot scan (daemon spawn + named-pipe
    // ~keep comms + scan) can take well over the ~10s a 200-iteration budget allowed, which flaked here.
    let mut settled: Option<Value> = None;
    for _ in 0..600 {
        let result = service
            .call_tool(call_params("index_status", json!({})))
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

/// No verb tool advertises an `output_schema`: the response structs nest, schemars emits nested
/// structs as `$ref` into `$defs` (the construct the Anthropic input_schema subset rejects silently
/// and registry-wide, GH #50), and inlining them would put `tools/list` over its byte ceiling.
///
/// This asserts the absence deliberately rather than leaving it untested: a well-meaning "add the
/// output schema back" change would look like an improvement and take the whole registry down.
/// The rest of what every verb-tool definition carries is asserted alongside: a description with a
/// worked example, and annotations that tell a host which two tools write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_tool_advertises_an_output_schema() {
    use basemind::mcp::params::VERB_TOOLS;

    const WRITING_TOOLS: [&str; 2] = ["memory_save", "index_rescan"];

    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let server = spawn_serve(root).await;
    let tools = server.list_all_tools().await.expect("list tools");

    for name in WRITING_TOOLS {
        assert!(
            VERB_TOOLS.contains(&name),
            "`{name}` is expected among the verb tools: {VERB_TOOLS:?}"
        );
    }
    for name in VERB_TOOLS {
        let tool = tools
            .iter()
            .find(|t| t.name.as_ref() == *name)
            .unwrap_or_else(|| panic!("verb tool `{name}` must be advertised"));
        assert!(
            tool.output_schema.is_none(),
            "`{name}` must NOT advertise an output_schema"
        );
        let description = tool.description.as_deref().unwrap_or_default();
        assert!(
            !description.trim().is_empty() && description.contains("Example:"),
            "`{name}` must carry a description with a worked example: {description:?}"
        );
        let annotations = tool
            .annotations
            .as_ref()
            .unwrap_or_else(|| panic!("`{name}` must carry ToolAnnotations"));
        assert_eq!(
            annotations.destructive_hint,
            Some(false),
            "`{name}` destroys nothing and must say so"
        );
        assert_eq!(
            annotations.read_only_hint,
            Some(!WRITING_TOOLS.contains(name)),
            "only {WRITING_TOOLS:?} write; `{name}` advertises the wrong read_only_hint"
        );
        assert_eq!(
            annotations.idempotent_hint,
            Some(true),
            "repeating `{name}` with the same arguments changes nothing further"
        );
        assert_eq!(
            annotations.open_world_hint,
            Some(false),
            "`{name}` reads and writes only this machine's index"
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
        "no tool may advertise an output_schema: {with_schema:?}"
    );

    let _ = server.cancel().await;
}

/// 0.8.0: the server advertises reusable prompt templates and renders them with arguments.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompts_are_listed_and_rendered_with_arguments() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);
    let server = spawn_serve(root).await;

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

    async fn render(
        server: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        name: &'static str,
        arguments: Value,
    ) -> String {
        let mut request = rmcp::model::GetPromptRequestParams::new(name);
        if let Some(arguments) = arguments.as_object().filter(|arguments| !arguments.is_empty()) {
            request = request.with_arguments(arguments.clone());
        }
        let rendered = server
            .get_prompt(request)
            .await
            .unwrap_or_else(|error| panic!("get_prompt {name}: {error}"));
        assert!(
            !rendered.messages.is_empty(),
            "rendered `{name}` must carry at least one message"
        );
        rendered
            .messages
            .iter()
            .filter_map(|m| match &m.content {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<String>()
    }

    // Every tool a prompt routes to must be one the server advertises.
    let advertised = server.list_all_tools().await.expect("list tools");
    let expectations: [(&'static str, Value, &[&str]); 4] = [
        (
            "trace-symbol",
            json!({ "symbol": "Greeter" }),
            &["Greeter", "code_symbols", "code_refs", "git_blame"],
        ),
        (
            "explain-file",
            json!({ "path": "a.rs" }),
            &["a.rs", "code_outline", "code_read"],
        ),
        (
            "onboard-repo",
            json!({}),
            &["index_status", "git_log", "find_files", "code_outline"],
        ),
        ("review-working-tree", json!({}), &["git_diff", "git_blame", "git_log"]),
    ];
    for (name, arguments, mentions) in expectations {
        let body = render(&server, name, arguments).await;
        for mention in mentions {
            assert!(
                body.contains(mention),
                "rendered `{name}` must mention `{mention}`, got: {body}"
            );
        }
        for tool in mentions.iter().filter(|mention| mention.contains('_')) {
            assert!(
                advertised.iter().any(|t| t.name.as_ref() == *tool),
                "`{name}` routes to `{tool}`, which the server does not advertise"
            );
        }
        assert!(
            !body.contains(" mode `"),
            "rendered `{name}` must not route to a removed mode-dispatched tool: {body}"
        );
    }

    let _ = server.cancel().await;
}

/// 0.8.0: the server completes prompt arguments from the indexed code map.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completes_prompt_arguments_from_the_code_map() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);
    let server = spawn_serve(root).await;

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

/// `index_rescan` emits progress notifications when the client supplies a progress token, carries the
/// summary counts in the tool result, and no longer advertises the (SEP-2577 deprecated) logging
/// capability. Uses a capturing client handler to observe the notifications.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_emits_progress_and_no_logging() {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use rmcp::model::{NumberOrString, ProgressNotificationParam};
    use rmcp::service::NotificationContext;
    use rmcp::{ClientHandler, RoleClient};

    #[derive(Clone, Default)]
    struct Capture {
        progress: Arc<StdMutex<Vec<ProgressNotificationParam>>>,
    }

    impl ClientHandler for Capture {
        async fn on_progress(&self, params: ProgressNotificationParam, _context: NotificationContext<RoleClient>) {
            self.progress.lock().unwrap().push(params);
        }
    }

    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let capture = Capture::default();
    let progress = Arc::clone(&capture.progress);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let server = capture.serve(transport).await.expect("rmcp handshake");

    let mut params = call_params("index_rescan", json!({}));
    rmcp::model::RequestParamsMeta::set_progress_token(
        &mut params,
        rmcp::model::ProgressToken(NumberOrString::String("rescan-1".into())),
    );
    let result = server.call_tool(params).await.expect("rescan call");
    assert!(
        format!("{result:?}").contains("scanned"),
        "rescan summary must be in the tool result: {result:?}"
    );
    assert!(
        server
            .peer_info()
            .is_some_and(|info| info.capabilities.logging.is_none()),
        "logging capability must not be advertised"
    );

    // Progress notifications reach `on_progress` on the client's own task, which may run after the
    // call's result is returned: wait for both instead of sleeping. The ceiling only stops a hang.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while progress.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let captured_progress = progress.lock().unwrap().clone();

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

/// The timing contract: a tool whose answer replaces reading content reports its own server-side
/// handler latency in microseconds. A locating tool leaves it out: its answer is a few rows, and the
/// field cost as much as one (telemetry records the latency of every call).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn latency_tools_report_microsecond_elapsed_us() {
    let (_dir, service) = spawn_paging_server().await;

    let search = decode_text(
        &service
            .call_tool(call_params("code_symbols", json!({ "name": "paged" })))
            .await
            .expect("code_symbols"),
    );
    assert!(
        !symbol_rows(&search).is_empty(),
        "fixture should match the `paged` symbol: {search}"
    );
    assert!(search.get("elapsed_us").is_none(), "a locating tool: {search}");

    let recent = decode_text(
        &service
            .call_tool(call_params("git_log", json!({ "limit": 2 })))
            .await
            .expect("git_log"),
    );
    assert_eq!(commit_shas(&recent).len(), 2, "fixture has 5 commits; asked for 2");
    assert!(recent.get("elapsed_us").is_none(), "a locating tool: {recent}");

    let outline = decode_text(
        &service
            .call_tool(call_params("code_outline", json!({ "path": "paged.rs" })))
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

/// A client built against an earlier response shape still reads what it knows of the lean one: no
/// response struct sets `deny_unknown_fields`, so serde skips keys the old client doesn't know, and
/// the fields it reads are still there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn older_clients_still_read_the_lean_responses() {
    /// The part of the `code_symbols` response an older client knew that the lean rows kept: a
    /// `results` list it walks generically, and `truncated`, left out when false.
    #[derive(serde::Deserialize)]
    struct LegacySearchResponse {
        // Left out when false since the verb tools (docs/UPGRADING.md, Response fields).
        #[serde(default)]
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
            .call_tool(call_params("code_symbols", json!({ "name": "paged" })))
            .await
            .expect("code_symbols"),
    );
    let legacy: LegacySearchResponse =
        serde_json::from_value(search).expect("an older client must still deserialize the response");
    assert!(!legacy.truncated, "old client still reads `truncated`");
    assert!(!legacy.results.is_empty(), "old client still reads `results`");

    let recent = decode_text(
        &service
            .call_tool(call_params("git_log", json!({ "limit": 2 })))
            .await
            .expect("git_log"),
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

/// Regression: `code_grep` must scan the whole indexed corpus, not the first
/// `limit * 8` files.
///
/// The old bound was a files-VISITED cap, which is the wrong bound for a linear content scan:
/// a default `limit = 100` grep visited 2000 files of a 68 k-file monorepo (2.9 %, in path order)
/// and returned a fast, confident, wrong zero for any token that lived past the cut. `limit` caps
/// HITS; the corpus is always fully scanned (subject to the `language` / `path` filters).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_grep_finds_a_rare_token_past_the_old_scan_cap() {
    const RARE: &str = "OptimizationStatusZQX";
    let dir = build_wide_repo(2_100, RARE);
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    let body = decode_text(
        &service
            .call_tool(call_params("code_grep", json!({ "pattern": RARE })))
            .await
            .expect("code_grep"),
    );

    let hits = hit_rows(&body);
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
    assert!(
        body.get("total").is_none() && body.get("next_cursor").is_none(),
        "a full-corpus scan lists every match on one page: {body}"
    );
    assert!(
        body.get("truncated").is_none(),
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
    let service = spawn_serve(root).await;

    let whole = decode_text(
        &service
            .call_tool(call_params(
                "code_grep",
                json!({ "pattern": "pub fn", "limit": 1000, "max_tokens": 100000 }),
            ))
            .await
            .expect("code_grep(unpaged)"),
    );
    assert!(
        whole.get("truncated").is_none(),
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
        let mut args = json!({ "pattern": "pub fn", "limit": 1 });
        if let Some(c) = &cursor {
            args["cursor"] = Value::String(c.clone());
        }
        let page = decode_text(
            &service
                .call_tool(call_params("code_grep", args))
                .await
                .expect("code_grep (page)"),
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

/// `(path, line)` of every hit in a grep response, in response order.
fn grep_keys(body: &Value) -> Vec<(String, u64)> {
    hit_rows(body)
        .iter()
        .map(|h| {
            (
                h.get("path").and_then(Value::as_str).unwrap_or_default().to_string(),
                h.get("line").and_then(Value::as_u64).unwrap_or_default(),
            )
        })
        .collect()
}

/// **P0 contract, at the MCP surface.** `code_refs` pinned to a definition by `path` must never
/// present a resolution-limited subset as the complete caller set.
///
/// The fixture is the shape that broke in production: `pkg/mod.py` defines `target` and calls it
/// itself (so intra-file resolution "succeeds"), while two other files reach it through
/// `from pkg import mod` + `mod.target()` — a module-object import, which binds a module rather than
/// the function, so the cross-file join has no export to bind and those callers are invisible to
/// resolution. The pinned lookup used to return only the resolvable subset with no truncation flag; an
/// agent asking "what calls this?" before a refactor got a confident, precise-looking, wrong answer.
///
/// The name scan is the sound floor, so `code_refs` with and without `path` must AGREE on the count
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
            .call_tool(call_params("code_refs", json!({ "name": "target", "limit": 500 })))
            .await
            .expect("code_refs"),
    );
    let reference_total = references.get("total").and_then(Value::as_u64).expect("total");
    assert_eq!(
        reference_total, 4,
        "ground truth: 4 target() call sites across the fixture: {references}"
    );

    let callers = decode_text(
        &service
            .call_tool(call_params(
                "code_refs",
                json!({ "path": "pkg/mod.py", "name": "target", "limit": 500 }),
            ))
            .await
            .expect("code_refs with `path`"),
    );
    assert_eq!(
        callers.get("total").and_then(Value::as_u64),
        Some(reference_total),
        "code_refs with `path` must agree with the unpinned name scan — never a subset: {callers}"
    );
    let hits = hit_rows(&callers);
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

/// Call a verb tool with arguments it must refuse and return the refusal an agent reads.
async fn refused(
    service: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tool: &'static str,
    args: Value,
) -> String {
    let result = service
        .call_tool(call_params(tool, args.clone()))
        .await
        .unwrap_or_else(|error| panic!("`{tool}` {args} must answer with a tool result, got {error}"));
    error_text(&result)
}

/// The argument contract of the code and index verb tools: an argument the tool does not take is
/// refused naming the field it most likely meant and the accepted set, a missing required field is
/// named, and an argument shape with no meaning is refused naming the shapes that have one. Every
/// refusal is an `isError` tool result, never a JSON-RPC error and never a call that ran with the
/// stray argument ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_and_index_tools_reject_inexact_arguments() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let tools = service.list_all_tools().await.expect("list tools");
    for &removed in basemind::mcp::params::RETIRED_TOOLS {
        assert!(
            tools.iter().all(|t| t.name.as_ref() != removed),
            "the mode-dispatched `{removed}` tool must no longer be advertised"
        );
    }

    let message = refused(&service, "code_grep", json!({ "pattern": "x", "glob": "*.rs" })).await;
    assert!(
        message.contains("`code_grep`: unknown field `glob`, did you mean `path`?"),
        "an unknown field must be named with the field it most likely meant: {message}"
    );
    assert!(
        message.contains("Accepted fields:") && message.contains("`pattern` (required)") && message.contains("`regex`"),
        "an unknown field must list the accepted set: {message}"
    );

    let message = refused(&service, "code_grep", json!({ "mode": "grep", "pattern": "x" })).await;
    assert!(
        message.contains("unknown field `mode`") && message.contains("each operation is its own tool"),
        "a leftover `mode` must explain the new surface: {message}"
    );

    let message = refused(&service, "code_grep", json!({})).await;
    assert!(
        message.contains("missing field `pattern`") && message.contains("`pattern` (required)"),
        "a missing required field must be named: {message}"
    );

    let message = refused(&service, "code_read", json!({ "path": "a.rs" })).await;
    assert!(
        message.contains("neither `symbol` nor `line` was given") && message.contains("pass {path, symbol}"),
        "code_read with neither selector must name the accepted shapes: {message}"
    );
    let message = refused(
        &service,
        "code_read",
        json!({ "path": "a.rs", "symbol": "alpha", "end_line": 2 }),
    )
    .await;
    assert!(
        message.contains("`end_line` applies only to a line span") && message.contains("pass {path, symbol}"),
        "code_read with a symbol and a span end must name the accepted shapes: {message}"
    );
    for (tool, args) in [
        ("code_grep", json!({ "pattern": "alpha", "ranges": true })),
        ("code_read", json!({ "path": "a.rs", "ranges": "1-2" })),
        ("code_refs", json!({ "name": "alpha", "ranges": true })),
    ] {
        let message = refused(&service, tool, args.clone()).await;
        assert!(
            message.contains(&format!("`{tool}`: unknown field `ranges`")) && message.contains("Accepted fields:"),
            "`ranges` is no longer an argument and must be refused as unknown: {args} -> {message}"
        );
    }

    let message = refused(&service, "code_refs", json!({ "name": "alpha", "line": 3 })).await;
    assert!(
        message.contains("`line` needs `path`") && message.contains("accepted shapes:"),
        "code_refs must refuse a line with no file to look in: {message}"
    );
    let message = refused(&service, "code_refs", json!({ "name": "alpha", "column": 3 })).await;
    assert!(
        message.contains("`column` applies only to relation `definition`") && message.contains("accepted shapes:"),
        "code_refs must refuse a column outside relation `definition`: {message}"
    );
    let message = refused(&service, "code_refs", json!({ "name": "alpha", "relation": "users" })).await;
    assert!(
        message.contains("`relation` must be one of") && message.contains("`importers`") && message.contains("users"),
        "an unknown relation must list the accepted ones: {message}"
    );
    let message = refused(
        &service,
        "code_refs",
        json!({ "name": "alpha", "resolution": "same_file,proven" }),
    )
    .await;
    assert!(
        message.contains("unknown resolution `proven`") && message.contains("scope_resolved"),
        "an unknown resolution must name the value and list the accepted ones: {message}"
    );
    let message = refused(
        &service,
        "code_refs",
        json!({ "name": "Drawable", "relation": "implementations", "resolution": "imported" }),
    )
    .await;
    assert!(
        message.contains("`resolution` filters call sites") && message.contains("accepted shapes:"),
        "the filter applies to call sites only: {message}"
    );

    let args = json!({ "name": "alpha", "relation": "importers", "changed": "HEAD" });
    let message = refused(&service, "code_refs", args.clone()).await;
    assert!(
        message.contains("`changed` applies to call sites") && message.contains("accepted shapes:"),
        "{args}: {message}"
    );

    let message = refused(&service, "index_status", json!({ "mode": "status" })).await;
    assert!(
        message.contains("`index_status`: unknown field `mode`") && message.contains("Accepted fields: none"),
        "index_status takes no arguments and must say so: {message}"
    );
    let message = refused(&service, "index_rescan", json!({ "component": "blobs" })).await;
    assert!(
        message.contains("unknown field `component`") && message.contains("`paths`") && !message.contains("`full`"),
        "index_rescan must refuse a field it does not take and list the one it does: {message}"
    );

    let _ = service.cancel().await;
}

/// The argument contract of `memory_save` and `memory_search`, mirroring the code and git coverage.
///
/// Argument exactness is checked before the feature gate, so the unknown-field and missing-field
/// refusals hold in every build; the selector conflict is decided in the body, which a build
/// without `--features memory` replaces with the missing-feature answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_tools_reject_inexact_arguments() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let tools = service.list_all_tools().await.expect("list tools");
    assert!(
        tools.iter().all(|t| t.name.as_ref() != "memory"),
        "the mode-dispatched `memory` tool must no longer be advertised"
    );
    for tool in ["memory_save", "memory_search", "docs_search"] {
        assert!(
            tools.iter().any(|t| t.name.as_ref() == tool),
            "`{tool}` must be advertised in every build"
        );
    }

    let message = refused(&service, "memory_search", json!({ "q": "x" })).await;
    assert!(
        message.contains("`memory_search`: unknown field `q`, did you mean `query`?")
            && message.contains("Accepted fields:")
            && message.contains("`prefix`"),
        "an unknown field must be named with the field it most likely meant: {message}"
    );

    let message = refused(&service, "memory_save", json!({ "key": "k" })).await;
    assert!(
        message.contains("missing field `value`") && message.contains("`value` (required)"),
        "memory_save must name the missing `value`: {message}"
    );

    let message = refused(
        &service,
        "memory_save",
        json!({ "key": "k", "value": "v", "embed": false }),
    )
    .await;
    assert!(
        message.contains("unknown field `embed`"),
        "the removed `embed` switch must be refused, not ignored: {message}"
    );

    let message = refused(&service, "memory_search", json!({ "query": "x", "key": "k" })).await;
    if cfg!(feature = "memory") {
        assert!(
            message.contains("`query`, `key` and `prefix` are alternatives") && message.contains("pass at most one of"),
            "memory_search with two selectors must name the accepted shapes: {message}"
        );
    } else {
        assert!(
            message.contains("`memory_search` needs the `memory` Cargo feature"),
            "without --features memory, memory_search must name the missing feature: {message}"
        );
    }

    let _ = service.cancel().await;
}

/// The argument contract of the git verb tools, mirroring the code and memory coverage: unknown
/// and missing fields are named, and every combination of filters with no meaning is refused
/// naming the shapes that have one. A filter that was ignored would read as a log scoped by it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_tools_reject_inexact_arguments() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let tools = service.list_all_tools().await.expect("list tools");
    assert!(
        tools.iter().all(|t| t.name.as_ref() != "git"),
        "the mode-dispatched `git` tool must no longer be advertised"
    );

    let message = refused(&service, "git_blame", json!({ "path": "a.rs", "line_start": 1 })).await;
    assert!(
        message.contains("`git_blame`: unknown field `line_start`, did you mean `line`?")
            && message.contains("Accepted fields:")
            && message.contains("`path` (required)"),
        "an unknown field must be named with the field it most likely meant: {message}"
    );

    let message = refused(&service, "git_blame", json!({})).await;
    assert!(
        message.contains("missing field `path`") && message.contains("`path` (required)"),
        "git_blame must name the missing `path`: {message}"
    );

    let message = refused(&service, "git_blame", json!({ "path": "a.rs", "end_line": 3 })).await;
    assert!(
        message.contains("`end_line` needs `line`") && message.contains("accepted shapes:"),
        "git_blame must refuse a span with no start: {message}"
    );
    let message = refused(
        &service,
        "git_blame",
        json!({ "path": "a.rs", "symbol": "alpha", "end_line": 3 }),
    )
    .await;
    assert!(
        message.contains("`end_line` applies only to a line span") && message.contains("accepted shapes:"),
        "git_blame must refuse a symbol combined with a span: {message}"
    );

    let message = refused(&service, "git_log", json!({ "rev": "HEAD", "path": "a.rs" })).await;
    assert!(
        message.contains("`git_log`: `rev` shows one commit and combines with no other filter")
            && message.contains("accepted shapes:"),
        "git_log must refuse `rev` combined with another filter: {message}"
    );

    let message = refused(&service, "git_log", json!({ "symbol": "alpha" })).await;
    assert!(
        message.contains("`symbol` needs the `path` of the file defining it") && message.contains("accepted shapes:"),
        "git_log must refuse a symbol history with no file: {message}"
    );

    let message = refused(&service, "git_log", json!({ "query": "tweak", "path": "a.rs" })).await;
    assert!(
        message.contains("do not combine with `path`") && message.contains("accepted shapes:"),
        "git_log must refuse a history search scoped to a path: {message}"
    );

    let message = refused(&service, "git_diff", json!({ "from": "HEAD" })).await;
    assert!(
        message.contains("`git_diff`: a diff needs the `path` of one file") && message.contains("accepted shapes:"),
        "git_diff must refuse revisions with no file: {message}"
    );

    let message = refused(&service, "git_diff", json!({ "path": "a.rs", "rev_old": "HEAD~1" })).await;
    assert!(
        message.contains("unknown field `rev_old`, did you mean `from`?"),
        "the old revision spelling must be refused with its replacement: {message}"
    );

    let _ = service.cancel().await;
}

/// The stdio relay re-sends a request in flight when the daemon is replaced, but only for a tool
/// someone decided is safe to run twice. A tool added to the server without that decision is
/// failed on every restart if it reads and must never be listed by default if it writes, so
/// advertising one fails here until it is put in one of the lists in `basemind::mcp::replay`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_advertised_tool_has_a_replay_decision() {
    use basemind::mcp::replay::{KEYED_REPLAY_TOOLS, NEVER_REPLAY_TOOLS, REPLAY_SAFE_TOOLS, decisions};

    let dir = build_repo();
    let root = dir.path();
    run_scan(root);
    let server = spawn_serve(root).await;
    let tools = server.list_all_tools().await.expect("list tools");
    assert!(tools.len() >= REPLAY_SAFE_TOOLS.len(), "the server lists its tools");

    for tool in &tools {
        assert_eq!(
            decisions(&tool.name),
            1,
            "`{}` is advertised and must be in exactly one of REPLAY_SAFE_TOOLS, NEVER_REPLAY_TOOLS and \
             KEYED_REPLAY_TOOLS (src/mcp/replay.rs): decide whether the relay may send it twice",
            tool.name
        );
    }
    let advertised = |name: &&str| tools.iter().any(|tool| tool.name == **name);
    for listed in REPLAY_SAFE_TOOLS.iter().chain(NEVER_REPLAY_TOOLS) {
        assert!(advertised(listed), "`{listed}` has a replay decision and is not a tool");
    }
    if cfg!(feature = "comms") {
        for listed in KEYED_REPLAY_TOOLS {
            assert!(advertised(listed), "`{listed}` has a replay decision and is not a tool");
        }
    }
    server.cancel().await.expect("shut down");
}

/// #127: `find_files` with `min_lines` lists the code files at least that long, largest first, each
/// with its line and byte count, and refuses a threshold of 0; `index_status` counts the files too
/// long to read whole and names the largest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_files_are_listed_by_size_and_counted_on_status() {
    let dir = build_repo();
    let root = dir.path();
    let big: String = (0..2100).map(|i| format!("pub fn f{i}() {{}}\n")).collect();
    let mid: String = (0..300).map(|i| format!("pub fn g{i}() {{}}\n")).collect();
    std::fs::write(root.join("big.rs"), &big).unwrap();
    std::fs::write(root.join("mid.rs"), &mid).unwrap();
    run_scan(root);
    let service = spawn_serve(root).await;

    let body = decode_text(
        &service
            .call_tool(call_params("find_files", json!({ "min_lines": 300 })))
            .await
            .expect("find_files min_lines"),
    );
    assert_eq!(body["files"], json!({ "big.rs": 2100, "mid.rs": 300 }), "{body}");
    assert!(body.get("total").is_none(), "no `next_cursor`, so no count: {body}");

    let body = decode_text(
        &service
            .call_tool(call_params("index_status", json!({})))
            .await
            .expect("index_status"),
    );
    assert_eq!(
        body["large_files"],
        json!({
            "threshold_lines": 2000,
            "count": 1,
            "largest": [{ "path": "big.rs", "lines": 2100, "bytes": big.len() }],
        }),
        "{body}"
    );

    let message = refused(&service, "find_files", json!({ "min_lines": 0 })).await;
    assert_eq!(
        message,
        "`find_files`: `min_lines` must be at least 1; omit it to list files of any size."
    );
    let message = refused(&service, "find_files", json!({ "min_lines": "many" })).await;
    assert!(
        message.contains("`min_lines`"),
        "a wrongly typed `min_lines` must be named: {message}"
    );
    service.cancel().await.expect("shut down");
}

/// `code_refs` over the import graph (#121, #135): `importers` with a `path` and `cycles` take no
/// `name`, every other relation still needs one, and each argument that has no meaning in a shape
/// is refused naming the shapes that have one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_graph_relations_answer_and_refuse_their_shapes() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);
    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    for (args, fields) in [
        (
            json!({ "relation": "importers", "path": "a.rs", "depth": 2 }),
            &["by_depth"][..],
        ),
        (json!({ "relation": "cycles" }), &["total", "cycles"][..]),
        (
            json!({ "relation": "cycles", "path": "cyc1.rs" }),
            &["total", "cycles"][..],
        ),
    ] {
        let result = service
            .call_tool(call_params("code_refs", args.clone()))
            .await
            .expect("code_refs over the graph");
        assert_ne!(result.is_error, Some(true), "{args}: {result:?}");
        let body = decode_text(&result);
        for field in fields {
            assert!(body.get(field).is_some(), "{args} must answer `{field}`: {body}");
        }
    }

    let refusals = [
        (json!({}), "relation `callers` needs `name`"),
        (
            json!({ "relation": "implementations" }),
            "relation `implementations` needs `name`",
        ),
        (
            json!({ "name": "alpha", "depth": 2 }),
            "`depth` applies only to relation `importers` with `path`",
        ),
        (
            json!({ "name": "alpha", "relation": "importers", "depth": 2 }),
            "`depth` applies only to relation `importers` with `path`",
        ),
        (
            json!({ "name": "alpha", "relation": "importers", "path": "a.rs" }),
            "not both",
        ),
        (
            json!({ "relation": "importers", "path": "a.rs", "line": 1 }),
            "`line` needs `path`",
        ),
        (
            json!({ "name": "alpha", "relation": "cycles" }),
            "relation `cycles` takes no `name` or `match`",
        ),
    ];
    for (args, expected) in refusals {
        let message = refused(&service, "code_refs", args.clone()).await;
        assert!(
            message.contains(expected) && message.contains("{path, relation:\"importers\", depth?}"),
            "{args} must be refused with `{expected}` and the accepted shapes: {message}"
        );
    }
    for depth in [0, 6] {
        let message = refused(
            &service,
            "code_refs",
            json!({ "relation": "importers", "path": "a.rs", "depth": depth }),
        )
        .await;
        assert!(message.contains("`depth` is 1 to 5"), "depth {depth}: {message}");
    }
    let message = refused(
        &service,
        "code_refs",
        json!({ "relation": "importers", "path": "missing.rs" }),
    )
    .await;
    assert!(
        message.contains("missing.rs"),
        "an unindexed file must be named: {message}"
    );
    service.cancel().await.expect("shut down");
}
/// The files of a `code_grep` answer, sorted.
fn grep_files(body: &Value) -> Vec<String> {
    let mut files: Vec<String> = hit_rows(body)
        .iter()
        .map(|row| row["path"].as_str().expect("a path").to_string())
        .collect();
    files.sort();
    files.dedup();
    files
}

/// #122: `changed` scopes `code_grep`, `code_symbols` and `find_files` to the files a change touched
/// (a revision's merge base with `HEAD` against the working tree, `A..B` or `A...B`), and says how
/// many files the scope holds, so a change that touched nothing (`scope_files: 0`) is told apart from
/// one without a hit. An unknown revision is refused naming it. `code_search` advertises `changed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_scopes_queries_to_the_files_a_change_touched() {
    let dir = build_repo();
    let root = dir.path();
    // `HEAD~1..HEAD` touched `a.rs`; the working tree changes `c.rs` and adds `f.rs`. `alpha` is
    // called from `c.rs` too, and `zed` is defined in `e.rs` too: both lie outside each scope.
    std::fs::write(root.join("c.rs"), b"pub fn zed() {}\npub fn caller() { alpha(); }\n").unwrap();
    std::fs::write(root.join("f.rs"), b"pub fn fresh() { alpha(); }\n").unwrap();
    run_scan(root);
    let service = spawn_serve(root).await;
    let call = |tool: &'static str, args: Value| {
        let service = &service;
        async move {
            let result = service
                .call_tool(call_params(tool, args.clone()))
                .await
                .unwrap_or_else(|error| panic!("{tool} {args}: {error}"));
            assert_ne!(result.is_error, Some(true), "{tool} {args}: {result:?}");
            decode_text(&result)
        }
    };

    let body = call("code_grep", json!({ "pattern": "alpha", "changed": "HEAD" })).await;
    assert_eq!(grep_files(&body), ["c.rs", "f.rs"], "{body}");
    assert_eq!(body["scope_files"], 2, "{body}");
    let body = call("code_grep", json!({ "pattern": "alpha", "changed": "HEAD~1..HEAD" })).await;
    assert_eq!(grep_files(&body), ["a.rs"], "{body}");
    assert_eq!(body["scope_files"], 1, "{body}");
    let body = call("code_grep", json!({ "pattern": "alpha" })).await;
    assert!(
        body.get("scope_files").is_none(),
        "no `changed`, no `scope_files`: {body}"
    );

    let body = call("code_refs", json!({ "name": "alpha", "changed": "HEAD" })).await;
    assert_eq!(grep_files(&body), ["c.rs", "f.rs"], "{body}");
    assert_eq!(
        (body["total"].clone(), body["scope_files"].clone()),
        (json!(2), json!(2)),
        "{body}"
    );
    let body = call(
        "code_refs",
        json!({ "name": "alpha", "path": "a.rs", "changed": "HEAD~1..HEAD" }),
    )
    .await;
    assert_eq!(
        (body["total"].clone(), body["scope_files"].clone()),
        (json!(0), json!(1)),
        "the change touched only the definition's file, which calls nothing: {body}"
    );
    let body = call("code_refs", json!({ "name": "alpha", "path": "a.rs" })).await;
    assert!(body.get("scope_files").is_none(), "{body}");
    assert!(
        grep_files(&body).contains(&"c.rs".to_string()),
        "unscoped callers still list every file: {body}"
    );

    let body = call("code_symbols", json!({ "name": "zed", "changed": "HEAD" })).await;
    let paths: Vec<String> = symbol_rows(&body).into_iter().map(|(path, _)| path).collect();
    assert_eq!(paths, ["c.rs"], "{body}");
    assert_eq!(body["scope_files"], 2, "{body}");

    let body = call("find_files", json!({ "changed": "HEAD" })).await;
    assert_eq!(body["files"], json!(["c.rs", "f.rs"]), "{body}");
    assert_eq!(body["scope_files"], 2, "{body}");
    let body = call("find_files", json!({ "query": "rs", "changed": "HEAD~1..HEAD" })).await;
    assert_eq!(body["files"], json!(["a.rs"]), "{body}");
    let body = call("find_files", json!({ "min_lines": 1, "changed": "HEAD~1.." })).await;
    assert_eq!(
        body["files"]
            .as_object()
            .map(|files| files.keys().cloned().collect::<Vec<_>>()),
        Some(vec!["a.rs".to_string()]),
        "{body}"
    );
    assert!(body.get("total").is_none(), "no `next_cursor`, so no count: {body}");

    // A range that changed nothing is an empty scope, not an error.
    let body = call("code_grep", json!({ "pattern": "alpha", "changed": "HEAD..HEAD" })).await;
    assert_eq!(
        (body["hits"].clone(), body["scope_files"].clone()),
        (json!({}), json!(0)),
        "{body}"
    );
    let body = call("find_files", json!({ "changed": "HEAD..HEAD" })).await;
    assert_eq!(body["files"], json!([]), "{body}");
    assert_eq!(body["scope_files"], 0, "{body}");

    let message = refused(
        &service,
        "code_grep",
        json!({ "pattern": "alpha", "changed": "no-such-rev" }),
    )
    .await;
    assert!(
        message.starts_with("`code_grep`: `changed`: `no-such-rev` does not name a revision"),
        "an unknown revision must be refused naming it: {message}"
    );
    let body = call("code_grep", json!({ "pattern": "alpha", "changed": "HEAD~1...HEAD" })).await;
    assert_eq!(grep_files(&body), ["a.rs"], "{body}");
    assert_eq!(body["scope_files"], 1, "{body}");
    let body = call("code_grep", json!({ "pattern": "alpha", "changed": "HEAD...HEAD~1" })).await;
    assert_eq!(
        (body["hits"].clone(), body["scope_files"].clone()),
        (json!({}), json!(0)),
        "`HEAD~1` is the merge base, so its side changed nothing: {body}"
    );
    let message = refused(&service, "code_symbols", json!({ "name": "zed", "changed": "..." })).await;
    assert!(
        message.contains("names no revision") && message.contains("`A...B`"),
        "a bare `...` must be refused with the accepted forms: {message}"
    );

    let tools = service.list_all_tools().await.expect("list tools");
    let code_search = tools
        .iter()
        .find(|tool| tool.name.as_ref() == "code_search")
        .expect("code_search is advertised");
    assert!(
        code_search
            .input_schema
            .get("properties")
            .and_then(|properties| properties.get("changed"))
            .and_then(|changed| changed.get("description"))
            .and_then(Value::as_str)
            .is_some_and(|doc| doc.contains("A...B")),
        "code_search advertises `changed`: {:?}",
        code_search.input_schema
    );
    service.cancel().await.expect("shut down");
}

/// #122: `+importers` adds the files importing a changed file directly; a file importing nothing
/// changed stays out.
#[cfg(feature = "code-intel-js")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_with_importers_adds_the_files_importing_the_change() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("lib.ts"), b"export function shared() { return 1; }\n").unwrap();
    std::fs::write(root.join("app.ts"), b"import { shared } from './lib';\nshared();\n").unwrap();
    std::fs::write(root.join("deep.ts"), b"import './app';\n").unwrap();
    std::fs::write(root.join("alone.ts"), b"export const alone = 1;\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    std::fs::write(root.join("lib.ts"), b"export function shared() { return 2; }\n").unwrap();
    run_scan(root);
    let service = spawn_serve(root).await;

    let result = service
        .call_tool(call_params("find_files", json!({ "changed": "HEAD+importers" })))
        .await
        .expect("find_files");
    let body = decode_text(&result);
    assert_ne!(result.is_error, Some(true), "{body}");
    assert_eq!(body["files"], json!(["app.ts", "lib.ts"]), "depth 1 only: {body}");
    assert_eq!(body["scope_files"], 2, "{body}");
    service.cancel().await.expect("shut down");
}

/// #122: without an import resolver in the build, `+importers` is refused rather than answered as if
/// nothing imported the change.
#[cfg(not(any(feature = "code-intel-js", feature = "code-intel-stack")))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_with_importers_is_refused_without_a_resolver() {
    let dir = build_repo();
    let root = dir.path();
    std::fs::write(root.join("f.rs"), b"pub fn fresh() {}\n").unwrap();
    run_scan(root);
    let service = spawn_serve(root).await;
    let message = refused(&service, "find_files", json!({ "changed": "HEAD+importers" })).await;
    assert!(message.contains("`+importers` needs import resolution"), "{message}");
    service.cancel().await.expect("shut down");
}

/// #126: HTTP calls with literal URLs are linked to the routes they reach by method and normalized
/// path (ADR-0048). A route's callers are graded `path_match`, `ambiguous` when two services expose
/// the path; a client call's `definition` is its route, or the tied routes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_links_http_calls_to_routes_by_path() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(
        root.join("server.rs"),
        b"pub fn app() -> Router {\n    Router::new()\n        .route(\"/users/{id}\", get(show_user))\n        \
          .route(\"/orders\", post(create_order))\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("billing.rs"),
        b"pub fn app() -> Router {\n    Router::new().route(\"/orders\", post(bill))\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("client.rs"),
        b"pub async fn call(client: &reqwest::Client) {\n    \
          reqwest::get(\"https://api.example.com/users/42\").await;\n    \
          client.post(\"https://api.example.com/api/orders\").send().await;\n}\n",
    )
    .unwrap();
    run_scan(root);
    let service = spawn_serve(root).await;
    let call = |args: Value| {
        let service = &service;
        async move {
            let result = service
                .call_tool(call_params("code_refs", args.clone()))
                .await
                .unwrap_or_else(|error| panic!("{args}: {error}"));
            assert_ne!(result.is_error, Some(true), "{args}: {result:?}");
            decode_text(&result)
        }
    };

    let users = call(json!({ "name": "GET /users/{id}" })).await;
    assert_eq!(users["resolution_default"], "path_match", "{users}");
    assert_eq!(
        users["hits"],
        json!({ "client.rs": [{ "line": 2, "callee": "GET /users/42" }] }),
        "{users}"
    );
    let orders = call(json!({ "name": "POST /orders", "path": "server.rs", "line": 4 })).await;
    assert_eq!(orders["definition"]["kind"], "route", "{orders}");
    assert_eq!(
        orders["hits"],
        json!({ "client.rs": [{ "line": 3, "callee": "POST /api/orders", "ambiguous": true }] }),
        "a suffix match that two services' routes tie on: {orders}"
    );

    let definition = call(json!({
        "name": "GET /users/42", "relation": "definition", "path": "client.rs", "line": 2
    }))
    .await;
    assert_eq!(
        (
            definition["resolution"].clone(),
            definition["definition"]["path"].clone(),
            definition["definition"]["line"].clone(),
            definition["definition"]["name"].clone(),
        ),
        (
            json!("path_match"),
            json!("server.rs"),
            json!(3),
            json!("GET /users/:param")
        ),
        "{definition}"
    );
    let tied = call(json!({
        "name": "POST /api/orders", "relation": "definition", "path": "client.rs", "line": 3
    }))
    .await;
    assert!(tied.get("definition").is_none(), "{tied}");
    let tied_paths: Vec<&str> = tied["ambiguous"]
        .as_array()
        .expect("the tied routes")
        .iter()
        .map(|route| route["path"].as_str().expect("a path"))
        .collect();
    assert_eq!(tied_paths, ["billing.rs", "server.rs"], "{tied}");

    let message = refused(
        &service,
        "code_refs",
        json!({ "name": "GET /users/7", "relation": "definition", "path": "client.rs", "line": 2 }),
    )
    .await;
    assert!(
        message.contains("is `GET /users/42`, not `GET /users/7`"),
        "the call on the line is named: {message}"
    );
    let message = refused(
        &service,
        "code_refs",
        json!({ "name": "GET /users/42", "relation": "definition", "path": "client.rs", "line": 1 }),
    )
    .await;
    assert!(
        message.contains("no HTTP call with a literal URL on line 1"),
        "{message}"
    );
    let message = refused(
        &service,
        "code_refs",
        json!({ "name": "GET /users/42", "relation": "definition", "path": "client.rs", "line": 2, "column": 5 }),
    )
    .await;
    assert!(message.contains("drop `column`"), "{message}");
    service.cancel().await.expect("shut down");
}
