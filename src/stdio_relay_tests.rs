//! Unit tests for the stdio relay: replay, restart errors and reconnect handling.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::*;

#[test]
fn restart_errors_preserve_json_rpc_ids_and_exclude_completed_requests() {
    let mut state = SessionState::default();
    state.observe_client("{\"jsonrpc\":\"2.0\",\"id\":\"done\",\"method\":\"tools/call\"}\n");
    state.observe_client("{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\"}\n");
    state.observe_backend("{\"jsonrpc\":\"2.0\",\"id\":\"done\",\"result\":{}}\n");
    state.observe_backend("{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"roots/list\"}\n");

    let errors = state.drain_restart_errors();

    assert_eq!(errors.len(), 1);
    let error: Value = serde_json::from_str(&errors[0]).expect("valid restart error");
    assert_eq!(error["id"], serde_json::json!(7));
    assert_eq!(error["error"]["message"], "backend_restarted");
    assert_eq!(error["error"]["data"]["retryable"], true);
}

#[tokio::test]
async fn reconnect_replays_initialization_and_keeps_client_transport_open() {
    let (client_input, proxy_input) = tokio::io::duplex(4096);
    let (proxy_output, client_output) = tokio::io::duplex(4096);
    let (proxy_backend_one, backend_one) = tokio::io::duplex(4096);
    let (proxy_backend_two, backend_two) = tokio::io::duplex(4096);
    let replacements = Arc::new(Mutex::new(VecDeque::from([proxy_backend_two])));
    let connector_replacements = Arc::clone(&replacements);

    let proxy = tokio::spawn(run(proxy_input, proxy_output, proxy_backend_one, move || {
        let replacement = connector_replacements.lock().expect("replacement lock").pop_front();
        std::future::ready(replacement.ok_or("no replacement backend"))
    }));
    let first_backend = tokio::spawn(fake_first_backend(backend_one));
    let second_backend = tokio::spawn(fake_second_backend(backend_two));

    let mut client_write = client_input;
    let mut client_read = BufReader::new(client_output);
    client_write
        .write_all(initialize_line().as_bytes())
        .await
        .expect("send initialize");
    let mut line = String::new();
    client_read
        .read_line(&mut line)
        .await
        .expect("read initialize response");
    assert_eq!(response_id(&line), serde_json::json!(1));
    client_write
        .write_all(initialized_line().as_bytes())
        .await
        .expect("send initialized");
    client_write
        .write_all(call_line(2, "shell", "run").as_bytes())
        .await
        .expect("send interrupted request");

    line.clear();
    client_read.read_line(&mut line).await.expect("read restart error");
    let restart: Value = serde_json::from_str(&line).expect("restart JSON");
    assert_eq!(restart["id"], serde_json::json!(2));
    assert_eq!(restart["error"]["message"], "backend_restarted");

    line.clear();
    client_read
        .read_line(&mut line)
        .await
        .expect("read buffered notification");
    let notification: Value = serde_json::from_str(&line).expect("notification JSON");
    assert_eq!(notification["method"], "notifications/progress");

    client_write
        .write_all(request_line(3).as_bytes())
        .await
        .expect("send request after restart");
    line.clear();
    client_read.read_line(&mut line).await.expect("read recovered response");
    assert_eq!(response_id(&line), serde_json::json!(3));

    drop(client_write);
    proxy.await.expect("proxy join").expect("proxy result");
    first_backend.await.expect("first backend join");
    second_backend.await.expect("second backend join");
}

/// A backend that swallows a request must not hang the host: the relay answers with a retryable
/// `backend_timeout` and drops the backend's late reply so the host never sees two responses.
#[tokio::test(start_paused = true)]
async fn unanswered_request_gets_a_timeout_error_and_the_late_reply_is_dropped() {
    let (client_input, proxy_input) = tokio::io::duplex(4096);
    let (proxy_output, client_output) = tokio::io::duplex(4096);
    let (proxy_backend, backend) = tokio::io::duplex(4096);
    let proxy = tokio::spawn(run_with_deadline(
        proxy_input,
        proxy_output,
        proxy_backend,
        || std::future::ready(Err::<tokio::io::DuplexStream, _>("no replacement")),
        Some(Duration::from_secs(30)),
    ));
    let (backend_read, mut backend_write) = tokio::io::split(backend);
    let mut backend_read = BufReader::new(backend_read);

    let mut client_write = client_input;
    let mut client_read = BufReader::new(client_output);
    client_write.write_all(request_line(5).as_bytes()).await.expect("send");
    let mut line = String::new();
    backend_read
        .read_line(&mut line)
        .await
        .expect("backend sees the request");

    line.clear();
    client_read.read_line(&mut line).await.expect("timeout error");
    let error: Value = serde_json::from_str(&line).expect("error JSON");
    assert_eq!(error["id"], serde_json::json!(5));
    assert_eq!(error["error"]["code"], BACKEND_TIMEOUT_CODE);
    assert_eq!(error["error"]["data"]["retryable"], true);

    // The late reply is swallowed; the next request's reply is the next thing the host reads.
    backend_write
        .write_all(response_line(5).as_bytes())
        .await
        .expect("late reply");
    client_write.write_all(request_line(6).as_bytes()).await.expect("send");
    backend_write
        .write_all(response_line(6).as_bytes())
        .await
        .expect("reply");
    line.clear();
    client_read.read_line(&mut line).await.expect("next response");
    assert_eq!(response_id(&line), serde_json::json!(6));

    drop(client_write);
    let _ = proxy.await;
}

