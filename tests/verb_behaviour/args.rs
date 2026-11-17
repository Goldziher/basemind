//! Argument handling through the real router: stringified scalars are coerced, a type error names
//! its field, and "no arguments" is a valid call.

use rmcp::model::CallToolRequestParams;
use serde_json::json;

use super::{call, call_err, started, text_of};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stringified_integers_and_booleans_are_coerced() {
    let (_dir, client) = started().await;

    let limited = call(&client, "code_symbols", json!({ "name": "file_outline", "limit": "1" })).await;
    assert_eq!(limited["results"].as_array().map(Vec::len), Some(1), "{limited}");
    assert!(limited["next_cursor"].is_string(), "{limited}");

    let with_context = call(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline() +", "context": "true" }),
    )
    .await;
    assert_eq!(
        super::rows(&with_context["hits"])[0]["context_before"],
        "    pub fn render(&self) -> u32 {",
        "{with_context}"
    );

    let without_context = call(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline() +", "context": "false" }),
    )
    .await;
    assert!(
        super::rows(&without_context["hits"])[0].get("context_before").is_none(),
        "{without_context}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_type_error_names_the_field() {
    let (_dir, client) = started().await;

    let limit = call_err(&client, "code_symbols", json!({ "name": "x", "limit": "many" })).await;
    assert!(limit.contains("`limit` must be an integer"), "{limit}");

    let name = call_err(&client, "code_symbols", json!({ "name": 5 })).await;
    assert!(name.contains("`name` must be a string"), "{name}");

    let flag = call_err(&client, "code_grep", json!({ "pattern": "x", "regex": "maybe" })).await;
    assert!(flag.contains("`regex` must be a boolean"), "{flag}");

    let both = call_err(
        &client,
        "code_grep",
        json!({ "pattern": "x", "glob": "*.rs", "limit": "many" }),
    )
    .await;
    assert!(
        both.contains("unknown field `glob`") && both.contains("`limit` must be an integer"),
        "unknown keys and type errors are reported together: {both}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn null_is_accepted_for_an_optional_field() {
    let (_dir, client) = started().await;

    let body = call(
        &client,
        "code_symbols",
        json!({ "name": "file_outline", "kind": null, "limit": null, "cursor": null }),
    )
    .await;
    assert_eq!(body["total"], 3, "{body}");

    let required = call_err(&client, "code_symbols", json!({ "name": null })).await;
    assert!(required.contains("`name` is required"), "{required}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_status_takes_empty_or_absent_arguments() {
    let (_dir, client) = started().await;

    let empty = call(&client, "index_status", json!({})).await;
    assert!(empty["cache_dir"].is_string(), "{empty}");

    let absent = client
        .call_tool(CallToolRequestParams::new("index_status"))
        .await
        .expect("a call without `arguments` is answered");
    assert_ne!(absent.is_error, Some(true), "{}", text_of(&absent));
}

/// `index_rescan {}` walks the whole workspace and skips what did not change. There is no `full`
/// flag: it ran the same walk, and beside `paths` it silently discarded them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_rescan_takes_paths_and_no_full_flag() {
    let (_dir, client) = started().await;

    for arguments in [json!({ "full": true }), json!({ "full": true, "paths": ["tools.py"] })] {
        let refused = call_err(&client, "index_rescan", arguments.clone()).await;
        assert!(
            refused.contains("`full`") && refused.contains("paths"),
            "{arguments}: the unknown field and the accepted one are named: {refused}"
        );
    }

    let whole = call(&client, "index_rescan", json!({})).await;
    assert_eq!(whole["scanned"], 4, "every file of the fixture: {whole}");
    let scoped = call(&client, "index_rescan", json!({ "paths": ["tools.py"] })).await;
    assert_eq!(scoped["scanned"], 1, "{scoped}");
}
