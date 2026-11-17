//! End-to-end smoke test for the SEP-2663 Tasks extension (`io.modelcontextprotocol/tasks`).
//!
//! Builds a throwaway repo, scans it, hosts the server in-process over an in-memory duplex
//! transport, and drives the task lifecycle over the real rmcp client:
//!
//! * a task-capable client calling `index_rescan` gets its result INLINE when the call finishes
//!   within the threshold: a fast call is never wrapped in a task;
//! * the same call on a server whose threshold is zero gets a `CallToolResponse::Task`, and polling
//!   `tasks/get` settles into a `Completed` payload carrying the tool's `CallToolResult`;
//! * a plain client (no tasks extension) always keeps the synchronous path;
//! * `get_info` advertises the tasks capability.
//!
//! Modeled on rmcp's own `tests/test_task.rs`, adapted to basemind's in-process serve harness.

use std::path::Path;
use std::process::Command;

use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ClientCapabilities, ClientConfig, GetTaskParams, Implementation,
    TaskPayload,
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn build_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\npub fn beta() {}\n").unwrap();
    std::fs::write(root.join("b.rs"), b"pub fn gamma() { alpha(); }\n").unwrap();
    git(root, &["add", "a.rs", "b.rs"]);
    git(root, &["commit", "-qm", "init"]);
    dir
}

fn run_scan(root: &Path) {
    let cfg = basemind::config::default_for_root(root);
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
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

/// Serve the code map in-process over an in-memory duplex transport (the stdio child-process serve
/// was removed with the stdio transport). Returns the client end for `().serve(...)`.
async fn serve(root: &Path) -> tokio::io::DuplexStream {
    basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve")
}

/// A server on which every offload-eligible call has already outlived its threshold.
async fn serve_with_zero_threshold(root: &Path) -> tokio::io::DuplexStream {
    basemind::mcp::serve_in_memory_with_task_threshold(root, "working", std::time::Duration::ZERO)
        .await
        .expect("in-memory serve")
}

fn tasks_client() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::builder().enable_tasks().build(),
        Implementation::from_build_env(),
    )
}

fn call(tool: &'static str, arguments: Value) -> CallToolRequestParams {
    CallToolRequestParams::new(tool).with_arguments(arguments.as_object().cloned().expect("object"))
}

fn body_of(result: &rmcp::model::CallToolResult) -> Value {
    let text = result.content[0].as_text().expect("result has a text block");
    serde_json::from_str(&text.text).expect("result text is JSON")
}

/// The defect this guards: calls that took 6 to 27 ms were answered as background tasks because
/// offload was decided by tool name alone. A task-capable client must get the result itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fast_call_returns_inline_to_a_task_capable_client() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let client = tasks_client().serve(serve(root).await).await.expect("rmcp handshake");

    let caps = client
        .peer_info()
        .map(|info| info.capabilities.clone())
        .unwrap_or_default();
    assert!(
        caps.supports_tasks(),
        "get_info must advertise the SEP-2663 tasks capability: {caps:?}"
    );

    for (tool, arguments, field) in [
        ("index_rescan", json!({}), "scanned"),
        ("index_status", json!({}), "file_count"),
        ("code_symbols", json!({ "name": "alpha" }), "results"),
    ] {
        let response = client
            .call_tool_once(call(tool, arguments))
            .await
            .unwrap_or_else(|error| panic!("{tool}: {error}"));
        match response {
            CallToolResponse::Complete(result) => {
                let body = body_of(&result);
                assert!(
                    body.get(field).is_some(),
                    "{tool} must answer inline with its own result: {body}"
                );
            }
            CallToolResponse::Task(create) => panic!(
                "a `{tool}` call that finishes within the threshold must not become a task; got task {:?}",
                create.task.task_id
            ),
            other => panic!("unexpected response variant for {tool}: {other:?}"),
        }
    }

    client.cancel().await.expect("shutdown client");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_past_the_threshold_becomes_a_task_and_completes() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let client = tasks_client()
        .serve(serve_with_zero_threshold(root).await)
        .await
        .expect("rmcp handshake");

    let response = client
        .call_tool_once(call("index_rescan", json!({})))
        .await
        .expect("rescan call");
    let create = match response {
        CallToolResponse::Task(create) => create,
        other => panic!("a call still running past the threshold must return a task handle, got {other:?}"),
    };
    let task_id = create.task.task_id.clone();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let final_task = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "the task never reached a terminal state"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let info = client
            .peer()
            .get_task(GetTaskParams::new(task_id.clone()))
            .await
            .expect("tasks/get");
        if info.task.status().is_terminal() {
            break info.task;
        }
    };

    match final_task.payload {
        TaskPayload::Completed { result } => {
            let result: rmcp::model::CallToolResult =
                serde_json::from_value(Value::Object(result)).expect("completed payload is a CallToolResult");
            let body = body_of(&result);
            assert!(
                body.get("scanned").and_then(Value::as_u64).is_some(),
                "rescan task result must carry a `scanned` count: {body}"
            );
        }
        other => panic!("expected a completed task, got {other:?}"),
    }

    client.cancel().await.expect("shutdown client");
}

/// A tool that is not offload-eligible never becomes a task, whatever the threshold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_that_cannot_run_long_is_never_a_task() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let client = tasks_client()
        .serve(serve_with_zero_threshold(root).await)
        .await
        .expect("rmcp handshake");

    let response = client
        .call_tool_once(call("index_status", json!({})))
        .await
        .expect("index_status call");
    match response {
        CallToolResponse::Complete(result) => {
            let body = body_of(&result);
            assert!(body.get("file_count").and_then(Value::as_u64).is_some(), "{body}");
        }
        other => panic!("`index_status` must answer inline, got {other:?}"),
    }

    client.cancel().await.expect("shutdown client");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_without_the_capability_always_gets_the_result() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let client = ().serve(serve_with_zero_threshold(root).await).await.expect("rmcp handshake");
    let result = client
        .call_tool(call("index_rescan", json!({})))
        .await
        .expect("plain client gets a synchronous CallToolResult");
    let body = body_of(&result);
    assert!(
        body.get("scanned").and_then(Value::as_u64).is_some(),
        "synchronous rescan result must carry a `scanned` count: {body}"
    );

    client.cancel().await.expect("shutdown client");
}
