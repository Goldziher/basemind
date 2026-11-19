//! `code_outline` answers byte for byte: a page, its cursor, and every refusal's exact words.
//!
//! The tool's body moved from the MCP adapter into the engine; these pin what an agent reads so
//! the move (and any later one) cannot change a response or an error text unnoticed.

use serde_json::json;

use super::{Client, call_err, call_raw, json_of, started, text_of};

/// The tool's answer as JSON with the measured `elapsed_us` zeroed, the one field that varies per call.
async fn outline_text(client: &Client, arguments: serde_json::Value) -> String {
    let result = call_raw(client, "code_outline", arguments.clone()).await;
    assert_ne!(
        result.is_error,
        Some(true),
        "code_outline {arguments} failed: {}",
        text_of(&result)
    );
    let text = json_of("code_outline", &result);
    let (head, tail) = text.split_once("\"elapsed_us\":").expect("elapsed_us field");
    let digits = tail.find(|c: char| !c.is_ascii_digit()).unwrap_or(tail.len());
    format!("{head}\"elapsed_us\":0{}", &tail[digits..])
}

/// The cursor a page hands out, split into its query binding and the tool's own cursor.
fn split_cursor(cursor: &str) -> (&str, &str) {
    cursor.split_once('.').expect("a bound cursor is `<query>.<cursor>`")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_and_its_continuation_are_answered_byte_for_byte() {
    let (_dir, client) = started().await;

    let first = outline_text(&client, json!({ "path": "src/outline.rs", "limit": 2 })).await;
    let parsed: serde_json::Value = serde_json::from_str(&first).expect("JSON");
    let cursor = parsed["next_cursor"].as_str().expect("a second page").to_string();
    assert_eq!(
        first,
        format!(
            "{{\"path\":\"src/outline.rs\",\"language\":\"rust\",\"size_bytes\":281,\"total\":6,\"symbols\":[\
             {{\"name\":\"file_outline\",\"kind\":\"function\",\"line\":3,\"end_line\":5,\
             \"signature\":\"pub fn file_outline() -> u32\"}},\
             {{\"name\":\"file_outline_l2\",\"kind\":\"function\",\"line\":7,\"end_line\":9,\
             \"signature\":\"pub fn file_outline_l2() -> u32\"}}],\
             \"next_cursor\":\"{cursor}\",\"elapsed_us\":0}}"
        )
    );

    let rest = outline_text(
        &client,
        json!({ "path": "src/outline.rs", "limit": 2, "cursor": cursor }),
    )
    .await;
    let parsed: serde_json::Value = serde_json::from_str(&rest).expect("JSON");
    let next = parsed["next_cursor"].as_str().expect("a third page").to_string();
    assert_eq!(split_cursor(&next).0, split_cursor(&cursor).0, "one query, one binding");
    assert_eq!(
        rest,
        format!(
            "{{\"path\":\"src/outline.rs\",\"language\":\"rust\",\"size_bytes\":281,\"total\":6,\"symbols\":[\
             {{\"name\":\"outline_file_outline_cached\",\"kind\":\"function\",\"line\":11,\"end_line\":13,\
             \"signature\":\"pub fn outline_file_outline_cached() -> u32\"}},\
             {{\"name\":\"Renderer\",\"kind\":\"struct\",\"line\":15,\"end_line\":15,\
             \"signature\":\"pub struct Renderer\"}}],\
             \"next_cursor\":\"{next}\",\"elapsed_us\":0}}"
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edited_file_is_outlined_as_it_is_now_and_flagged_stale() {
    let (dir, client) = started().await;
    std::fs::write(dir.path().join("src/outline.rs"), "// moved\npub fn only() {}\n").expect("edit");

    let text = outline_text(&client, json!({ "path": "src/outline.rs" })).await;
    assert_eq!(
        text,
        "{\"path\":\"src/outline.rs\",\"language\":\"rust\",\"size_bytes\":26,\"total\":1,\"stale\":true,\
         \"symbols\":[{\"name\":\"only\",\"kind\":\"function\",\"line\":2,\"end_line\":2,\
         \"signature\":\"pub fn only()\"}],\"elapsed_us\":0}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_refusal_keeps_its_exact_words() {
    let (dir, client) = started().await;
    std::fs::write(dir.path().join("src/late.rs"), "pub fn late() {}\n").expect("late.rs");

    let refusals = [
        (
            json!({ "path": "src/late.rs" }),
            "`code_outline`: `src/late.rs` exists but is not indexed. Run `index_rescan \
             {\"paths\":[\"src/late.rs\"]}` and retry; if it is still missing the file is ignored or not a \
             supported language.",
        ),
        (
            json!({ "path": "nope.rs" }),
            "`code_outline`: no file `nope.rs` in this repository. Locate it with `find_files {\"query\":\"…\"}`.",
        ),
        (
            json!({ "path": "../outside.rs" }),
            "`code_outline`: `../outside.rs` is outside this repository. Pass a repo-relative path with no `..`; \
             locate one with `find_files {\"query\":\"…\"}`.",
        ),
        (
            json!({ "path": "src/outline.rs", "limit": 0 }),
            "`code_outline`: `limit` must be at least 1; omit it for the default page size.",
        ),
        (
            json!({ "path": "src/outline.rs", "cursor": "not-bound" }),
            "`code_outline`: this cursor belongs to a different query. A cursor continues only the call that \
             returned it, with the same arguments apart from `limit` and `max_tokens`. Call again without \
             `cursor`.",
        ),
    ];
    for (arguments, expected) in refusals {
        assert_eq!(call_err(&client, "code_outline", arguments).await, expected);
    }

    let first = call_raw(&client, "code_outline", json!({ "path": "src/outline.rs", "limit": 1 })).await;
    let first: serde_json::Value = serde_json::from_str(&json_of("code_outline", &first)).expect("JSON");
    let cursor = first["next_cursor"].as_str().expect("a second page").to_string();
    let (binding, _) = split_cursor(&cursor);
    let page_with =
        |inner: &str| json!({ "path": "src/outline.rs", "limit": 1, "cursor": format!("{binding}.{inner}") });

    let moved = basemind::testing::encode_in_memory_cursor(1, u32::MAX);
    assert_eq!(
        call_err(&client, "code_outline", page_with(&moved)).await,
        "`code_outline`: the index changed since this cursor was issued. Call again without `cursor`."
    );
    assert_eq!(
        call_err(&client, "code_outline", page_with("**")).await,
        "invalid cursor: non-base64url byte"
    );
    let not_msgpack = call_err(&client, "code_outline", page_with("AAAA")).await;
    assert!(
        not_msgpack.starts_with("invalid in-memory cursor payload: "),
        "{not_msgpack}"
    );

    std::fs::remove_file(dir.path().join("src/outline.rs")).expect("delete");
    assert_eq!(
        call_err(&client, "code_outline", json!({ "path": "src/outline.rs" })).await,
        "`code_outline`: `src/outline.rs` is in the index but can no longer be read: it was deleted or moved \
         after the scan. Run `index_rescan {\"paths\":[\"src/outline.rs\"]}` to bring the index up to date."
    );
}
