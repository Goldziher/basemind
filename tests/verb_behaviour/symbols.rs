//! `code_symbols` answers byte for byte: the ranked rows, a page and its continuation, a fuzzy
//! near-miss, a row in a file edited since the scan, and every refusal's exact words.
//!
//! The tool's body moves from the MCP adapter into the engine; these pin what an agent reads so the
//! move (and any later one) cannot change a response or an error text unnoticed.

use serde_json::{Value, json};

use super::{Client, call_err, call_raw, started, text_of};

/// The tool's JSON text; it must not be an error.
async fn answer(client: &Client, arguments: Value) -> String {
    let result = call_raw(client, "code_symbols", arguments.clone()).await;
    let text = text_of(&result);
    assert_ne!(result.is_error, Some(true), "code_symbols {arguments} failed: {text}");
    text
}

/// `text` with its `next_cursor` replaced: the cursor carries the map generation, which a deferred
/// warm moves, so only its presence is pinned here (`cursors.rs` pins how cursors bind).
fn without_cursor(text: &str) -> String {
    match text.split_once("\"next_cursor\":\"") {
        Some((head, tail)) => {
            let end = tail.find('"').expect("a closed cursor string");
            format!("{head}\"next_cursor\":\"<cursor>\"{}", &tail[end + 1..])
        }
        None => text.to_string(),
    }
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
async fn code_symbols_answers_byte_for_byte() {
    let (dir, client) = started().await;
    let cases = [
        (
            json!({ "name": "file_outline" }),
            r#"{"results":["src/outline.rs:3: pub fn file_outline() -> u32","src/outline.rs:7: pub fn file_outline_l2() -> u32","src/outline.rs:11: pub fn outline_file_outline_cached() -> u32"]}"#,
        ),
        (
            json!({ "name": "render", "kind": "function" }),
            r#"{"results":["src/outline.rs:18: pub fn render(&self) -> u32"]}"#,
        ),
        (
            json!({ "name": "rendr", "fuzzy": true }),
            r#"{"results":["src/outline.rs:15: pub struct Renderer","src/outline.rs:17: impl Renderer","src/outline.rs:18: pub fn render(&self) -> u32"]}"#,
        ),
        (json!({ "name": "zzz_none" }), r#"{"results":[]}"#),
        (
            json!({ "name": "outline", "limit": 2 }),
            r#"{"total":3,"results":["src/outline.rs:11: pub fn outline_file_outline_cached() -> u32","src/outline.rs:3: pub fn file_outline() -> u32"],"next_cursor":"<cursor>"}"#,
        ),
    ];
    let mut rows = Vec::new();
    for (arguments, expected) in cases {
        let actual = without_cursor(&answer(&client, arguments.clone()).await);
        rows.push((format!("code_symbols {arguments}"), actual, expected.to_string()));
    }
    let first = basemind::wire::decode(&answer(&client, json!({ "name": "outline", "limit": 2 })).await).unwrap();
    let cursor = first["next_cursor"].as_str().expect("a second page").to_string();
    let rest = answer(&client, json!({ "name": "outline", "limit": 2, "cursor": cursor })).await;
    rows.push((
        "code_symbols page 2".to_string(),
        rest,
        r#"{"results":["src/outline.rs:7: pub fn file_outline_l2() -> u32"]}"#.to_string(),
    ));
    std::fs::write(
        dir.path().join("src/callers.rs"),
        "// moved\n\npub fn first() -> u32 {\n    7\n}\n",
    )
    .unwrap();
    let edited = answer(&client, json!({ "name": "first" })).await;
    rows.push((
        "code_symbols edited".to_string(),
        edited,
        r#"{"results":["src/callers.rs:3: pub fn first() -> u32"],"stale":["src/callers.rs"]}"#.to_string(),
    ));
    assert_all(&rows);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_code_symbols_refusal_keeps_its_exact_words() {
    let (_dir, client) = started().await;
    let cases = [
        (
            json!({ "name": "render", "limit": 0 }),
            r#"`code_symbols`: `limit` must be at least 1; omit it for the default page size."#,
        ),
        (
            json!({ "name": "render", "cursor": "not-bound" }),
            r#"`code_symbols`: this cursor belongs to a different query. A cursor continues only the call that returned it, with the same arguments apart from `limit` and `max_tokens`. Call again without `cursor`."#,
        ),
        (
            json!({ "name": "render", "kind": "bogus" }),
            r#"unknown symbol kind: bogus"#,
        ),
        (
            json!({ "name": "" }),
            r#"`code_symbols`: `name` is empty; pass the name of the definition to find."#,
        ),
    ];
    let mut rows = Vec::new();
    for (arguments, expected) in cases {
        let actual = call_err(&client, "code_symbols", arguments.clone()).await;
        rows.push((format!("code_symbols {arguments}"), actual, expected.to_string()));
    }
    assert_all(&rows);
}
