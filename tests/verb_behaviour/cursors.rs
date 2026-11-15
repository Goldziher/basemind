//! A cursor belongs to the query that issued it: every paginated tool refuses another query's
//! cursor, and a cursor that outlived its index or history is refused the same way everywhere.

use serde_json::{Value, json};

use super::{Client, call, call_err, git, started};

/// Query A's `next_cursor`, fed to query B, must be refused as belonging to a different query.
async fn assert_foreign_cursor_is_refused(
    client: &Client,
    tool_a: &'static str,
    query_a: Value,
    tool_b: &'static str,
    mut query_b: Value,
) {
    // A cursor can also go stale between two calls (the map finishes warming, the watcher rescans),
    // which is the other refusal and says so. The page is then simply asked for again.
    let mut attempts = 0;
    let cursor = loop {
        let first = call(client, tool_a, query_a.clone()).await;
        let cursor = first["next_cursor"].clone();
        assert!(
            cursor.is_string(),
            "{tool_a} {query_a} must have a second page: {first}"
        );
        let mut same_query = query_a.clone();
        same_query["cursor"] = cursor.clone();
        let second = client
            .call_tool(
                rmcp::model::CallToolRequestParams::new(tool_a)
                    .with_arguments(same_query.as_object().cloned().expect("object")),
            )
            .await
            .unwrap_or_else(|error| panic!("{tool_a}: {error}"));
        if second.is_error != Some(true) {
            break cursor;
        }
        let message = super::text_of(&second);
        attempts += 1;
        assert!(
            message.contains("changed since this cursor was issued") && attempts < 5,
            "the cursor must continue its own query: {message}"
        );
    };

    query_b["cursor"] = cursor;
    let message = call_err(client, tool_b, query_b.clone()).await;
    assert!(
        message.contains("belongs to a different query") && message.contains("without `cursor`"),
        "{tool_b} {query_b}: {message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_outline_refuses_another_file_s_cursor() {
    let (_dir, client) = started().await;
    assert_foreign_cursor_is_refused(
        &client,
        "code_outline",
        json!({ "path": "src/outline.rs", "limit": 1 }),
        "code_outline",
        json!({ "path": "src/callers.rs", "limit": 1 }),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_symbols_refuses_another_name_s_cursor() {
    let (_dir, client) = started().await;
    assert_foreign_cursor_is_refused(
        &client,
        "code_symbols",
        json!({ "name": "file_outline", "limit": 1 }),
        "code_symbols",
        json!({ "name": "render", "limit": 1 }),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_refuses_another_pattern_s_cursor() {
    let (_dir, client) = started().await;
    assert_foreign_cursor_is_refused(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline", "limit": 1 }),
        "code_grep",
        json!({ "pattern": "pub fn", "limit": 1 }),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_refuses_another_name_s_cursor() {
    let (_dir, client) = started().await;
    assert_foreign_cursor_is_refused(
        &client,
        "code_refs",
        json!({ "name": "file_outline_l2", "limit": 1 }),
        "code_refs",
        json!({ "name": "file_outline", "limit": 1 }),
    )
    .await;
    assert_foreign_cursor_is_refused(
        &client,
        "code_refs",
        json!({ "name": "file_outline_l2", "limit": 1 }),
        "code_refs",
        json!({ "name": "file_outline_l2", "match": "substring", "limit": 1 }),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn find_files_refuses_another_query_s_cursor() {
    let (_dir, client) = started().await;
    assert_foreign_cursor_is_refused(
        &client,
        "find_files",
        json!({ "limit": 1 }),
        "find_files",
        json!({ "path_prefix": "src/", "limit": 1 }),
    )
    .await;
    assert_foreign_cursor_is_refused(
        &client,
        "find_files",
        json!({ "query": "s", "limit": 1 }),
        "find_files",
        json!({ "query": "r", "limit": 1 }),
    )
    .await;
}

/// A second commit, so the log has two entries and `src/outline.rs` blames to two hunks.
fn second_commit(root: &std::path::Path) {
    let outline = std::fs::read_to_string(root.join("src/outline.rs")).unwrap();
    std::fs::write(root.join("src/outline.rs"), outline.replace("    2\n", "    22\n")).unwrap();
    git(root, &["commit", "-qam", "bump the second outline"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_refuses_another_query_s_cursor() {
    let (dir, client) = started().await;
    second_commit(dir.path());

    assert_foreign_cursor_is_refused(
        &client,
        "git_log",
        json!({ "limit": 1 }),
        "git_log",
        json!({ "path": "src/outline.rs", "limit": 1 }),
    )
    .await;
    assert_foreign_cursor_is_refused(
        &client,
        "git_log",
        json!({ "path": "src/outline.rs", "limit": 1 }),
        "git_log",
        json!({ "path": "src/callers.rs", "limit": 1 }),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_blame_refuses_another_file_s_cursor() {
    let (dir, client) = started().await;
    second_commit(dir.path());

    assert_foreign_cursor_is_refused(
        &client,
        "git_blame",
        json!({ "path": "src/outline.rs", "limit": 1 }),
        "git_blame",
        json!({ "path": "src/callers.rs", "limit": 1 }),
    )
    .await;
}

/// A cursor issued before HEAD moved is an error, like one issued before a rescan: an empty page
/// in its place would read as the end of the list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cursor_that_outlived_its_history_is_an_error() {
    let (dir, client) = started().await;
    second_commit(dir.path());

    let first = call(&client, "git_log", json!({ "limit": 1 })).await;
    let cursor = first["next_cursor"].clone();
    assert!(cursor.is_string(), "{first}");

    std::fs::write(dir.path().join("later.rs"), "pub fn later() {}\n").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "later"]);

    let message = call_err(&client, "git_log", json!({ "limit": 1, "cursor": cursor })).await;
    assert!(
        message.contains("changed since this cursor was issued") && message.contains("without `cursor`"),
        "{message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cursor_that_outlived_its_index_is_an_error() {
    let (dir, client) = started().await;

    let first = call(&client, "code_symbols", json!({ "name": "file_outline", "limit": 1 })).await;
    let cursor = first["next_cursor"].clone();
    assert!(cursor.is_string(), "{first}");

    std::fs::write(dir.path().join("src/extra.rs"), "pub fn file_outline_extra() {}\n").unwrap();
    call(&client, "index_rescan", json!({ "paths": ["src/extra.rs"] })).await;

    let message = call_err(
        &client,
        "code_symbols",
        json!({ "name": "file_outline", "limit": 1, "cursor": cursor }),
    )
    .await;
    assert!(
        message.contains("changed since this cursor was issued") && message.contains("without `cursor`"),
        "{message}"
    );
}

/// Every ranked or listing tool checks the binding before its body runs, so a cursor from another
/// tool is refused the same way whether or not the tool's retrieval stack is compiled in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cursor_from_another_tool_is_refused() {
    let (_dir, client) = started().await;
    let first = call(&client, "code_symbols", json!({ "name": "file_outline", "limit": 1 })).await;
    let cursor = first["next_cursor"].clone();
    assert!(cursor.is_string(), "{first}");

    for (tool, arguments) in [
        ("code_search", json!({ "query": "outline", "cursor": cursor })),
        ("docs_search", json!({ "query": "outline", "cursor": cursor })),
        ("memory_search", json!({ "prefix": "", "cursor": cursor })),
        ("find_files", json!({ "cursor": cursor })),
    ] {
        let message = call_err(&client, tool, arguments).await;
        assert!(message.contains("belongs to a different query"), "{tool}: {message}");
    }
}

#[cfg(feature = "memory")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_search_refuses_another_prefix_s_cursor() {
    let (_dir, client) = started().await;
    for key in ["alpha/one", "alpha/two", "beta/one", "beta/two"] {
        let saved = client
            .call_tool(
                rmcp::model::CallToolRequestParams::new("memory_save")
                    .with_arguments(json!({ "key": key, "value": "note" }).as_object().cloned().unwrap()),
            )
            .await
            .expect("memory_save");
        let _ = saved;
    }
    assert_foreign_cursor_is_refused(
        &client,
        "memory_search",
        json!({ "prefix": "alpha/", "limit": 1 }),
        "memory_search",
        json!({ "prefix": "beta/", "limit": 1 }),
    )
    .await;
}

#[cfg(feature = "code-search")]
const NOTES_RS: &str = "\
/// Parse a configuration file into a typed value.
pub fn parse_config() {}

/// Retry a failed upload with exponential backoff.
pub fn retry_upload() {}

/// Render the outline of a source file.
pub fn render_outline() {}
";

/// The ranked tools share one cursor shape, so only the binding to the query keeps a
/// `code_search` cursor out of `docs_search`.
#[cfg(feature = "code-search")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_search_refuses_another_query_s_cursor() {
    let (_dir, client) = super::started_with(&[("src/notes.rs", NOTES_RS)]).await;
    let first = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("code_search").with_arguments(
                json!({ "query": "outline", "lane": "keyword", "limit": 1 })
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .expect("code_search");
    let text = super::text_of(&first);
    assert_ne!(first.is_error, Some(true), "the keyword lane needs no model: {text}");
    let body: Value =
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("code_search: non-JSON payload ({error}): {text}"));
    let cursor = body["next_cursor"].clone();
    assert!(cursor.is_string(), "several chunks mention `outline`: {body}");

    let other = call_err(
        &client,
        "code_search",
        json!({ "query": "upload", "lane": "keyword", "limit": 1, "cursor": cursor.clone() }),
    )
    .await;
    assert!(other.contains("belongs to a different query"), "{other}");

    #[cfg(feature = "documents")]
    {
        let docs = call_err(&client, "docs_search", json!({ "query": "outline", "cursor": cursor })).await;
        assert!(docs.contains("belongs to a different query"), "{docs}");
    }
}

/// `[documents.output] format = "toon"` reaches `code_search` through the same response formatter
/// as `docs_search`. TOON has no structured form to cut a page from, so a verb tool that honoured
/// it returned one unpaged block. The keyword lane needs no embedding model, so this runs everywhere.
#[cfg(feature = "code-search")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_search_pages_as_json_when_the_config_asks_for_toon() {
    let dir = super::fixture_with(&[("src/notes.rs", NOTES_RS)]);
    std::fs::write(
        dir.path().join("basemind.toml"),
        "\"$schema\" = \"v1\"\n\n[documents.output]\nformat = \"toon\"\n",
    )
    .unwrap();
    super::scan(dir.path());
    let client = super::serve(dir.path()).await;

    let arguments = json!({ "query": "outline", "lane": "keyword", "limit": 1 });
    let first = super::call(&client, "code_search", arguments.clone()).await;
    let first_hits = first["hits"].as_array().expect("hits");
    assert_eq!(first_hits.len(), 1, "`limit` is honoured on the first page: {first}");
    let cursor = first["next_cursor"].clone();
    assert!(cursor.is_string(), "several chunks mention `outline`: {first}");

    let mut paged = arguments;
    paged["cursor"] = cursor;
    let second = super::call(&client, "code_search", paged).await;
    let second_hits = second["hits"].as_array().expect("hits");
    assert_eq!(second_hits.len(), 1, "{second}");
    assert_ne!(first_hits[0], second_hits[0], "the cursor advances: {second}");
}
