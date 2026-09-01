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

/// One HTTP/1.1 GET over loopback, returning `(status_code, lowercased_header_block, body)`.
/// `Connection: close` lets us read to EOF without parsing `Content-Length`.
async fn http_get(addr: &str, target: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect loopback");
    let request = format!("GET {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write request");
    stream.flush().await.expect("flush");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read response");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status line in response: {text}"));
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    (status, head.to_lowercase(), body.to_string())
}

/// One HTTP/1.1 GET over loopback with an explicit `Host` header, returning the status code. Used to
/// exercise the DNS-rebinding guard: a browser page that rebinds its domain to 127.0.0.1 sends the
/// attacker's host here, not the loopback address.
async fn http_get_with_host(addr: &str, target: &str, host: &str) -> u16 {
    let mut stream = TcpStream::connect(addr).await.expect("connect loopback");
    let request = format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write request");
    stream.flush().await.expect("flush");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read response");
    let text = String::from_utf8_lossy(&raw).into_owned();
    text.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status line in response: {text}"))
}

/// Make `dir` a project root the workspace-root allow-list accepts (issue #62). `.git/` is
/// invisible to the scanner, so the fixtures' indexed content is unchanged by the init.
fn git_init(dir: &std::path::Path) {
    let status = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir)
        .status()
        .expect("run git init");
    assert!(status.success(), "git init succeeds in {dir:?}");
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
    git_init(repo.path());
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
    // Matched on the stable identity phrase, not on a whole opening sentence: the instructions are
    // prose that gets re-tuned (they were rewritten to fit the client's 2048-char ceiling), and an
    // assertion quoting a full sentence goes red on an edit that changed nothing this test is about.
    let instructions = parsed["result"]["instructions"].as_str().unwrap_or_default();
    assert!(
        instructions.contains("basemind") && instructions.contains("indexed context layer"),
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
        tools.iter().any(|tool| tool["name"] == "code"),
        "tools/list must include the 'code' domain tool: {parsed}"
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

/// ADR-0006: `GET /ui?root=<repo>` serves the self-contained interactive graph page (the browser/
/// agent-drivable twin the `ui` tool points at), and the route's error paths return the right status.
#[tokio::test]
async fn ui_route_serves_interactive_html() {
    basemind::store::init_isolated_cache();

    let comms_dir = tempfile::tempdir().expect("comms tempdir");
    let repo = tempfile::tempdir().expect("repo tempdir");
    git_init(repo.path());
    std::fs::write(
        repo.path().join("a.rs"),
        b"pub fn alpha() { beta(); }\npub fn beta() {}\n",
    )
    .expect("write source");
    let root = std::fs::canonicalize(repo.path()).expect("canonicalize repo root");

    // SAFETY: set before the server task reads it; both HTTP tests use the same "127.0.0.1:0" value,
    // so a concurrent set is benign, and each test discovers its actual port via its own portfile.
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
    // The route reads a percent-encoded root; `/` encodes to %2F.
    let encoded_root = root_str.replace('/', "%2F");
    let (status, head, body) = http_get(&addr, &format!("/ui?root={encoded_root}")).await;
    assert_eq!(status, 200, "GET /ui must return 200: {body}");
    assert!(head.contains("content-type: text/html"), "html content-type: {head}");
    assert!(
        body.contains("<!doctype html>"),
        "serves the self-contained page: {}",
        &body[..body.len().min(120)]
    );
    assert!(body.contains("id=\"c\""), "the canvas element is present");
    assert!(body.contains("id=\"search\""), "the live-search input is present");
    assert!(
        body.contains("application/json"),
        "the embedded graph-data island is present"
    );

    // A missing root is a 400 (the query is malformed), a nonexistent root is a 404. The error body
    // is plain text that names the problem, not an opaque status.
    let (status, head, body) = http_get(&addr, "/ui").await;
    assert_eq!(status, 400, "missing ?root must 400");
    assert!(
        head.contains("content-type: text/plain"),
        "the 400 is plain text: {head}"
    );
    assert!(body.contains("root"), "the 400 body names the missing root: {body}");
    let (status, _, _) = http_get(&addr, "/ui?root=%2Fno%2Fsuch%2Fpath%2Fbm-xyz").await;
    assert_eq!(status, 404, "a root that does not resolve must 404");

    // DNS-rebinding guard: a foreign `Host` (a rebound-to-127.0.0.1 attacker page) is rejected before
    // the workspace is read, even for an otherwise-valid root — 403, not 200.
    let status = http_get_with_host(&addr, &format!("/ui?root={encoded_root}"), "evil.example").await;
    assert_eq!(
        status, 403,
        "a non-loopback Host must be rejected on the loopback listener"
    );
    // A `localhost` Host is loopback and still served (regression guard for the allowlist).
    let status = http_get_with_host(&addr, &format!("/ui?root={encoded_root}"), "localhost:1234").await;
    assert_eq!(status, 200, "a loopback Host (localhost) is still served");

    // The route serves visual formats only (like the `ui` tool): a graph *data* format is a 400 whose
    // body names the rejected knob — the route and tool reject it through the same `render_ui_parts`.
    let (status, _, body) = http_get(&addr, &format!("/ui?root={encoded_root}&format=node_link")).await;
    assert_eq!(status, 400, "a graph data format is rejected by the route: {body}");
    assert!(body.contains("format"), "the 400 body names the bad format: {body}");
    // `format=svg` renders and is served with the SVG content-type (proves the format knob is plumbed
    // through to the response, not just the default HTML path).
    let (status, head, _) = http_get(&addr, &format!("/ui?root={encoded_root}&format=svg")).await;
    assert_eq!(status, 200, "svg renders");
    assert!(head.contains("image/svg+xml"), "svg content-type: {head}");

    // `/ui` is a pure read render, so it is method-agnostic: a POST carrying the same query serves the
    // same page (the body is ignored). Pins current behavior — the route has no write/side-effect path.
    let (status, _) = http_post(&addr, &format!("/ui?root={encoded_root}"), b"", &[]).await;
    assert_eq!(status, 200, "POST /ui serves the same read-only page");

    shutdown_tx.send(true).ok();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}

/// Issue #62 at the HTTP front-end: `?root=` is documented as an absolute path and must name a
/// project. A relative value is rejected BEFORE `std::fs::canonicalize` (which would otherwise
/// resolve it against the daemon's own cwd — wherever the daemon happened to be spawned), and the
/// filesystem root is refused outright. Neither may mint a workspace cache directory on the way.
#[tokio::test]
async fn mcp_root_param_must_be_absolute_and_must_be_a_project() {
    basemind::store::init_isolated_cache();

    let comms_dir = tempfile::tempdir().expect("comms tempdir");
    // SAFETY: set before the server task reads it; every HTTP test here uses the same value.
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

    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}});
    let payload = list.to_string();

    let (status, body) = http_post(&addr, "/mcp?root=relative%2Fpath", payload.as_bytes(), &json_headers()).await;
    assert_eq!(
        status, 400,
        "a relative ?root must 400, never resolve against the daemon cwd: {body}"
    );
    assert!(body.contains("absolute"), "the 400 body says why: {body}");

    let (status, body) = http_post(&addr, "/mcp?root=%2F", payload.as_bytes(), &json_headers()).await;
    assert_eq!(status, 403, "the filesystem root must be refused: {body}");
    assert!(
        body.contains("filesystem/volume root") && body.contains("no override"),
        "the refusal explains itself: {body}"
    );

    assert!(
        !basemind::store::workspace_cache_dir(std::path::Path::new("/")).exists(),
        "a refused root must not mint a workspace cache dir"
    );

    // A repo SUBDIRECTORY must attach to the repository, exactly as every CLI verb does — the CLI
    // gets that from `discover_root_with_basemind` before its guard runs, and the HTTP front-end
    // taking `?root=` verbatim made the two disagree (the guard requires `workdir == root`, so a
    // subdirectory 403'd here while working on the command line).
    let repo = tempfile::tempdir().expect("repo tempdir");
    git_init(repo.path());
    let repo_root = repo.path().canonicalize().expect("canonicalize repo");
    let nested = repo_root.join("src");
    std::fs::create_dir_all(&nested).expect("mkdir src");
    std::fs::write(nested.join("lib.rs"), "pub fn nested() {}\n").expect("write source");
    let encoded_nested = nested.to_str().expect("utf-8 path").replace('/', "%2F");
    let (status, body) = http_post(
        &addr,
        &format!("/mcp?root={encoded_nested}"),
        payload.as_bytes(),
        &json_headers(),
    )
    .await;
    assert_eq!(status, 200, "a repo subdirectory resolves to its repository: {body}");

    shutdown_tx.send(true).ok();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}
