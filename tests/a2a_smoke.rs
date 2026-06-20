//! End-to-end smoke test for the A2A server (`basemind a2a serve`).
//!
//! Spawns the real binary on a loopback port and drives it over actual TCP with a
//! dependency-free raw HTTP/1.1 client (each request sets `Connection: close`, so
//! the response is simply read to EOF). Covers the live serve path that the
//! in-process router unit tests (`src/a2a/server.rs`) cannot: real bind, the full
//! tower stack, bearer auth enforcement, and the public agent card.
//!
//! Gated on `--features a2a`; runs under the `--features full` CI matrix.
#![cfg(feature = "a2a")]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TOKEN: &str = "smoke-test-token";

/// Kills the spawned server on drop so a failing assertion never leaks a process.
struct ServerGuard {
    child: Child,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Grab a free loopback port by binding to `:0` and immediately releasing it.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Spawn `basemind a2a serve --addr 127.0.0.1:<port> --token <TOKEN>` and wait
/// until the port accepts connections.
fn spawn_server(port: u16) -> ServerGuard {
    let child = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args([
            "a2a",
            "serve",
            "--addr",
            &format!("127.0.0.1:{port}"),
            "--token",
            TOKEN,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn basemind a2a serve");

    let guard = ServerGuard { child };
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return guard;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("A2A server did not start listening on port {port} within 20s");
}

/// Issue one raw HTTP/1.1 request and return `(status_code, body)`.
fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to server");
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    for (key, value) in headers {
        request.push_str(&format!("{key}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    request.push_str(body);

    stream.write_all(request.as_bytes()).expect("write request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read response");

    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("parse HTTP status line");
    (status, body.to_owned())
}

const JSON: &[(&str, &str)] = &[("Content-Type", "application/json")];

fn message_send_body() -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/send",
        "params": {
            "message": {
                "messageId": "",
                "role": "user",
                "parts": [{"kind": "text", "text": "hi"}]
            }
        }
    })
    .to_string()
}

#[test]
fn a2a_serve_enforces_auth_and_serves_public_card() {
    let port = free_port();
    let _server = spawn_server(port);

    // 1. The agent card is public (no token) and advertises the bearer scheme.
    let (status, body) = http(port, "GET", "/.well-known/agent-card.json", &[], "");
    assert_eq!(status, 200, "agent card must be public: {body}");
    let card: serde_json::Value = serde_json::from_str(&body).expect("card is JSON");
    assert_eq!(card["name"], serde_json::json!("basemind"));
    assert_eq!(
        card["securitySchemes"]["bearer"]["scheme"],
        serde_json::json!("bearer"),
        "auth-on card must advertise the bearer scheme"
    );

    // 2. A protected call without a token is rejected.
    let (status, _) = http(port, "POST", "/", JSON, &message_send_body());
    assert_eq!(status, 401, "unauthenticated call must be rejected");

    // 3. The same call with the correct bearer token succeeds and creates a task.
    let authed: Vec<(&str, &str)> = vec![
        ("Content-Type", "application/json"),
        ("Authorization", "Bearer smoke-test-token"),
    ];
    let (status, body) = http(port, "POST", "/", &authed, &message_send_body());
    assert_eq!(status, 200, "authenticated call must succeed: {body}");
    let resp: serde_json::Value = serde_json::from_str(&body).expect("response is JSON");
    assert_eq!(resp["result"]["kind"], serde_json::json!("task"));

    // 4. A wrong token is rejected even though the route exists.
    let wrong: Vec<(&str, &str)> = vec![
        ("Content-Type", "application/json"),
        ("Authorization", "Bearer wrong"),
    ];
    let (status, _) = http(port, "POST", "/", &wrong, &message_send_body());
    assert_eq!(status, 401, "wrong token must be rejected");
}

/// Drive `message/send` and return the created task id.
fn create_task(port: u16) -> String {
    let authed: Vec<(&str, &str)> = vec![
        ("Content-Type", "application/json"),
        ("Authorization", "Bearer smoke-test-token"),
    ];
    let (status, body) = http(port, "POST", "/", &authed, &message_send_body());
    assert_eq!(status, 200, "message/send must succeed: {body}");
    let resp: serde_json::Value = serde_json::from_str(&body).expect("task response is JSON");
    resp["result"]["id"]
        .as_str()
        .expect("task result must carry an id")
        .to_owned()
}

/// Issue a `tasks/pushNotificationConfig/set` for `task_id` pointing at `url`.
fn set_push_config(port: u16, task_id: &str, url: &str) -> serde_json::Value {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tasks/pushNotificationConfig/set",
        "params": {
            "taskId": task_id,
            "pushNotificationConfig": { "url": url, "token": "corr-token" }
        }
    })
    .to_string();
    let authed: Vec<(&str, &str)> = vec![
        ("Content-Type", "application/json"),
        ("Authorization", "Bearer smoke-test-token"),
    ];
    let (status, body) = http(port, "POST", "/", &authed, &payload);
    assert_eq!(status, 200, "push-config set HTTP status: {body}");
    serde_json::from_str(&body).expect("push-config set response is JSON")
}

