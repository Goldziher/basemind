//! A `limit` above a tool's maximum is lowered to it, and the answer says so with `limit_clamped`;
//! a `limit` at or below the maximum, or none, leaves the field out.

use serde_json::{Value, json};

use super::{call, started};
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
use super::started_with;

/// The largest `limit` any of these tools honours.
const LIMIT_MAX: u64 = 1000;

/// Call `tool` with `arguments` plus each `limit`, and check `limit_clamped` is `true` exactly when
/// the limit is above [`LIMIT_MAX`].
async fn assert_clamp_is_reported(client: &super::Client, tool: &'static str, arguments: Value) {
    for (limit, clamped) in [
        (Some(LIMIT_MAX + 1), true),
        (Some(u64::from(u32::MAX)), true),
        (Some(LIMIT_MAX), false),
        (Some(1), false),
        (None, false),
    ] {
        let mut arguments = arguments.clone();
        if let Some(limit) = limit {
            arguments["limit"] = json!(limit);
        }
        let answer = call(client, tool, arguments.clone()).await;
        let reported = answer.get("limit_clamped");
        if clamped {
            assert_eq!(reported, Some(&json!(true)), "{tool} {arguments}: {answer}");
        } else {
            assert_eq!(reported, None, "{tool} {arguments}: {answer}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_symbols_reports_a_clamped_limit() {
    let (_dir, client) = started().await;
    assert_clamp_is_reported(&client, "code_symbols", json!({ "name": "file_outline" })).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_outline_reports_a_clamped_limit() {
    let (_dir, client) = started().await;
    assert_clamp_is_reported(&client, "code_outline", json!({ "path": "src/outline.rs" })).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_reports_a_clamped_limit() {
    let (_dir, client) = started().await;
    assert_clamp_is_reported(&client, "code_grep", json!({ "pattern": "pub fn" })).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_reports_a_clamped_limit_on_every_relation() {
    let (_dir, client) = started().await;
    for arguments in [
        json!({ "name": "file_outline" }),
        json!({ "name": "file_outline", "path": "src/outline.rs" }),
        json!({ "name": "Renderer", "relation": "implementations" }),
        json!({ "name": "outline", "relation": "importers" }),
    ] {
        assert_clamp_is_reported(&client, "code_refs", arguments).await;
    }
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_reports_a_clamped_limit_on_the_import_graph() {
    let (_dir, client) = started_with(&super::graph::CRATE).await;
    for arguments in [
        json!({ "relation": "importers", "path": "g/src/a.rs" }),
        json!({ "relation": "cycles" }),
    ] {
        assert_clamp_is_reported(&client, "code_refs", arguments).await;
    }
}

#[cfg(feature = "memory")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_search_reports_a_clamped_limit_on_a_listing() {
    let (_dir, client) = started().await;
    assert_clamp_is_reported(&client, "memory_search", json!({ "prefix": "" })).await;
}
