//! The three agent tools (ADR-0021) end to end through real `basemind serve` relays: each session
//! is a child process speaking stdio MCP to the daemon that hosts it, exactly as a host connects.
//! Hermetic via `init_isolated_cache`: the daemon, its socket and its stores live in a per-process
//! tempdir, which the children inherit.
#![cfg(all(feature = "comms", any(unix, windows)))]

use std::path::Path;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::{Value, json};
use tokio::process::Command as AsyncCommand;

fn repo() -> tempfile::TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let status = basemind::testing::git_command()
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .expect("git init");
    assert!(status.success());
    std::fs::write(dir.path().join("a.rs"), b"pub fn alpha() {}\n").expect("write a.rs");
    dir
}

/// A `basemind serve` session for `root`, as `agent` when given (else the derived id).
async fn session(root: &Path, agent: Option<&str>) -> RunningService<RoleClient, ()> {
    let bin = env!("CARGO_BIN_EXE_basemind");
    let agent = agent.map(str::to_owned);
    let cmd = AsyncCommand::new(bin).configure(move |c| {
        c.arg("--root").arg(root).arg("serve");
        c.env("BASEMIND_HOST", "smoke");
        match &agent {
            Some(agent) => c.env("BASEMIND_AGENT_ID", agent),
            None => c.env_remove("BASEMIND_AGENT_ID"),
        };
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    ().serve(transport).await.expect("rmcp handshake over the relay")
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

async fn call(service: &RunningService<RoleClient, ()>, tool: &'static str, args: Value) -> Value {
    let params = CallToolRequestParams::new(tool).with_arguments(args.as_object().expect("an object").clone());
    let result = service.call_tool(params).await.expect("tool call");
    assert_ne!(result.is_error, Some(true), "`{tool}` refused: {}", text(&result));
    let reply = text(&result);
    basemind::wire::decode(&reply).unwrap_or_else(|error| panic!("`{tool}`: unreadable reply ({error}): {reply}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sessions_message_each_other_and_a_sub_identity_reads_apart() {
    let dir = repo();
    let root = dir.path();
    let alice = session(root, Some("smoke-alice")).await;
    let bob = session(root, Some("smoke-bob")).await;

    let tools = alice.list_all_tools().await.expect("list tools");
    for tool in ["agent_post", "agent_inbox", "agent_threads"] {
        assert!(tools.iter().any(|t| t.name == tool), "`{tool}` is advertised");
    }

    let posted = call(
        &alice,
        "agent_post",
        json!({"subject": "hand-off", "to": ["smoke-bob"], "body": "take src/auth"}),
    )
    .await;
    let thread = posted["thread"].as_str().expect("a thread id").to_string();

    let read = call(&bob, "agent_inbox", json!({})).await;
    assert_eq!(read["messages"][0]["body"], "take src/auth", "{read}");
    assert_eq!(read["messages"][0]["from"], "smoke-alice", "{read}");
    let again = call(&bob, "agent_inbox", json!({})).await;
    assert_eq!(
        again["messages"].as_array().map(Vec::len).unwrap_or(0),
        0,
        "reading marked read: {again}"
    );

    call(
        &alice,
        "agent_post",
        json!({"thread": thread, "body": "for the worker"}),
    )
    .await;
    let worker = call(&bob, "agent_inbox", json!({"as": "worker"})).await;
    let bodies: Vec<&str> = worker["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|m| m["body"].as_str())
        .collect();
    assert_eq!(
        bodies,
        ["for the worker"],
        "a sub-identity starts where its principal has read"
    );
    let principal = call(&bob, "agent_inbox", json!({})).await;
    assert_eq!(
        principal["messages"][0]["body"], "for the worker",
        "the sub-identity consumed nothing of bob's"
    );

    alice.cancel().await.expect("close alice");
    bob.cancel().await.expect("close bob");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_sharing_a_derived_identity_are_told_so() {
    let dir = repo();
    let root = dir.path();
    let one = session(root, None).await;
    let two = session(root, None).await;

    let reply = call(&one, "agent_threads", json!({})).await;
    let notices = reply["notices"].as_array().expect("notices");
    assert!(
        notices
            .iter()
            .any(|n| n.as_str().is_some_and(|n| n.contains("share the identity"))),
        "{reply}"
    );
    let with_as = call(&one, "agent_threads", json!({"as": "lead"})).await;
    assert_eq!(
        with_as["notices"],
        json!([]),
        "a call with `as` is told nothing: {with_as}"
    );

    one.cancel().await.expect("close one");
    two.cancel().await.expect("close two");
}
