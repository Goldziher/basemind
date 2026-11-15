//! What the tools answer about a file that was edited after it was indexed.

use serde_json::json;

use super::{call, call_err, started, started_with};

const SWAP_INDEXED: &str = "pub fn aa() -> u32 {\n    1\n}\n\npub fn b() -> u32 {\n    2\n}\n";
/// The same length as [`SWAP_INDEXED`], with every byte of the second function one to the left.
const SWAP_EDITED: &str = "pub fn a() -> u32 {\n    1\n}\n\npub fn bb() -> u32 {\n    2\n}\n";

/// A symbol read returns the symbol as the file now has it. The indexed byte offsets belong to
/// the bytes that were scanned; laid over an edited file they cut out some other text and label it
/// with the symbol's name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_read_follows_a_symbol_through_an_edit_made_after_the_scan() {
    let (dir, client) = started().await;
    let path = dir.path().join("src/outline.rs");
    let source = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("// a new first line\n// and another\n{source}")).unwrap();

    let read = call(
        &client,
        "code_read",
        json!({ "path": "src/outline.rs", "symbol": "file_outline_l2" }),
    )
    .await;
    assert_eq!(read["body"], "pub fn file_outline_l2() -> u32 {\n    2\n}", "{read}");
    assert_eq!(read["line"], 9, "{read}");
    assert_eq!(read["end_line"], 11, "{read}");
    assert_eq!(read["stale"], true, "the index no longer describes this file: {read}");
}

/// An edit that keeps the file's size is still an edit: the size alone cannot vouch for the offsets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_that_keeps_the_size_is_seen_by_every_tool_that_uses_indexed_offsets() {
    assert_eq!(SWAP_INDEXED.len(), SWAP_EDITED.len());
    let (dir, client) = started_with(&[("src/swap.rs", SWAP_INDEXED)]).await;

    let fresh = call(&client, "code_read", json!({ "path": "src/swap.rs", "symbol": "b" })).await;
    assert_eq!(fresh["body"], "pub fn b() -> u32 {\n    2\n}", "{fresh}");
    assert!(fresh.get("stale").is_none(), "{fresh}");

    std::fs::write(dir.path().join("src/swap.rs"), SWAP_EDITED).unwrap();

    let renamed = call(&client, "code_read", json!({ "path": "src/swap.rs", "symbol": "bb" })).await;
    assert_eq!(renamed["body"], "pub fn bb() -> u32 {\n    2\n}", "{renamed}");
    assert_eq!(renamed["line"], 5, "{renamed}");

    let gone = call_err(&client, "code_read", json!({ "path": "src/swap.rs", "symbol": "b" })).await;
    assert!(gone.contains("no symbol `b` in `src/swap.rs`"), "{gone}");
    assert!(
        gone.contains("bb (lines 5-7)"),
        "the candidates are the file's current symbols: {gone}"
    );

    let outline = call(&client, "code_outline", json!({ "path": "src/swap.rs" })).await;
    assert_eq!(outline["stale"], true, "{outline}");

    let grep = call(
        &client,
        "code_grep",
        json!({ "pattern": "    2", "path": "src/swap.rs" }),
    )
    .await;
    assert_eq!(grep["total_matches"], 1, "{grep}");
    assert!(
        grep["hits"][0].get("symbol").is_none(),
        "the indexed spans name `b`, which the file no longer has: {grep}"
    );
}
