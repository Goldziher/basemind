//! End-to-end smoke test for the daemon's stateless streamable-HTTP MCP front-end.
//!
//! Builds a [`Broker`] in-process over an isolated comms store (the same construction the daemon
//! uses), spawns [`http_frontend::serve_http`] on a free loopback port, and drives the transport
//! with a tiny hand-rolled HTTP/1.1 client (no `reqwest` — feature unification pulls in a rustls
//! provider that panics on a plain client, and a raw loopback POST is dependency-free): a JSON-RPC
//! `initialize` and `tools/list` over `POST /mcp?root=<repo>&agent=<id>`, asserting `200` + the
//! expected server identity and tool list. Also checks that a non-`/mcp` path and a missing `root`
//! both 404, and that a request with no bearer token or the wrong one is refused. Uses an
//! isolated `BASEMIND_DATA_HOME` (via `init_isolated_cache`) and a `127.0.0.1:0` port so it never
//! collides with a real daemon or a sibling test.
//!
//! The listener is opt-in, so every test here sets the grant. The *disabled* case cannot live in
//! this binary — env vars are process-global and these tests run in parallel — so it has its own:
//! `tests/http_frontend_disabled_smoke.rs`.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::sync::Arc;
use std::time::Duration;

use basemind::comms::daemon::Broker;
use basemind::comms::http_frontend;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The filesystem/volume root as this platform expresses it: `/` on unix, the system volume
/// (`D:/` on a GitHub Windows runner) on Windows. Unix-style `/` is *relative* on Windows, so a
/// hard-coded `/` would be refused as a non-absolute path (400) instead of exercising the
/// filesystem-root guard (403).
fn filesystem_root() -> std::path::PathBuf {
    std::env::current_dir()
        .expect("current dir")
        .ancestors()
        .last()
        .expect("filesystem root")
        .to_path_buf()
}

/// Percent-encode a path for a `?root=` query value, normalising separators so Windows
/// backslashes are emitted as `/` rather than needing their own escape.
fn encode_root(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/").replace('/', "%2F")
}

/// One HTTP/1.1 POST over loopback, returning `(status_code, body)`. `Connection: close` lets us
/// read the body to EOF without parsing `Content-Length`.
async fn http_post(addr: &str, target: &str, body: &[u8], extra_headers: &[(&str, &str)]) -> (u16, String) {
    http_post_with_host(addr, addr, target, body, extra_headers)
        .await
        .unwrap_or_else(|error| panic!("read response: {error}"))
}

/// One HTTP/1.1 POST with an explicit `Host` header, the headers and the body written separately as
/// a client streaming its body does. An I/O error is returned, not retried: a connection reset here
/// is a refusal the server failed to deliver.
async fn http_post_with_host(
    addr: &str,
    host: &str,
    target: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) -> std::io::Result<(u16, String)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut request = format!(
        "POST {target} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status line in response: {text}"));
    let body = text.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
    Ok((status, body))
}

/// One HTTP/1.1 GET over loopback with an explicit `Host` header and an optional `Authorization`
/// value, returning `(status_code, lowercased_header_block, body)`. `Connection: close` lets us read
/// to EOF without parsing `Content-Length`. The explicit host also exercises the DNS-rebinding guard:
/// a browser page that rebinds its domain to 127.0.0.1 sends the attacker's host here, not the
/// loopback address.
async fn http_get_with_host(addr: &str, target: &str, host: &str, bearer: Option<&str>) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect loopback");
    let authorization = bearer.map_or_else(String::new, |value| format!("Authorization: {value}\r\n"));
    let request = format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n{authorization}Connection: close\r\n\r\n");
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