/// A loopback webhook target is private/loopback, which the delivery-time SSRF
/// guard BLOCKS. Registering it (the host is a hostname `127.0.0.1` IP literal,
/// rejected at create time) and/or driving a task transition must NOT result in
/// a POST to the listener. We assert the negative: no connection arrives.
///
/// We do not weaken the SSRF guard to let loopback through; the happy-path HTTP
/// shape (headers + body + 2xx/4xx handling) is proven at unit level inside
/// `src/a2a/core/webhook.rs`'s `#[cfg(test)]` module against a loopback listener
/// by calling the request builder directly.
#[test]
fn a2a_loopback_webhook_is_refused_no_post_delivered() {
    let port = free_port();
    let _server = spawn_server(port);

    // Stand up a loopback listener that would record a POST if one arrived.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind webhook listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let webhook_addr = listener.local_addr().expect("listener addr");
    let webhook_url = format!("http://{webhook_addr}/webhook");

    let task_id = create_task(port);

    // `127.0.0.1` is an IP literal in the blocked loopback range, so create-time
    // SSRF validation rejects it outright — assert the JSON-RPC error.
    let resp = set_push_config(port, &task_id, &webhook_url);
    assert!(
        resp.get("error").is_some(),
        "loopback webhook must be rejected at create time: {resp}",
    );

    // Give any (erroneous) delivery attempt a window to connect, then assert the
    // listener never accepted a connection.
    std::thread::sleep(Duration::from_millis(300));
    let accepted = listener.accept();
    assert!(
        matches!(&accepted, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "no POST must be delivered to a loopback webhook, but a connection arrived: {accepted:?}",
    );
}

/// Create-time SSRF rejection: registering a webhook pointing at a cloud
/// metadata endpoint or a private-range IP must fail with a JSON-RPC error.
#[test]
fn a2a_push_config_rejects_ssrf_targets() {
    let port = free_port();
    let _server = spawn_server(port);
    let task_id = create_task(port);

    for url in ["http://169.254.169.254/", "http://10.0.0.1/"] {
        let resp = set_push_config(port, &task_id, url);
        assert!(
            resp.get("error").is_some(),
            "SSRF target {url} must be rejected at create time: {resp}",
        );
        // JSON-RPC invalid-params (-32602) is the mapped code for InvalidInput.
        assert_eq!(
            resp["error"]["code"],
            serde_json::json!(-32602),
            "SSRF rejection must map to invalid-params for {url}: {resp}",
        );
    }
}

#[test]
fn a2a_serve_refuses_public_bind_without_token() {
    // Binding a non-loopback interface without auth must fail fast (bind-safety),
    // not silently expose an unauthenticated server.
    let mut child = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["a2a", "serve", "--addr", "0.0.0.0:0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn basemind a2a serve");

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("public bind without a token should have exited, but it kept running");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        !status.success(),
        "public bind without a token must exit non-zero"
    );
}
