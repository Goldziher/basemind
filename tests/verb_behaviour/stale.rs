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
    assert_eq!(grep["total"], 1, "{grep}");
    assert!(
        super::rows(&grep["hits"])[0].get("symbol").is_none(),
        "the indexed spans name `b`, which the file no longer has: {grep}"
    );
}

fn prepend(path: &std::path::Path, text: &str) {
    let source = std::fs::read_to_string(path).unwrap();
    std::fs::write(path, format!("{text}{source}")).unwrap();
}

/// `code_refs` answers from indexed byte offsets: resolution edges, definition starts, a symbol's
/// span. Laid over an edited file they name some other line, so the call is refused with the way
/// to bring the index up to date.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_refuses_positions_in_a_file_edited_after_the_scan() {
    let (dir, client) = started().await;

    let before = call(
        &client,
        "code_refs",
        json!({ "name": "total", "relation": "definition", "path": "tools.py", "line": 3 }),
    )
    .await;
    assert_eq!(before["definition"]["line"], 2, "{before}");

    prepend(&dir.path().join("tools.py"), "# one\n# two\n");
    let definition = call_err(
        &client,
        "code_refs",
        json!({ "name": "total", "relation": "definition", "path": "tools.py", "line": 5 }),
    )
    .await;
    assert!(
        definition.contains("`tools.py` changed since it was indexed")
            && definition.contains("index_rescan {\"paths\":[\"tools.py\"]}"),
        "{definition}"
    );
    let with_column = call_err(
        &client,
        "code_refs",
        json!({ "name": "total", "relation": "definition", "path": "tools.py", "line": 5, "column": 12 }),
    )
    .await;
    assert_eq!(with_column, definition, "the column form is judged the same way");

    prepend(&dir.path().join("src/outline.rs"), "// one\n// two\n");
    let pinned = call_err(
        &client,
        "code_refs",
        json!({ "name": "file_outline", "path": "src/outline.rs" }),
    )
    .await;
    assert!(
        pinned.contains("`src/outline.rs` changed since it was indexed") && pinned.contains("index_rescan"),
        "{pinned}"
    );

    let unpinned = call(&client, "code_refs", json!({ "name": "file_outline" })).await;
    assert_eq!(unpinned["total"], 2, "the name scan reads no file: {unpinned}");
}

#[cfg(feature = "code-intel-js")]
const LIB_TS: &str = "export function helper(): number {\n  return 1;\n}\n";
#[cfg(feature = "code-intel-js")]
const USE_TS: &str = "import { helper } from './lib';\n\nexport const value = helper();\n";

/// A cross-file edge lands on a byte offset in another file. That file is held to the same test as
/// the one the reference is in.
#[cfg(feature = "code-intel-js")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_refuses_a_definition_in_another_file_edited_after_the_scan() {
    let (dir, client) = started_with(&[("web/lib.ts", LIB_TS), ("web/use.ts", USE_TS)]).await;
    let reference = json!({ "name": "helper", "relation": "definition", "path": "web/use.ts", "line": 3 });

    let fresh = call(&client, "code_refs", reference.clone()).await;
    assert_eq!(fresh["definition"]["path"], "web/lib.ts", "{fresh}");
    assert_eq!(fresh["definition"]["line"], 1, "{fresh}");

    prepend(&dir.path().join("web/lib.ts"), "// one\n// two\n");
    let stale = call_err(&client, "code_refs", reference).await;
    assert!(
        stale.contains("`web/lib.ts` changed since it was indexed") && stale.contains("index_rescan"),
        "{stale}"
    );
}