/// One HTTP/1.1 GET over loopback, addressed to the listener's own address.
async fn http_get(addr: &str, target: &str, bearer: Option<&str>) -> (u16, String, String) {
    http_get_with_host(addr, target, addr, bearer).await
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

/// The JSON-RPC content headers plus the bearer credential. Borrowed from a caller-held
/// `format!("Bearer {token}")` so the header slice stays `&str`-shaped.
fn json_headers(bearer: &str) -> Vec<(&str, &str)> {
    vec![
        ("content-type", "application/json"),
        ("accept", "application/json, text/event-stream"),
        ("authorization", bearer),
    ]
}

/// Grant the opt-in listener and pin it to a free loopback port.
///
/// Every test in this binary sets the same two values, so the parallel writes are benign — and the
/// actual port each test bound is discovered from its own portfile, never from the env.
fn grant_http_listener() {
    // SAFETY: every test in this binary writes these same values; no reader races a differing write.
    unsafe {
        std::env::set_var(http_frontend::ALLOW_HTTP_ENV, "1");
        std::env::set_var(http_frontend::HTTP_ADDR_ENV, "127.0.0.1:0");
    }
}

/// A running front-end plus everything a test needs to talk to it and shut it down.
struct ServedHttp {
    addr: String,
    token: String,
    shutdown: tokio::sync::watch::Sender<bool>,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    _comms_dir: tempfile::TempDir,
}

impl ServedHttp {
    async fn stop(self) {
        self.shutdown.send(true).ok();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.server).await;
    }
}

