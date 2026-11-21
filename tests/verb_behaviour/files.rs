//! `find_files` answers byte for byte: a listing, a fuzzy and a typo match, the filters, a page
//! and its continuation, and every refusal's exact words.
//!
//! The tool's body moves from the MCP adapter into the engine; these pin what an agent reads so the
//! move (and any later one) cannot change a response or an error text unnoticed.

use serde_json::{Value, json};

use super::{Client, call_err, call_raw, json_of, started, text_of};

/// The tool's JSON text; it must not be an error.
async fn answer(client: &Client, arguments: Value) -> String {
    let result = call_raw(client, "find_files", arguments.clone()).await;
    assert_ne!(
        result.is_error,
        Some(true),
        "find_files {arguments} failed: {}",
        text_of(&result)
    );
    json_of("find_files", &result)
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
async fn find_files_answers_byte_for_byte() {
    let (_dir, client) = started().await;
    let cases = [
        (
            json!({}),
            r#"{"files":["src/callers.rs","src/outline.rs","tools.py","worker.rb"]}"#,
        ),
        (json!({ "query": "outline" }), r#"{"files":["src/outline.rs"]}"#),
        (json!({ "query": "wroker" }), r#"{"files":["worker.rb"]}"#),
        (
            json!({ "query": "rb", "language": "ruby" }),
            r#"{"files":["worker.rb"]}"#,
        ),
        (
            json!({ "path": "src/" }),
            r#"{"files":["src/callers.rs","src/outline.rs"]}"#,
        ),
        (json!({ "query": "zzz_none" }), r#"{"files":[]}"#),
        (
            json!({ "limit": 2 }),
            r#"{"total":4,"files":["src/callers.rs","src/outline.rs"],"next_cursor":"<cursor>"}"#,
        ),
        (
            json!({ "limit": 999999 }),
            r#"{"limit_clamped":true,"files":["src/callers.rs","src/outline.rs","tools.py","worker.rb"]}"#,
        ),
        (
            json!({ "min_lines": 7 }),
            r#"{"files":{"src/outline.rs":21,"src/callers.rs":9,"worker.rb":7}}"#,
        ),
        (
            json!({ "min_lines": 7, "query": "callers" }),
            r#"{"files":{"src/callers.rs":9}}"#,
        ),
        (
            json!({ "min_lines": 7, "language": "ruby" }),
            r#"{"files":{"worker.rb":7}}"#,
        ),
        (json!({ "min_lines": 1000 }), r#"{"files":{}}"#),
        (
            json!({ "min_lines": 7, "limit": 1 }),
            r#"{"total":3,"files":{"src/outline.rs":21},"next_cursor":"<cursor>"}"#,
        ),
    ];
    let mut rows = Vec::new();
    for (arguments, expected) in cases {
        let actual = without_cursor(&answer(&client, arguments.clone()).await);
        rows.push((format!("find_files {arguments}"), actual, expected.to_string()));
    }
    let first: Value = serde_json::from_str(&answer(&client, json!({ "limit": 2 })).await).unwrap();
    let cursor = first["next_cursor"].as_str().expect("a second page").to_string();
    let rest = answer(&client, json!({ "limit": 2, "cursor": cursor })).await;
    rows.push((
        "find_files page 2".to_string(),
        without_cursor(&rest),
        r#"{"files":["tools.py","worker.rb"]}"#.to_string(),
    ));
    let first: Value = serde_json::from_str(&answer(&client, json!({ "min_lines": 7, "limit": 1 })).await).unwrap();
    let cursor = first["next_cursor"]
        .as_str()
        .expect("a second page by size")
        .to_string();
    let rest = answer(&client, json!({ "min_lines": 7, "limit": 1, "cursor": cursor })).await;
    rows.push((
        "find_files min_lines page 2".to_string(),
        without_cursor(&rest),
        r#"{"total":2,"files":{"src/callers.rs":9},"next_cursor":"<cursor>"}"#.to_string(),
    ));
    assert_all(&rows);
    // A list is TOON and a `min_lines` map JSON, as an agent reads them (ADR-0053).
    let listed = call_raw(&client, "find_files", json!({ "query": "outline" })).await;
    assert_eq!(text_of(&listed), "files[1]: src/outline.rs");
    let sized = call_raw(&client, "find_files", json!({ "min_lines": 7, "language": "ruby" })).await;
    assert_eq!(text_of(&sized), r#"{"files":{"worker.rb":7}}"#);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_find_files_refusal_keeps_its_exact_words() {
    let (_dir, client) = started().await;
    let cases = [
        (
            json!({ "limit": 0 }),
            r#"`find_files`: `limit` must be at least 1; omit it for the default page size."#,
        ),
        (
            json!({ "min_lines": 0 }),
            r#"`find_files`: `min_lines` must be at least 1; omit it to list files of any size."#,
        ),
        (
            json!({ "min_lines": 7, "path": "nope/" }),
            r#"`find_files`: no indexed path starts with `nope/`; `path` is a repo-relative prefix, or a glob when it holds `*`. List what is indexed with `find_files`."#,
        ),
        (
            json!({ "cursor": "not-bound" }),
            r#"`find_files`: this cursor belongs to a different query. A cursor continues only the call that returned it, with the same arguments apart from `limit` and `max_tokens`. Call again without `cursor`."#,
        ),
        (
            json!({ "language": "cobol" }),
            r#"`find_files`: no indexed file has language `cobol`. Indexed languages: python, ruby, rust."#,
        ),
        (
            json!({ "path": "nope/" }),
            r#"`find_files`: no indexed path starts with `nope/`; `path` is a repo-relative prefix, or a glob when it holds `*`. List what is indexed with `find_files`."#,
        ),
        (
            json!({ "query": "outline", "path": "nope/" }),
            r#"`find_files`: no indexed path starts with `nope/`; `path` is a repo-relative prefix, or a glob when it holds `*`. List what is indexed with `find_files`."#,
        ),
    ];
    let mut rows = Vec::new();
    for (arguments, expected) in cases {
        let actual = call_err(&client, "find_files", arguments.clone()).await;
        rows.push((format!("find_files {arguments}"), actual, expected.to_string()));
    }
    assert_all(&rows);
}
