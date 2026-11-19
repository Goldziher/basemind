//! `code_read` answers byte for byte: a symbol's body, a line span, an edited file's fresh parse,
//! and every refusal's exact words.
//!
//! The tool's body moves from the MCP adapter into the engine; these pin what an agent reads so the
//! move (and any later one) cannot change a response or an error text unnoticed.

use serde_json::{Value, json};

use super::{Client, call, call_err, call_raw, started, started_with, text_of};

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
            r#"`code_read`: neither `symbol` nor `line` was given; pass {path, symbol} (a symbol's body; add `line`, any line inside it, or `kind` when several share the name), {path, line, end_line} (a 1-based inclusive line span) or {path, ranges} (several spans, e.g. "120-148,310")"#,
        ),
        (
            json!({ "path": "src/outline.rs", "symbol": "render", "end_line": 3 }),
            r#"`code_read`: `end_line` applies only to a line span; pass {path, symbol} (a symbol's body; add `line`, any line inside it, or `kind` when several share the name), {path, line, end_line} (a 1-based inclusive line span) or {path, ranges} (several spans, e.g. "120-148,310")"#,
        ),
        (
            json!({ "path": "src/outline.rs", "line": 3, "kind": "function" }),
            r#"`code_read`: `kind` applies only to a symbol read; pass {path, symbol} (a symbol's body; add `line`, any line inside it, or `kind` when several share the name), {path, line, end_line} (a 1-based inclusive line span) or {path, ranges} (several spans, e.g. "120-148,310")"#,
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

/// `ranges` reads several spans in one call: sorted, merged, each clamped to the file, each with its
/// text; a span past the end and every shape `ranges` does not combine with are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_read_ranges_answers_every_span_in_one_call() {
    let (_dir, client) = started().await;
    let rows = vec![
        (
            "two spans, out of order and overlapping".to_string(),
            answer(&client, json!({ "path": "src/outline.rs", "ranges": "20-99,3-4,4-5" })).await,
            r#"{"path":"src/outline.rs","total_lines":21,"spans":[{"line":3,"end_line":5,"body":"pub fn file_outline() -> u32 {\n    1\n}"},{"line":20,"end_line":21,"body":"    }\n}"}],"bytes":45}"#.to_string(),
        ),
        (
            "one line".to_string(),
            answer(&client, json!({ "path": "src/outline.rs", "ranges": " 15 " })).await,
            r#"{"path":"src/outline.rs","total_lines":21,"spans":[{"line":15,"end_line":15,"body":"pub struct Renderer;"}],"bytes":20}"#.to_string(),
        ),
    ];
    let shapes = r#"pass {path, symbol} (a symbol's body; add `line`, any line inside it, or `kind` when several share the name), {path, line, end_line} (a 1-based inclusive line span) or {path, ranges} (several spans, e.g. "120-148,310")"#;
    let together = format!(
        "`code_read`: `ranges` names every span itself and takes none of `symbol`, `line`, `end_line` or `kind`; {shapes}"
    );
    let refusals = [
        (json!({ "path": "src/outline.rs", "ranges": "3-5", "line": 3 }), together.clone()),
        (json!({ "path": "src/outline.rs", "ranges": "3-5", "symbol": "render" }), together.clone()),
        (json!({ "path": "src/outline.rs", "ranges": "3-5", "end_line": 5 }), together),
        (
            json!({ "path": "src/outline.rs", "ranges": "3-" }),
            r#"ranges item `3-` is not a line number or span; pass 1-based lines or inclusive spans separated by commas, e.g. "120-148,310""#.to_string(),
        ),
        (
            json!({ "path": "src/outline.rs", "ranges": "5-3" }),
            r#"ranges item `5-3` ends before it starts; pass 1-based lines or inclusive spans separated by commas, e.g. "120-148,310""#.to_string(),
        ),
        (
            json!({ "path": "src/outline.rs", "ranges": "1,,2" }),
            r#"ranges item `` is empty; pass 1-based lines or inclusive spans separated by commas, e.g. "120-148,310""#.to_string(),
        ),
        (
            json!({ "path": "src/outline.rs", "ranges": "0" }),
            r#"ranges item `0` names line 0; pass 1-based lines or inclusive spans separated by commas, e.g. "120-148,310""#.to_string(),
        ),
        (
            json!({ "path": "src/outline.rs", "ranges": "3-5,40-50" }),
            r#"`code_read`: `src/outline.rs` has 21 lines; line 40 is out of range"#.to_string(),
        ),
    ];
    let mut rows = rows;
    for (arguments, expected) in refusals {
        let actual = call_err(&client, "code_read", arguments.clone()).await;
        rows.push((format!("code_read {arguments}"), actual, expected));
    }
    assert_all(&rows);
}

/// The 128 KiB cap holds across every span of a `ranges` read: the span it falls in ends at its last
/// whole line that fits, later spans are left out, and `next_ranges` reads on from there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_read_ranges_cuts_across_spans_and_says_where_to_read_on() {
    // 2,000 lines of 100 bytes each (99 and the newline).
    let big = format!("// {}\n", "x".repeat(96)).repeat(2_000);
    let (_dir, client) = started_with(&[("src/big.rs", &big)]).await;

    let cut = call(
        &client,
        "code_read",
        json!({ "path": "src/big.rs", "ranges": "1-1000,1500-2000" }),
    )
    .await;
    let spans = cut["spans"].as_array().expect("spans");
    assert_eq!(spans.len(), 2, "{cut}");
    assert_eq!(
        (spans[0]["line"].as_u64(), spans[0]["end_line"].as_u64()),
        (Some(1), Some(1000))
    );
    // 131,072 - 99,999 bytes of the first span leave room for 310 whole lines of the second.
    assert_eq!(
        (spans[1]["line"].as_u64(), spans[1]["end_line"].as_u64()),
        (Some(1500), Some(1809))
    );
    let kept: usize = spans.iter().map(|span| span["body"].as_str().unwrap().len()).sum();
    assert!(kept <= 128 * 1024, "{kept}");
    assert_eq!(cut["truncated"], true);
    assert_eq!(cut["truncated_reason"], "size_cap");
    assert_eq!(cut["bytes"].as_u64(), Some(99_999 + 50_099));
    assert_eq!(cut["omitted_bytes"].as_u64(), Some((99_999 + 50_099 - kept) as u64));
    assert_eq!(cut["next_ranges"], "1810-2000");

    let rest = call(
        &client,
        "code_read",
        json!({ "path": "src/big.rs", "ranges": "1810-2000" }),
    )
    .await;
    assert_eq!(rest["spans"][0]["end_line"].as_u64(), Some(2000), "{rest}");
    assert!(
        rest.get("truncated").is_none() && rest.get("next_ranges").is_none(),
        "{rest}"
    );

    // A later span with no whole line left to fit is left out whole and named in `next_ranges`.
    let full = call(
        &client,
        "code_read",
        json!({ "path": "src/big.rs", "ranges": "1-1310,1400,1600-1601" }),
    )
    .await;
    assert_eq!(full["spans"].as_array().map(Vec::len), Some(1), "{full}");
    assert_eq!(full["spans"][0]["end_line"].as_u64(), Some(1310), "{full}");
    assert_eq!(full["next_ranges"], "1400,1600-1601", "{full}");
}