/// Spawn the front-end over an isolated comms store and wait for it to answer.
async fn serve() -> ServedHttp {
    grant_http_listener();
    let comms_dir = tempfile::tempdir().expect("comms tempdir");
    let broker = Arc::new(Broker::new(basemind::comms::agents_hub::AgentHub::open(
        comms_dir.path(),
    )));

    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let broker_for_http = broker.clone();
    let comms_path = comms_dir.path().to_path_buf();
    let server = tokio::spawn(async move { http_frontend::serve_http(broker_for_http, comms_path, shutdown_rx).await });

    let addr = http_frontend::await_http_ready(comms_dir.path(), Duration::from_secs(10))
        .await
        .expect("streamable-HTTP transport ready");
    let token = http_frontend::published_token(comms_dir.path()).expect("the portfile publishes the bearer token");

    ServedHttp {
        addr,
        token,
        shutdown,
        server,
        _comms_dir: comms_dir,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamable_http_serves_initialize_and_tools_list() {
    basemind::store::init_isolated_cache();

    let repo = tempfile::tempdir().expect("repo tempdir");
    git_init(repo.path());
    std::fs::write(repo.path().join("a.rs"), b"pub fn alpha() {}\n").expect("write source");
    let root = std::fs::canonicalize(repo.path()).expect("canonicalize repo root");

    let served = serve().await;
    let addr = served.addr.clone();
    let bearer = format!("Bearer {}", served.token);
    let headers = json_headers(&bearer);

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
    let (status, body) = http_post(&addr, &target, init.to_string().as_bytes(), &headers).await;
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
    let (status, body) = http_post(&addr, &target, list.to_string().as_bytes(), &headers).await;
    assert_eq!(status, 200, "tools/list must return 200: {body}");
    let parsed: Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("tools/list json ({e}): {body}"));
    let tools = parsed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools array present: {parsed}"));
    assert!(
        tools.iter().any(|tool| tool["name"] == "code_outline"),
        "tools/list must include the `code_outline` tool: {parsed}"
    );
    assert!(
        tools.len() > 5,
        "tools/list must expose the full tool surface, got {}",
        tools.len()
    );
    for agent_tool in [
        "agent_post",
        "agent_inbox",
        "agent_threads",
        "agent_launch",
        "agent_stop",
    ] {
        assert!(
            !tools.iter().any(|tool| tool["name"] == agent_tool),
            "agent messaging is not served over HTTP (ADR-0021 rule 17), so `{agent_tool}` is not listed: {parsed}"
        );
    }

    // --- the CLI-only commands are not on the HTTP surface: only a CLI's relay hello reaches them ---
    // --- nor the lead tools, which only `serve --lead` answers, on its own stdio (ADR-0040) ---
    for (id, name) in [
        "memory_delete",
        "git_churn",
        "git_by_path",
        "code_chunk",
        "code_impact",
        "code_cycles",
        "code_migration_check",
        "overview",
        "git_hotspots",
        "code_explain",
        "agent_launch",
        "agent_stop",
    ]
    .into_iter()
    .enumerate()
    {
        let call = json!({
            "jsonrpc": "2.0",
            "id": 10 + id,
            "method": "tools/call",
            "params": { "name": name, "arguments": {} }
        });
        let (status, body) = http_post(&addr, &target, call.to_string().as_bytes(), &headers).await;
        assert_eq!(status, 200, "{name}: {body}");
        let parsed: Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{name} json ({e}): {body}"));
        let message = parsed["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("tool not found"),
            "{name} must be unknown over HTTP: {parsed}"
        );
    }

    // --- routing: a non-/mcp path is a 404 ---
    let (status, _) = http_post(&addr, "/nope", b"{}", &headers).await;
    assert_eq!(status, 404, "unknown path must 404");

    // --- routing: a missing root is a 404 ---
    let (status, _) = http_post(&addr, "/mcp", list.to_string().as_bytes(), &headers).await;
    assert_eq!(status, 404, "missing ?root must 404");

    served.stop().await;
}

/// The bearer gate. Reaching the port proves only that a local process opened
/// a socket, and behind it sits the full tool surface, so an
/// unauthenticated request must be refused before any routing or workspace work happens.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_request_requires_the_daemon_bearer_token() {
    basemind::store::init_isolated_cache();

    let repo = tempfile::tempdir().expect("repo tempdir");
    git_init(repo.path());
    std::fs::write(
        repo.path().join("a.rs"),
        b"pub fn alpha() { beta(); }\npub fn beta() {}\n",
    )
    .expect("write source");
    let root = std::fs::canonicalize(repo.path()).expect("canonicalize repo root");
    let encoded_root = root.to_str().expect("utf-8 repo path").replace('/', "%2F");

    let served = serve().await;
    let addr = served.addr.clone();
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}).to_string();
    let content_headers: [(&str, &str); 2] = [
        ("content-type", "application/json"),
        ("accept", "application/json, text/event-stream"),
    ];

    // --- /mcp: no credential, then a wrong one ---
    let mcp_target = format!("/mcp?root={encoded_root}&agent=smoke");
    let (status, body) = http_post(&addr, &mcp_target, list.as_bytes(), &content_headers).await;
    assert_eq!(status, 401, "POST /mcp with no token must be refused: {body}");
    assert!(
        body.contains("bearer token"),
        "the 401 names the credential it wants: {body}"
    );
    let wrong = format!("Bearer {}", "0".repeat(served.token.len()));
    let (status, body) = http_post(&addr, &mcp_target, list.as_bytes(), &json_headers(&wrong)).await;
    assert_eq!(status, 401, "POST /mcp with a wrong token must be refused: {body}");
    let (status, body) = http_post(
        &addr,
        &format!("{mcp_target}&token={}", "0".repeat(served.token.len())),
        list.as_bytes(),
        &content_headers,
    )
    .await;
    assert_eq!(status, 401, "a wrong ?token= is refused: {body}");

    // --- /mcp: the right credential is accepted in the header only ---
    let bearer = format!("Bearer {}", served.token);
    let (status, body) = http_post(&addr, &mcp_target, list.as_bytes(), &json_headers(&bearer)).await;
    assert_eq!(status, 200, "POST /mcp with the right token is served: {body}");
    let (status, body) = http_post(
        &addr,
        &format!("{mcp_target}&token={}", served.token),
        list.as_bytes(),
        &content_headers,
    )
    .await;
    assert_eq!(
        status, 401,
        "the token is not accepted in the query string, even the right one: {body}"
    );

    // The gate runs BEFORE routing: an unknown path is 401, not 404, so an unauthenticated caller
    // cannot probe the route table or the workspace guard.
    let (status, _) = http_post(&addr, "/nope", b"{}", &content_headers).await;
    assert_eq!(status, 401, "an unknown path is refused before it is routed");
    let (status, head, _) = http_get(&addr, "/ui", None).await;
    assert_eq!(status, 401, "an unauthenticated GET is refused before it is routed");
    assert!(
        head.contains("www-authenticate: bearer"),
        "the 401 advertises the scheme: {head}"
    );

    served.stop().await;
}

