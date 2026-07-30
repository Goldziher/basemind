//! End-to-end smoke test for the daemon's stateless streamable-HTTP MCP front-end.
//!
//! Builds a [`Broker`] in-process over an isolated comms store (the same construction the daemon
//! uses), spawns [`http_frontend::serve_http`] on a free loopback port, and drives the transport
//! with a tiny hand-rolled HTTP/1.1 client (no `reqwest` — feature unification pulls in a rustls
//! provider that panics on a plain client, and a raw loopback POST is dependency-free): a JSON-RPC
//! `initialize` and `tools/list` over `POST /mcp?root=<repo>&agent=<id>`, asserting `200` + the
//! expected server identity and tool list. Also checks that a non-`/mcp` path and a missing `root`
//! both 404. Uses an isolated `BASEMIND_DATA_HOME` (via `init_isolated_cache`) and a `127.0.0.1:0`
//! port so it never collides with a real daemon or a sibling test.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::sync::Arc;
use std::time::Duration;

use basemind::comms::daemon::Broker;
use basemind::comms::http_frontend;
use basemind::comms::store::CommsStore;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// One HTTP/1.1 POST over loopback, returning `(status_code, body)`. `Connection: close` lets us
/// read the body to EOF without parsing `Content-Length`.
async fn http_post(addr: &str, target: &str, body: &[u8], extra_headers: &[(&str, &str)]) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect loopback");
    let mut request = format!(
        "POST {target} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.expect("write request head");
    stream.write_all(body).await.expect("write request body");
    stream.flush().await.expect("flush");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read response");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status line in response: {text}"));
    let body = text.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
    (status, body)
}

fn json_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("content-type", "application/json"),
        ("accept", "application/json, text/event-stream"),
    ]
}

#[tokio::test]
async fn streamable_http_serves_initialize_and_tools_list() {
    basemind::store::init_isolated_cache();

    let comms_dir = tempfile::tempdir().expect("comms tempdir");
    let repo = tempfile::tempdir().expect("repo tempdir");
    std::fs::write(repo.path().join("a.rs"), b"pub fn alpha() {}\n").expect("write source");
    let root = std::fs::canonicalize(repo.path()).expect("canonicalize repo root");

    // Free loopback port so the test never collides with a real daemon or a sibling test.
    // SAFETY: set before the server task reads it; no other thread races this in-test.
    unsafe { std::env::set_var(http_frontend::HTTP_ADDR_ENV, "127.0.0.1:0") };

    let store = Arc::new(CommsStore::open(comms_dir.path()).expect("open comms store"));
    let broker = Arc::new(Broker::new(store));

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let broker_for_http = broker.clone();
    let comms_path = comms_dir.path().to_path_buf();
    let server = tokio::spawn(async move { http_frontend::serve_http(broker_for_http, comms_path, shutdown_rx).await });

    let addr = http_frontend::await_http_ready(comms_dir.path(), Duration::from_secs(10))
        .await
        .expect("streamable-HTTP transport ready");

    let root_str = root.to_str().expect("utf-8 repo path");
    let target = format!("/mcp?root={root_str}&agent=smoke");

    // --- initialize ---
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2026-07-28",
            "capabilities": {},
            "clientInfo": {"name": "http-smoke", "version": "0"}
        }
    });
    let (status, body) = http_post(&addr, &target, init.to_string().as_bytes(), &json_headers()).await;
    assert_eq!(status, 200, "initialize must return 200: {body}");
    let parsed: Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("initialize json ({e}): {body}"));
    assert!(
        parsed["result"]["serverInfo"]["name"].is_string(),
        "initialize result must carry serverInfo: {parsed}"
    );
    assert!(
        parsed["result"]["instructions"]
            .as_str()
            .unwrap_or_default()
            .contains("basemind is the indexed context layer"),
        "initialize instructions must identify basemind: {parsed}"
    );

    // --- tools/list --- (stateless: no session, no MCP-Protocol-Version header so the request is
    // served on the default lifecycle without SEP-2243 standard-header enforcement)
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}});
    let (status, body) = http_post(&addr, &target, list.to_string().as_bytes(), &json_headers()).await;
    assert_eq!(status, 200, "tools/list must return 200: {body}");
    let parsed: Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("tools/list json ({e}): {body}"));
    let tools = parsed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools array present: {parsed}"));
    assert!(
        tools.iter().any(|tool| tool["name"] == "outline"),
        "tools/list must include the 'outline' code-map tool: {parsed}"
    );
    assert!(
        tools.len() > 5,
        "tools/list must expose the full tool surface, got {}",
        tools.len()
    );

    // --- routing: a non-/mcp path is a 404 ---
    let (status, _) = http_post(&addr, "/nope", b"{}", &json_headers()).await;
    assert_eq!(status, 404, "unknown path must 404");

    // --- routing: a missing root is a 404 ---
    let (status, _) = http_post(&addr, "/mcp", list.to_string().as_bytes(), &json_headers()).await;
    assert_eq!(status, 404, "missing ?root must 404");

    shutdown_tx.send(true).ok();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}