async fn fake_first_backend(stream: tokio::io::DuplexStream) {
    let (read, mut write) = tokio::io::split(stream);
    let mut read = BufReader::new(read);
    let mut line = String::new();
    read.read_line(&mut line).await.expect("first initialize");
    write
        .write_all(response_line(1).as_bytes())
        .await
        .expect("initialize response");
    line.clear();
    read.read_line(&mut line).await.expect("initialized notification");
    line.clear();
    read.read_line(&mut line).await.expect("interrupted request");
}

async fn fake_second_backend(stream: tokio::io::DuplexStream) {
    let (read, mut write) = tokio::io::split(stream);
    let mut read = BufReader::new(read);
    let mut line = String::new();
    read.read_line(&mut line).await.expect("replayed initialize");
    assert_eq!(line, initialize_line());
    let replay_frames = format!(
        "{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}}\n{}",
        response_line(1)
    );
    write
        .write_all(replay_frames.as_bytes())
        .await
        .expect("notification and replayed initialize response");
    line.clear();
    read.read_line(&mut line).await.expect("replayed initialized");
    assert_eq!(line, initialized_line());
    line.clear();
    read.read_line(&mut line).await.expect("post-restart request");
    assert_eq!(response_id(&line), serde_json::json!(3));
    write
        .write_all(response_line(3).as_bytes())
        .await
        .expect("post-restart response");
    line.clear();
    assert_eq!(
        read.read_line(&mut line).await.expect("client shutdown"),
        0,
        "proxy closes the backend write half after client EOF"
    );
}

fn call_line(id: u64, tool: &str, mode: &str) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\"params\":{{\"name\":\"{tool}\",\"arguments\":{{\"mode\":\"{mode}\"}}}}}}\n"
    )
}

#[test]
fn keyless_post_is_stamped_with_an_idempotency_key_and_is_replayable() {
    let mut state = SessionState::default();
    let forwarded = state.observe_client(&call_line(1, "agents", "post"));
    let message: Value = serde_json::from_str(&forwarded).expect("forwarded JSON");
    let key = message["params"]["arguments"]["idempotency_key"].as_str().expect("key");
    assert!(key.starts_with("relay-"));
    assert!(forwarded.ends_with('\n'));
    assert_eq!(state.replay.get("1").expect("replayable").line, forwarded);

    let keyed = "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"agents\",\"arguments\":{\"mode\":\"post\",\"idempotency_key\":\"mine\"}}}\n";
    assert_eq!(state.observe_client(keyed), keyed);
    assert!(state.replay.contains_key("2"));
}

#[test]
fn only_reads_and_keyed_posts_are_replayable() {
    let mut state = SessionState::default();
    state.observe_client(&request_line(1));
    state.observe_client(&call_line(2, "code_outline", ""));
    state.observe_client(&call_line(3, "agents", "inbox"));
    state.observe_client(&call_line(4, "agents", "thread_start"));
    state.observe_client(&call_line(5, "index_rescan", ""));
    state.observe_client(&call_line(6, "memory_save", ""));
    let errors = state.drain_restart_errors();
    let failed: Vec<Value> = errors
        .iter()
        .map(|e| serde_json::from_str::<Value>(e).expect("json")["id"].clone())
        .collect();
    assert_eq!(
        failed,
        vec![serde_json::json!(4), serde_json::json!(5), serde_json::json!(6)]
    );
    assert_eq!(state.take_replays().len(), 3);
    // A second restart fails what was already replayed once.
    assert_eq!(state.drain_restart_errors().len(), 3);
}

/// An in-flight keyed post survives a backend restart: the replacement backend receives the
/// identical line (same key) once, and the host sees only the real reply, no `backend_restarted`.
#[tokio::test]
async fn in_flight_post_is_replayed_once_on_the_replacement_backend() {
    let (client_input, proxy_input) = tokio::io::duplex(4096);
    let (proxy_output, client_output) = tokio::io::duplex(4096);
    let (proxy_backend_one, backend_one) = tokio::io::duplex(4096);
    let (proxy_backend_two, backend_two) = tokio::io::duplex(4096);
    let replacements = Arc::new(Mutex::new(VecDeque::from([proxy_backend_two])));
    let connector = Arc::clone(&replacements);
    let proxy = tokio::spawn(run(proxy_input, proxy_output, proxy_backend_one, move || {
        std::future::ready(connector.lock().expect("lock").pop_front().ok_or("none"))
    }));

    let mut client_write = client_input;
    let mut client_read = BufReader::new(client_output);
    client_write
        .write_all(call_line(9, "agents", "post").as_bytes())
        .await
        .expect("send post");

    let mut first = BufReader::new(backend_one);
    let mut sent = String::new();
    first.read_line(&mut sent).await.expect("first backend sees post");
    drop(first);

    let (read, mut write) = tokio::io::split(backend_two);
    let mut read = BufReader::new(read);
    let mut replayed = String::new();
    read.read_line(&mut replayed).await.expect("replayed post");
    assert_eq!(replayed, sent, "same line, same idempotency key");
    write.write_all(response_line(9).as_bytes()).await.expect("reply");

    let mut line = String::new();
    client_read.read_line(&mut line).await.expect("response");
    assert_eq!(response_id(&line), serde_json::json!(9));
    assert!(line.contains("result"));

    drop(client_write);
    let _ = proxy.await;
}

fn initialize_line() -> String {
    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n".to_owned()
}

fn initialized_line() -> String {
    "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n".to_owned()
}

fn request_line(id: u64) -> String {
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/list\"}}\n")
}

fn response_line(id: u64) -> String {
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{}}}}\n")
}

fn response_id(line: &str) -> Value {
    serde_json::from_str::<Value>(line).expect("valid response")["id"].clone()
}