/// An edited file is described as it is: the outline is extracted from the bytes on disk, so its
/// lines are the ones `code_read` returns, and a grep hit in it says the index is behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edited_file_is_outlined_and_read_as_it_is_and_flagged_stale() {
    let (dir, client) = started().await;
    prepend(&dir.path().join("src/outline.rs"), "// one\n// two\n");

    let outline = call(&client, "code_outline", json!({ "path": "src/outline.rs" })).await;
    assert_eq!(outline["stale"], true, "{outline}");
    let symbol = outline["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == "file_outline_l2")
        .expect("file_outline_l2");
    assert_eq!(
        (symbol["line"].as_u64(), symbol["end_line"].as_u64()),
        (Some(9), Some(11)),
        "two lines further down than the index has it: {outline}"
    );
    let read = call(
        &client,
        "code_read",
        json!({ "path": "src/outline.rs", "symbol": "file_outline_l2" }),
    )
    .await;
    assert_eq!(
        (read["line"].clone(), read["end_line"].clone(), read["stale"].clone()),
        (symbol["line"].clone(), symbol["end_line"].clone(), json!(true)),
        "{read}"
    );

    let grep = call(&client, "code_grep", json!({ "pattern": "file_outline() +" })).await;
    let hit = &super::rows(&grep["hits"])[0];
    assert_eq!(hit["line"], 21, "{grep}");
    assert_eq!(hit["stale"], true, "{grep}");
    assert!(hit.get("symbol").is_none(), "{grep}");
    let fresh = call(&client, "code_grep", json!({ "pattern": "def helper" })).await;
    assert!(super::rows(&fresh["hits"])[0].get("stale").is_none(), "{fresh}");
}

/// A file deleted after the scan is still in the index. Every tool that needs its content says so
/// in the same words and names the call that fixes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_deleted_after_the_scan_gets_one_answer_naming_index_rescan() {
    let (dir, client) = started().await;
    std::fs::remove_file(dir.path().join("src/outline.rs")).unwrap();

    for (tool, arguments) in [
        ("code_outline", json!({ "path": "src/outline.rs" })),
        (
            "code_read",
            json!({ "path": "src/outline.rs", "symbol": "file_outline" }),
        ),
        (
            "code_read",
            json!({ "path": "src/outline.rs", "line": 1, "end_line": 3 }),
        ),
        ("code_refs", json!({ "name": "file_outline", "path": "src/outline.rs" })),
        (
            "code_refs",
            json!({ "name": "file_outline", "relation": "definition", "path": "src/outline.rs", "line": 19 }),
        ),
    ] {
        let message = call_err(&client, tool, arguments.clone()).await;
        assert_eq!(
            message,
            format!(
                "`{tool}`: `src/outline.rs` is in the index but can no longer be read: it was deleted or moved \
                 after the scan. Run `index_rescan {{\"paths\":[\"src/outline.rs\"]}}` to bring the index up to date."
            ),
            "{tool} {arguments}"
        );
    }

    let grep = call(&client, "code_grep", json!({ "pattern": "file_outline" })).await;
    assert_eq!(grep["skipped_files"], 1, "{grep}");
    assert_eq!(
        grep["hits"]
            .as_object()
            .map(|files| files.keys().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["src/callers.rs"]),
        "{grep}"
    );
}

/// The scan indexes no file over `scan.max_file_bytes`. One that has grown past it since is not
/// held whole by any tool; a line span is still streamed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_grown_past_the_scan_limit_is_not_read_whole() {
    let (dir, client) = started_with(&[(
        "basemind.toml",
        "\"$schema\" = \"v1\"\n\n[scan]\nmax_file_bytes = 4096\n",
    )])
    .await;
    let path = dir.path().join("src/outline.rs");
    let source = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        format!("{source}{}", "// padding to outgrow the limit\n".repeat(200)),
    )
    .unwrap();

    for (tool, arguments) in [
        ("code_outline", json!({ "path": "src/outline.rs" })),
        (
            "code_read",
            json!({ "path": "src/outline.rs", "symbol": "file_outline" }),
        ),
        ("code_refs", json!({ "name": "file_outline", "path": "src/outline.rs" })),
    ] {
        let message = call_err(&client, tool, arguments.clone()).await;
        assert!(
            message.contains("has grown past 4096 bytes (`scan.max_file_bytes`)") && message.contains("index_rescan"),
            "{tool} {arguments}: {message}"
        );
    }
    let span = call(
        &client,
        "code_read",
        json!({ "path": "src/outline.rs", "line": 3, "end_line": 5 }),
    )
    .await;
    assert_eq!(span["body"], "pub fn file_outline() -> u32 {\n    1\n}", "{span}");
    let grep = call(&client, "code_grep", json!({ "pattern": "padding to outgrow" })).await;
    assert_eq!(
        (grep["total"].as_u64(), grep["skipped_files"].as_u64()),
        (Some(0), Some(1)),
        "{grep}"
    );
}
