//! `code_read` answers byte for byte: a symbol's body, a line span, an edited file's fresh parse,
//! and every refusal's exact words.
//!
//! The tool's body moves from the MCP adapter into the engine; these pin what an agent reads so the
//! move (and any later one) cannot change a response or an error text unnoticed.

use serde_json::{Value, json};

use super::{Client, call_err, call_raw, started, text_of};

/// The tool's JSON text; it must not be an error.
async fn answer(client: &Client, arguments: Value) -> String {
    let result = call_raw(client, "code_read", arguments.clone()).await;
    let text = text_of(&result);
    assert_ne!(result.is_error, Some(true), "code_read {arguments} failed: {text}");
    text
}

/// Compare every `(label, actual, expected)` at once, so one run shows each drifted answer.
fn assert_all(rows: &[(String, String, String)]) {
    let drifted: Vec<String> = rows
        .iter()
        .filter(|(_, actual, expected)| actual != expected)
        .map(|(label, actual, expected)| format!("{label}\n  actual:   {actual}\n  expected: {expected}"))
        .collect();
    assert!(
        drifted.is_empty(),
        "{} answers drifted:\n{}",
        drifted.len(),
        drifted.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_read_answers_byte_for_byte() {
    let (dir, client) = started().await;
    let cases = [
        (
            json!({ "path": "src/outline.rs", "symbol": "render" }),
            r#"{"path":"src/outline.rs","name":"render","kind":"function","line":18,"end_line":20,"body":"pub fn render(&self) -> u32 {\n        file_outline() + file_outline_l2()\n    }","bytes":78}"#,
        ),
        (
            json!({ "path": "src/outline.rs", "symbol": "Renderer", "kind": "struct" }),
            r#"{"path":"src/outline.rs","name":"Renderer","kind":"struct","line":15,"end_line":15,"body":"pub struct Renderer;","bytes":20}"#,
        ),
        (
            json!({ "path": "src/outline.rs", "symbol": "file_outline", "line": 4 }),
            r#"{"path":"src/outline.rs","name":"file_outline","kind":"function","line":3,"end_line":5,"body":"pub fn file_outline() -> u32 {\n    1\n}","bytes":38}"#,
        ),
        (
            json!({ "path": "src/outline.rs", "line": 3, "end_line": 5 }),
            r#"{"path":"src/outline.rs","line":3,"end_line":5,"total_lines":21,"body":"pub fn file_outline() -> u32 {\n    1\n}","bytes":38}"#,
        ),
        (
            json!({ "path": "src/outline.rs", "line": 21 }),
            r#"{"path":"src/outline.rs","line":21,"end_line":21,"total_lines":21,"body":"}","bytes":1}"#,
        ),
        (
            json!({ "path": "src/outline.rs", "line": 20, "end_line": 99 }),
            r#"{"path":"src/outline.rs","line":20,"end_line":21,"total_lines":21,"body":"    }\n}","bytes":7}"#,
        ),
    ];
    let mut rows = Vec::new();
    for (arguments, expected) in cases {
        let actual = answer(&client, arguments.clone()).await;
        rows.push((format!("code_read {arguments}"), actual, expected.to_string()));
    }
    std::fs::write(
        dir.path().join("src/callers.rs"),
        "// moved\n\npub fn first() -> u32 {\n    7\n}\n",
    )
    .unwrap();
    let edited = answer(&client, json!({ "path": "src/callers.rs", "symbol": "first" })).await;
    rows.push(("code_read edited".to_string(), edited, r#"{"path":"src/callers.rs","name":"first","kind":"function","line":3,"end_line":5,"body":"pub fn first() -> u32 {\n    7\n}","bytes":31,"stale":true}"#.to_string()));
    assert_all(&rows);
}

/// The refusal for a file the index holds but the working tree no longer has.
const DELETED: &str = r#"`code_read`: `src/outline.rs` is in the index but can no longer be read: it was deleted or moved after the scan. Run `index_rescan {"paths":["src/outline.rs"]}` to bring the index up to date."#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_code_read_refusal_keeps_its_exact_words() {
    let (dir, client) = started().await;
    std::fs::write(dir.path().join("src/late.rs"), "pub fn late() {}\n").unwrap();
    let cases = [
        (
            json!({ "path": "src/outline.rs" }),
            r#"`code_read`: neither `symbol` nor `line` was given; pass {path, symbol} (a symbol's body; add `line`, any line inside it, or `kind` when several share the name) or {path, line, end_line} (a 1-based inclusive line span)"#,
        ),
        (
            json!({ "path": "src/outline.rs", "symbol": "render", "end_line": 3 }),
            r#"`code_read`: `end_line` applies only to a line span; pass {path, symbol} (a symbol's body; add `line`, any line inside it, or `kind` when several share the name) or {path, line, end_line} (a 1-based inclusive line span)"#,
        ),
        (
            json!({ "path": "src/outline.rs", "line": 3, "kind": "function" }),
            r#"`code_read`: `kind` applies only to a symbol read; pass {path, symbol} (a symbol's body; add `line`, any line inside it, or `kind` when several share the name) or {path, line, end_line} (a 1-based inclusive line span)"#,
        ),
        (
            json!({ "path": "src/outline.rs", "line": 0 }),
            r#"`code_read`: lines are 1-based and `end_line` must be >= `line` (got 0..0)"#,
        ),
        (
            json!({ "path": "src/outline.rs", "line": 5, "end_line": 3 }),
            r#"`code_read`: lines are 1-based and `end_line` must be >= `line` (got 5..3)"#,
        ),
        (
            json!({ "path": "src/outline.rs", "line": 99 }),
            r#"`code_read`: `src/outline.rs` has 21 lines; line 99 is out of range"#,
        ),
        (
            json!({ "path": "src/outline.rs", "symbol": "nope" }),
            r#"`code_read`: no symbol `nope` in `src/outline.rs`; it defines: file_outline (lines 3-5), file_outline_l2 (lines 7-9), outline_file_outline_cached (lines 11-13), Renderer (line 15), Renderer (lines 17-21), render (lines 18-20)"#,
        ),
        (
            json!({ "path": "src/outline.rs", "symbol": "render", "kind": "bogus" }),
            r#"`code_read`: invalid kind Some("bogus"): unknown symbol kind: bogus"#,
        ),
        (
            json!({ "path": "src/outline.rs", "symbol": "render", "line": 1 }),
            r#"`code_read`: `line` 1 is inside no definition of `render` in `src/outline.rs`: function render (lines 18-20)"#,
        ),
        (
            json!({ "path": "nope.rs", "symbol": "x" }),
            r#"`code_read`: no file `nope.rs` in this repository. Locate it with `find_files {"query":"…"}`."#,
        ),
        (
            json!({ "path": "nope.rs", "line": 1 }),
            r#"`code_read`: no file `nope.rs` in this repository. Locate it with `find_files {"query":"…"}`."#,
        ),
        (
            json!({ "path": "../outside.rs", "line": 1 }),
            r#"`code_read`: `../outside.rs` is outside this repository. Pass a repo-relative path with no `..`; locate one with `find_files {"query":"…"}`."#,
        ),
        (
            json!({ "path": "src/late.rs", "symbol": "late" }),
            r#"`code_read`: `src/late.rs` exists but is not indexed. Run `index_rescan {"paths":["src/late.rs"]}` and retry; if it is still missing the file is ignored or not a supported language."#,
        ),
        (
            json!({ "path": "src/late.rs", "line": 1 }),
            r#"`code_read`: `src/late.rs` exists but is not indexed. Run `index_rescan {"paths":["src/late.rs"]}` and retry; if it is still missing the file is ignored or not a supported language."#,
        ),
    ];
    let mut rows = Vec::new();
    for (arguments, expected) in cases {
        let actual = call_err(&client, "code_read", arguments.clone()).await;
        rows.push((format!("code_read {arguments}"), actual, expected.to_string()));
    }
    std::fs::remove_file(dir.path().join("src/outline.rs")).unwrap();
    for arguments in [
        json!({ "path": "src/outline.rs", "symbol": "render" }),
        json!({ "path": "src/outline.rs", "line": 1 }),
    ] {
        let actual = call_err(&client, "code_read", arguments.clone()).await;
        rows.push((format!("code_read deleted {arguments}"), actual, DELETED.to_string()));
    }
    assert_all(&rows);
}
