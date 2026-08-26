//! ADR-0006 `graph` mode `open` (the interactive-UI URL), in its own test binary.
//!
//! This test asserts the DEGRADED path: with no daemon serving, the tool must fall back to the
//! `file://` export. `store::init_isolated_cache` mints one comms dir per test BINARY, so the
//! premise only holds in a process where nothing spawns a comms daemon. It used to live in
//! `mcp_smoke.rs` alongside the `agents` / `workspace` / `shell` tests, which do spawn one — and
//! whenever one of those won the race first, this test found its portfile and saw `served: true`.
//! Cargo gives each integration-test file its own process, so isolating it here restores the
//! premise instead of weakening the assertion. ~keep

use std::path::Path;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use tempfile::TempDir;

fn run_scan(root: &Path) {
    let cfg = basemind::config::default_for_root(root);
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    // `#[tokio::test]`, so run the scan on a dedicated std thread to mirror the production context.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
            basemind::scanner::scan(
                root,
                &mut store,
                &cfg,
                basemind::scanner::ScanSource::WorkingTree,
                basemind::scanner::EmbedMode::Inline,
            )
            .expect("scan");
        });
    });
}

fn decode_text(result: &CallToolResult) -> Value {
    use rmcp::model::ContentBlock;
    let raw = result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or(Value::Null)
}

fn assert_structured_matches_text(result: &CallToolResult) {
    let structured = result
        .structured_content
        .as_ref()
        .expect("tool result must carry SEP-2106 structured_content");
    assert_eq!(
        structured,
        &decode_text(result),
        "structured_content must match the JSON text mirror"
    );
}

fn call_params(name: &'static str, args: Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name);
    if let Some(obj) = args.as_object() {
        params = params.with_arguments(obj.clone());
    }
    params
}

/// ADR-0006: the `ui` tool renders the interactive graph, always writes it to the export cache, and
/// returns a URL. With no daemon serving (the isolated test comms dir has none) it degrades to the
/// `file://` export; `open: false` returns the URL without launching a viewer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_returns_url_and_writes_export_without_opening() {
    basemind::store::init_isolated_cache();
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    std::fs::write(
        root.join("core.rs"),
        "pub fn engine() {}\npub fn helper() { engine(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app.rs"),
        "use crate::core::helper;\npub fn run() { helper(); }\n",
    )
    .unwrap();
    run_scan(root);

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    // Default format html; open:false returns the URL without launching (never spawns a viewer in CI).
    let shown = service
        .call_tool(call_params("graph", json!({ "mode": "open", "open": false})))
        .await
        .expect("ui html");
    assert_structured_matches_text(&shown);
    let body = decode_text(&shown);
    assert_eq!(
        body.get("format").and_then(Value::as_str),
        None,
        "ui response carries a url/method, not a format echo: {body}"
    );
    // The isolated test comms dir has no daemon portfile, so the tool degrades to the file export.
    assert_eq!(
        body.get("served").and_then(Value::as_bool),
        Some(false),
        "no daemon serving in the isolated test env: {body}"
    );
    assert_eq!(
        body.get("method").and_then(Value::as_str),
        Some("file"),
        "degrades to the file export: {body}"
    );
    let url = body.get("url").and_then(Value::as_str).expect("url present");
    assert!(url.starts_with("file://"), "file fallback URL: {url}");
    assert!(
        body.get("node_count").and_then(Value::as_u64).unwrap_or(0) >= 3,
        "nodes: {body}"
    );
    // The product is the URL + the written file, not inline bytes.
    assert!(
        body.get("content").is_none(),
        "ui does not return rendered bytes inline: {body}"
    );
    let output_path = body
        .get("output_path")
        .and_then(Value::as_str)
        .expect("output_path always present");
    assert!(
        output_path.ends_with(".html"),
        "html export named by extension: {output_path}"
    );
    let output_name = Path::new(output_path)
        .file_name()
        .and_then(|name| name.to_str())
        .expect("export filename");
    assert!(
        url.ends_with(output_name),
        "the file:// URL points at the written export: url={url} path={output_path}"
    );
    let on_disk = std::fs::read_to_string(output_path).expect("export file exists on disk");
    assert!(
        on_disk.contains("<!doctype html>"),
        "written file is the interactive HTML page"
    );

    // Knob plumbing: `max_nodes` caps the rendered graph and the response reports the cut. The repo has
    // threads the graph-shaping knobs through `render_ui_parts`, not just the defaults.
    let capped = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "open", "max_nodes": 1, "open": false}),
        ))
        .await
        .expect("ui max_nodes=1");
    let capped = decode_text(&capped);
    assert_eq!(
        capped.get("node_count").and_then(Value::as_u64),
        Some(1),
        "max_nodes=1 caps the rendered graph to one node: {capped}"
    );
    assert_eq!(
        capped.get("truncated").and_then(Value::as_bool),
        Some(true),
        "capping a larger graph reports truncated: {capped}"
    );
    // `focus` is threaded end-to-end (param -> render): the tool accepts it and renders without error.
    let focused = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "open", "focus": "core.rs", "open": false}),
        ))
        .await
        .expect("ui focus renders");
    let focused = decode_text(&focused);
    assert!(
        focused.get("node_count").and_then(Value::as_u64).is_some(),
        "focus renders a graph carrying a node count: {focused}"
    );

    // A graph *data* format is rejected — the UI shows a picture; graph_export returns the data.
    let rejected = service
        .call_tool(call_params(
            "graph",
            json!({ "mode": "open", "format": "node_link", "open": false}),
        ))
        .await;
    assert!(
        rejected.is_err(),
        "a non-visual format must be rejected, not silently rendered"
    );

    let _ = service.cancel().await;
}
