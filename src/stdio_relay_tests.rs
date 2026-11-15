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

/// A reconnect failure of a chosen kind.
#[derive(Debug)]
struct Refusal(ReconnectFailure, &'static str);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.1)
    }
}

impl ReconnectError for Refusal {
    fn failure(&self) -> ReconnectFailure {
        self.0
    }
}

/// Read the relay's next line to the host. Bounded in the paused clock's time, so a relay that
/// keeps retrying instead of answering fails the test rather than spinning it forever.
async fn read_json(reader: &mut BufReader<tokio::io::DuplexStream>) -> Value {
    const HOST_WAIT: Duration = Duration::from_secs(3600);
    let mut line = String::new();
    tokio::time::timeout(HOST_WAIT, reader.read_line(&mut line))
        .await
        .expect("the relay must answer the host")
        .expect("read a line from the relay");
    serde_json::from_str(&line).expect("the relay writes JSON lines")
}

/// A newer daemon on another protocol can never be reconnected to. The relay must say so on every
/// request it still owes an answer, and on one that arrives just after, then end with an error.
#[tokio::test(start_paused = true)]
async fn a_fatal_reconnect_failure_answers_the_host_and_ends_the_relay() {
    const REASON: &str = "a newer basemind comms daemon is running; update this basemind";
    let (client_input, proxy_input) = tokio::io::duplex(4096);
    let (proxy_output, client_output) = tokio::io::duplex(4096);
    let (proxy_backend, backend) = tokio::io::duplex(4096);
    let attempts = Arc::new(Mutex::new(0u32));
    let counted = Arc::clone(&attempts);
    let proxy = tokio::spawn(run_with_deadline(
        proxy_input,
        proxy_output,
        proxy_backend,
        move || {
            *counted.lock().expect("attempts lock") += 1;
            std::future::ready(Err::<tokio::io::DuplexStream, _>(Refusal(
                ReconnectFailure::Fatal,
                REASON,
            )))
        },
        None,
    ));

    let mut client_write = client_input;
    let mut client_read = BufReader::new(client_output);
    client_write.write_all(request_line(2).as_bytes()).await.expect("send");
    let mut backend = BufReader::new(backend);
    let mut seen = String::new();
    backend
        .read_line(&mut seen)
        .await
        .expect("the backend sees the request");
    drop(backend);

    let pending = read_json(&mut client_read).await;
    assert_eq!(pending["id"], serde_json::json!(2));
    assert_eq!(pending["error"]["code"], BACKEND_UNAVAILABLE_CODE);
    assert!(
        pending["error"]["message"].as_str().expect("message").contains(REASON),
        "{pending}"
    );
    assert_eq!(pending["error"]["data"]["retryable"], false);

    client_write.write_all(request_line(3).as_bytes()).await.expect("send");
    let later = read_json(&mut client_read).await;
    assert_eq!(later["id"], serde_json::json!(3));
    assert!(
        later["error"]["message"].as_str().expect("message").contains(REASON),
        "{later}"
    );

    let ended = tokio::time::timeout(Duration::from_secs(30), proxy)
        .await
        .expect("a fatal failure must end the relay rather than retry")
        .expect("proxy join");
    let error = ended.expect_err("the relay ends with an error, so the process exits non-zero");
    assert!(error.to_string().contains(REASON), "{error}");
    assert_eq!(
        *attempts.lock().expect("attempts lock"),
        1,
        "a fatal failure is not retried"
    );
}