/// `/mcp` is the only route. The removed `/ui` page is a 404 for a credentialed caller, and the
/// DNS-rebinding guard refuses a foreign `Host` before the workspace is read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_mcp_is_routed_and_a_foreign_host_is_refused() {
    basemind::store::init_isolated_cache();

    let repo = tempfile::tempdir().expect("repo tempdir");
    git_init(repo.path());
    std::fs::write(repo.path().join("a.rs"), b"pub fn alpha() {}\n").expect("write source");
    let root = std::fs::canonicalize(repo.path()).expect("canonicalize repo root");
    let encoded_root = encode_root(&root);

    let served = serve().await;
    let addr = served.addr.clone();
    let bearer = format!("Bearer {}", served.token);

    let (status, head, body) = http_get(&addr, &format!("/ui?root={encoded_root}"), Some(&bearer)).await;
    assert_eq!(status, 404, "the /ui route is gone: {body}");
    assert!(
        head.contains("content-type: text/plain"),
        "the 404 is plain text: {head}"
    );
    assert!(body.contains("/mcp only"), "the 404 names the one route: {body}");

    let mcp_target = format!("/mcp?root={encoded_root}");
    let (status, _, _) = http_get_with_host(&addr, &mcp_target, "evil.example", Some(&bearer)).await;
    assert_eq!(
        status, 403,
        "a non-loopback Host must be rejected on the loopback listener"
    );
    let (status, _, body) = http_get_with_host(&addr, &mcp_target, "localhost:1234", Some(&bearer)).await;
    assert!(
        status != 403 && status != 401,
        "a loopback Host (localhost) passes the guard, got {status}: {body}"
    );

    served.stop().await;
}

/// Issue #62 at the HTTP front-end: `?root=` is documented as an absolute path and must name a
/// project. A relative value is rejected BEFORE `std::fs::canonicalize` (which would otherwise
/// resolve it against the daemon's own cwd — wherever the daemon happened to be spawned), and the
/// filesystem root is refused outright. Neither may mint a workspace cache directory on the way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_root_param_must_be_absolute_and_must_be_a_project() {
    basemind::store::init_isolated_cache();

    let served = serve().await;
    let addr = served.addr.clone();
    let bearer = format!("Bearer {}", served.token);
    let headers = json_headers(&bearer);

    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}});
    let payload = list.to_string();

    let (status, body) = http_post(&addr, "/mcp?root=relative%2Fpath", payload.as_bytes(), &headers).await;
    assert_eq!(
        status, 400,
        "a relative ?root must 400, never resolve against the daemon cwd: {body}"
    );
    assert!(body.contains("absolute"), "the 400 body says why: {body}");

    let fs_root = filesystem_root();
    let (status, body) = http_post(
        &addr,
        &format!("/mcp?root={}", encode_root(&fs_root)),
        payload.as_bytes(),
        &headers,
    )
    .await;
    assert_eq!(status, 403, "the filesystem root must be refused: {body}");
    assert!(
        body.contains("filesystem/volume root") && body.contains("no override"),
        "the refusal explains itself: {body}"
    );

    assert!(
        !basemind::store::workspace_cache_dir(&fs_root).exists(),
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
        &headers,
    )
    .await;
    assert_eq!(status, 200, "a repo subdirectory resolves to its repository: {body}");

    served.stop().await;
}

/// A refusal must reach the client however the request body arrives. The front-end answers 403, 404
/// and 400 before anything reads the body, and closing a socket with unread bytes in its receive
/// buffer sends RST instead of FIN: the client's read then fails with `ECONNRESET` and the status is
/// lost. Under load the body's segment often lands after the headers were parsed, which is how this
/// surfaced as a flaky `connection reset` in the smoke above. A body larger than hyper's first read
/// leaves bytes unread every time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_is_delivered_while_the_request_body_is_still_arriving() {
    basemind::store::init_isolated_cache();

    let served = serve().await;
    let addr = served.addr.clone();
    let bearer = format!("Bearer {}", served.token);
    let headers = json_headers(&bearer);
    let body = vec![b' '; 32 * 1024];

    for (target, expected) in [
        ("/nope".to_string(), 404),
        ("/mcp".to_string(), 404),
        ("/mcp?root=relative%2Fpath".to_string(), 400),
        (format!("/mcp?root={}", encode_root(&filesystem_root())), 403),
    ] {
        let (status, _) = http_post_with_host(&addr, &addr, &target, &body, &headers)
            .await
            .unwrap_or_else(|error| panic!("POST {target}: the refusal was lost: {error}"));
        assert_eq!(status, expected, "POST {target}");
    }
    let (status, _) = http_post_with_host(&addr, "evil.example", "/mcp?root=%2Ftmp", &body, &headers)
        .await
        .unwrap_or_else(|error| panic!("POST with a foreign Host: the refusal was lost: {error}"));
    assert_eq!(status, 403, "a foreign Host is refused");

    served.stop().await;
}
