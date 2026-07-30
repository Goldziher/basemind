//! End-to-end smoke test for the SEP-2663 Tasks extension (`io.modelcontextprotocol/tasks`).
//!
//! Builds a throwaway repo, scans it, hosts the server in-process over an in-memory duplex
//! transport, and drives the task lifecycle over the real rmcp client:
//!
//! * a task-capable client calling a slow tool (`rescan`) gets a `CallToolResponse::Task`, and
//!   polling `tasks/get` settles into a `Completed` payload carrying the tool's `CallToolResult`;
//! * a plain client (no tasks extension) calling the same tool keeps the synchronous path and gets
//!   an ordinary `CallToolResult`;
//! * `get_info` advertises the tasks capability.
//!
//! Modeled on rmcp's own `tests/test_task.rs`, adapted to basemind's in-process serve harness.

use std::path::Path;
use std::process::Command;

use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ClientCapabilities, ClientInfo, GetTaskParams, Implementation, TaskPayload,
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

fn tasks_client() -> ClientInfo {
    ClientInfo::new(
        ClientCapabilities::builder().enable_tasks().build(),
        Implementation::from_build_env(),
    )
}

/// A task-capable client calling a slow tool (`rescan`) is answered with a task handle; polling
/// `tasks/get` settles into a `Completed` payload carrying the tool's real `CallToolResult`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_tool_offloads_to_task_and_completes() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let client = tasks_client().serve(serve(root).await).await.expect("rmcp handshake");

    // The server must advertise the tasks extension so a client knows to poll.
    let caps = client
        .peer_info()
        .map(|info| info.capabilities.clone())
        .unwrap_or_default();
    assert!(
        caps.supports_tasks(),
        "get_info must advertise the SEP-2663 tasks capability: {caps:?}"
    );

    let response = client
        .call_tool_once(CallToolRequestParams::new("rescan").with_arguments(json!({}).as_object().cloned().unwrap()))
        .await
        .expect("rescan call");
    let create = match response {
        CallToolResponse::Task(create) => create,
        other => panic!("task-capable client must get a task handle for a slow tool, got {other:?}"),
    };
    let task_id = create.task.task_id.clone();

    // Poll until terminal, then assert the completed payload is the tool's CallToolResult.
    let final_task = loop {
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
            let text = result.content[0].as_text().expect("rescan result has a text block");
            let body: Value = serde_json::from_str(&text.text).expect("rescan text is JSON");
            assert!(
                body.get("scanned").and_then(Value::as_u64).is_some(),
                "rescan task result must carry a `scanned` count: {body}"
            );
        }
        other => panic!("expected a completed task, got {other:?}"),
    }

    client.cancel().await.expect("shutdown client");
}

/// A plain client that did not declare the tasks extension keeps the synchronous path: the same
/// slow tool returns an ordinary `CallToolResult`, never a task handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_tool_stays_synchronous_without_capability() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let client = ().serve(serve(root).await).await.expect("rmcp handshake");
    let result = client
        .call_tool(CallToolRequestParams::new("rescan").with_arguments(json!({}).as_object().cloned().unwrap()))
        .await
        .expect("synchronous rescan");
    let text = result.content[0].as_text().expect("rescan result has a text block");
    let body: Value = serde_json::from_str(&text.text).expect("rescan text is JSON");
    assert!(
        body.get("scanned").and_then(Value::as_u64).is_some(),
        "non-task client must get the tool's result directly: {body}"
    );

    client.cancel().await.expect("shutdown client");
}