/// An older daemon that will not stop keeps the backend away for as long as its sessions last. The
/// relay backs off further than for a restart, tells the host why instead of leaving requests
/// unread, and carries on once a backend is there.
#[tokio::test(start_paused = true)]
async fn a_blocked_reconnect_backs_off_refuses_requests_and_then_recovers() {
    const REASON: &str = "a basemind comms daemon from another build refused to stop";
    const BLOCKED_ATTEMPTS: usize = 12;
    let (client_input, proxy_input) = tokio::io::duplex(4096);
    let (proxy_output, client_output) = tokio::io::duplex(4096);
    let (proxy_backend_one, backend_one) = tokio::io::duplex(4096);
    let (proxy_backend_two, backend_two) = tokio::io::duplex(4096);
    let attempts: Arc<Mutex<Vec<tokio::time::Instant>>> = Arc::default();
    let recorded = Arc::clone(&attempts);
    let mut replacement = Some(proxy_backend_two);
    let proxy = tokio::spawn(run_with_deadline(
        proxy_input,
        proxy_output,
        proxy_backend_one,
        move || {
            let mut attempts = recorded.lock().expect("attempts lock");
            attempts.push(tokio::time::Instant::now());
            let outcome = if attempts.len() <= BLOCKED_ATTEMPTS {
                Err(Refusal(ReconnectFailure::Blocked, REASON))
            } else {
                Ok(replacement.take().expect("one replacement backend"))
            };
            std::future::ready(outcome)
        },
        None,
    ));

    let mut client_write = client_input;
    let mut client_read = BufReader::new(client_output);
    client_write.write_all(request_line(4).as_bytes()).await.expect("send");
    let mut backend_one = BufReader::new(backend_one);
    let mut in_flight = String::new();
    backend_one
        .read_line(&mut in_flight)
        .await
        .expect("the first backend sees the request");
    drop(backend_one);
    let held = read_json(&mut client_read).await;
    assert_eq!(
        held["id"],
        serde_json::json!(4),
        "a request held for replay is not left waiting"
    );
    assert_eq!(held["error"]["code"], BACKEND_UNAVAILABLE_CODE, "{held}");
    assert_eq!(held["error"]["data"]["retryable"], true);

    client_write.write_all(request_line(5).as_bytes()).await.expect("send");
    let refused = read_json(&mut client_read).await;
    assert_eq!(refused["id"], serde_json::json!(5));
    assert_eq!(refused["error"]["code"], BACKEND_UNAVAILABLE_CODE);
    assert!(
        refused["error"]["message"].as_str().expect("message").contains(REASON),
        "{refused}"
    );
    assert_eq!(refused["error"]["data"]["retryable"], true);

    while attempts.lock().expect("attempts lock").len() <= BLOCKED_ATTEMPTS {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let (read, mut write) = tokio::io::split(backend_two);
    let mut read = BufReader::new(read);
    client_write.write_all(request_line(6).as_bytes()).await.expect("send");
    let mut forwarded = String::new();
    read.read_line(&mut forwarded)
        .await
        .expect("the replacement backend is used");
    assert_eq!(response_id(&forwarded), serde_json::json!(6));
    write.write_all(response_line(6).as_bytes()).await.expect("reply");
    let answer = read_json(&mut client_read).await;
    assert_eq!(answer["id"], serde_json::json!(6));
    assert!(answer.get("result").is_some(), "{answer}");

    let attempts = attempts.lock().expect("attempts lock").clone();
    assert_eq!(attempts.len(), BLOCKED_ATTEMPTS + 1);
    let gaps: Vec<Duration> = attempts.windows(2).map(|pair| pair[1] - pair[0]).collect();
    assert!(
        gaps.windows(2).all(|pair| pair[1] >= pair[0]),
        "the back-off never shrinks: {gaps:?}"
    );
    assert_eq!(gaps[0], INITIAL_RECONNECT_DELAY);
    assert_eq!(
        gaps.last().copied(),
        Some(MAX_BLOCKED_RECONNECT_DELAY),
        "a blocked reconnect backs off past the restart cap: {gaps:?}"
    );

    drop(client_write);
    let _ = proxy.await;
}

#[test]
fn stop_is_asked_on_the_first_attempt_and_then_once_per_interval() {
    let mut pacing = StopPacing::default();
    let start = std::time::Instant::now();

    assert!(pacing.may_ask_at(start), "the first attempt asks");
    assert!(!pacing.may_ask_at(start + Duration::from_secs(1)));
    assert!(!pacing.may_ask_at(start + STOP_REQUEST_INTERVAL - Duration::from_millis(1)));
    assert!(pacing.may_ask_at(start + STOP_REQUEST_INTERVAL));
    assert!(!pacing.may_ask_at(start + STOP_REQUEST_INTERVAL + Duration::from_secs(1)));
}

#[test]
fn takeover_errors_decide_how_the_relay_reconnects() {
    use anyhow::Context as _;
    use basemind::comms::singleton::SingletonError;

    let through_context = |error: SingletonError| Err::<(), _>(error).context("ensure daemon").expect_err("an error");
    let newer = through_context(SingletonError::NewerDaemon {
        version: "9.9.9".to_string(),
        pid: 1,
        daemon_proto: 99,
        ours: "0.1.0".to_string(),
        our_proto: 1,
    });
    assert_eq!(newer.failure(), ReconnectFailure::Fatal);
    assert!(newer.reason().contains("update this basemind"), "{}", newer.reason());

    let busy = through_context(SingletonError::BusyPredecessor {
        version: "0.1.0".to_string(),
        pid: 1,
        code: "daemon_busy".to_string(),
        message: "busy".to_string(),
    });
    assert_eq!(busy.failure(), ReconnectFailure::Blocked);
    let stale = through_context(SingletonError::StalePredecessor {
        version: "0.1.0".to_string(),
        pid: 1,
    });
    assert_eq!(stale.failure(), ReconnectFailure::Blocked);
    assert_eq!(
        through_context(SingletonError::SpawnTimeout).failure(),
        ReconnectFailure::Transient
    );
    assert_eq!(
        anyhow::anyhow!("connection refused").failure(),
        ReconnectFailure::Transient
    );
}
