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
    let replacements = Arc::new(Mutex::new(VecDeque::from([proxy_backend_one, proxy_backend_two])));
    let connector_replacements = Arc::clone(&replacements);

    let proxy = tokio::spawn(run(proxy_input, proxy_output, move || {
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
    let mut only = Some(proxy_backend);
    let proxy = tokio::spawn(run_with_deadline(
        proxy_input,
        proxy_output,
        move || std::future::ready(only.take().ok_or("no replacement")),
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

/// `inbox` with `mark_read` changes what the next `inbox` returns. Replayed after the daemon
/// marked and died before replying, it would come back without the messages nobody read.
#[test]
fn an_inbox_read_that_marks_messages_read_is_not_replayed() {
    let inbox = |id: u64, mark_read: &str| {
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\"params\":{{\"name\":\"agents\",\
             \"arguments\":{{\"mode\":\"inbox\",\"mark_read\":{mark_read}}}}}}}\n"
        )
    };
    let mut state = SessionState::default();
    state.observe_client(&inbox(1, "true"));
    state.observe_client(&inbox(2, "\"true\""));
    state.observe_client(&inbox(3, "false"));
    state.observe_client(&inbox(4, "null"));
    state.observe_client(&call_line(5, "agents", "inbox"));
    assert_eq!(
        state.replay.keys().map(String::as_str).collect::<Vec<_>>(),
        ["3", "4", "5"],
        "only a read that marks nothing is safe to send twice"
    );
}

/// A tool that is not listed as safe is not replayed, whatever else is true of it: the agent tools
/// of the next release are not in the list, and a post sent twice is a message delivered twice.
#[test]
fn a_tool_without_a_replay_decision_is_not_replayed() {
    let mut state = SessionState::default();
    for (id, tool) in ["agent_post", "agent_inbox", "agent_threads", "a_tool_added_later"]
        .into_iter()
        .enumerate()
    {
        state.observe_client(&call_line(id as u64, tool, ""));
    }
    assert!(state.replay.is_empty(), "{:?}", state.replay.keys());
    assert_eq!(state.drain_restart_errors().len(), 4);
}

/// An in-flight keyed post survives a backend restart: the replacement backend receives the
/// identical line (same key) once, and the host sees only the real reply, no `backend_restarted`.
#[tokio::test]
async fn in_flight_post_is_replayed_once_on_the_replacement_backend() {
    let (client_input, proxy_input) = tokio::io::duplex(4096);
    let (proxy_output, client_output) = tokio::io::duplex(4096);
    let (proxy_backend_one, backend_one) = tokio::io::duplex(4096);
    let (proxy_backend_two, backend_two) = tokio::io::duplex(4096);
    let replacements = Arc::new(Mutex::new(VecDeque::from([proxy_backend_one, proxy_backend_two])));
    let connector = Arc::clone(&replacements);
    let proxy = tokio::spawn(run(proxy_input, proxy_output, move || {
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

/// What one connect attempt yields.
enum Step {
    Backend(tokio::io::DuplexStream),
    Fail(ReconnectFailure, &'static str),
    /// The attempt never completes, as one waiting on a daemon that neither answers nor goes.
    Hang,
    /// The attempt takes this long, as a takeover or a daemon spawn does, and then ends as the
    /// inner step.
    After(Duration, Box<Step>),
}

impl Step {
    fn attempt(self) -> Attempt {
        match self {
            Self::Backend(stream) => Box::pin(std::future::ready(Ok(stream))),
            Self::Fail(failure, reason) => Box::pin(std::future::ready(Err(Refusal(failure, reason)))),
            Self::Hang => Box::pin(std::future::pending()),
            Self::After(wait, then) => Box::pin(async move {
                tokio::time::sleep(wait).await;
                then.attempt().await
            }),
        }
    }
}

type Attempt = std::pin::Pin<Box<dyn Future<Output = Result<tokio::io::DuplexStream, Refusal>> + Send>>;

/// The relay under test with the host's ends of its pipes, a script of connect attempts and the
/// time of each attempt made so far.
struct Session {
    to_relay: tokio::io::DuplexStream,
    from_relay: BufReader<tokio::io::DuplexStream>,
    relay: tokio::task::JoinHandle<io::Result<()>>,
    attempts: Arc<Mutex<Vec<tokio::time::Instant>>>,
    started: tokio::time::Instant,
}

impl Session {
    /// Start a relay whose connect attempts follow `steps`, and fail as `then` once they run out.
    fn start(steps: Vec<Step>, then: (ReconnectFailure, &'static str)) -> Self {
        let (to_relay, relay_input) = tokio::io::duplex(4096);
        let (relay_output, from_relay) = tokio::io::duplex(4096);
        let attempts: Arc<Mutex<Vec<tokio::time::Instant>>> = Arc::default();
        let recorded = Arc::clone(&attempts);
        let mut steps = VecDeque::from(steps);
        let relay = tokio::spawn(run_with_deadline(
            relay_input,
            relay_output,
            move || {
                recorded
                    .lock()
                    .expect("attempts lock")
                    .push(tokio::time::Instant::now());
                steps.pop_front().unwrap_or(Step::Fail(then.0, then.1)).attempt()
            },
            None,
        ));
        Self {
            to_relay,
            from_relay: BufReader::new(from_relay),
            relay,
            attempts,
            started: tokio::time::Instant::now(),
        }
    }

    async fn send(&mut self, line: String) {
        self.to_relay
            .write_all(line.as_bytes())
            .await
            .expect("send to the relay");
    }

    async fn read(&mut self) -> Value {
        read_json(&mut self.from_relay).await
    }

    fn attempt_count(&self) -> usize {
        self.attempts.lock().expect("attempts lock").len()
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The relay's result; it must have ended, or be about to.
    async fn ended(self) -> io::Result<()> {
        tokio::time::timeout(Duration::from_secs(30), self.relay)
            .await
            .expect("the relay must end rather than keep retrying")
            .expect("relay join")
    }
}

/// A backend the relay can be handed, with the far end a test drives.
fn backend() -> (Step, BufReader<tokio::io::DuplexStream>) {
    let (near, far) = tokio::io::duplex(4096);
    (Step::Backend(near), BufReader::new(far))
}

/// The next line the relay forwards to `backend`. Bounded like [`read_json`], so a relay that
/// forwards nothing fails the test instead of hanging it.
async fn backend_reads(backend: &mut BufReader<tokio::io::DuplexStream>) -> String {
    const BACKEND_WAIT: Duration = Duration::from_secs(3600);
    let mut line = String::new();
    tokio::time::timeout(BACKEND_WAIT, backend.read_line(&mut line))
        .await
        .expect("the relay must forward to the backend")
        .expect("the backend reads a line");
    line
}

fn blocked(times: usize, reason: &'static str) -> Vec<Step> {
    (0..times)
        .map(|_| Step::Fail(ReconnectFailure::Blocked, reason))
        .collect()
}

const NEWER: &str = "a newer basemind comms daemon is running; update this basemind";
const WONT_STOP: &str = "a basemind comms daemon from another build refused to stop";
const REFUSED: &str = "connection refused";

/// A newer daemon on another protocol can never be reconnected to. The relay must say so on every
/// request it still owes an answer, and on one that arrives just after, then end with an error.
#[tokio::test(start_paused = true)]
async fn a_fatal_reconnect_failure_answers_the_host_and_ends_the_relay() {
    let (first, mut backend) = backend();
    let mut session = Session::start(vec![first], (ReconnectFailure::Fatal, NEWER));
    session.send(request_line(2)).await;
    backend_reads(&mut backend).await;
    drop(backend);

    let pending = session.read().await;
    assert_eq!(pending["id"], serde_json::json!(2));
    assert_eq!(pending["error"]["code"], BACKEND_UNAVAILABLE_CODE);
    assert!(
        pending["error"]["message"].as_str().expect("message").contains(NEWER),
        "{pending}"
    );
    assert_eq!(pending["error"]["data"]["retryable"], false);

    session.send(request_line(3)).await;
    let later = session.read().await;
    assert_eq!(later["id"], serde_json::json!(3));
    assert!(
        later["error"]["message"].as_str().expect("message").contains(NEWER),
        "{later}"
    );

    assert_eq!(session.attempt_count(), 2, "a fatal failure is not retried");
    let error = session
        .ended()
        .await
        .expect_err("the relay ends with an error, so the process exits non-zero");
    assert!(error.to_string().contains(NEWER), "{error}");
}

/// The upgrade path: the session starts while a daemon it can never use holds the generation.
/// There is no backend to lose, so the answer has to come from the first connect itself, and the
/// host's `initialize` must get it whether it was already written or arrives a little later.
#[tokio::test(start_paused = true)]
async fn a_session_that_can_never_start_answers_initialize_with_the_reason_and_ends() {
    for host_delay in [Duration::ZERO, Duration::from_secs(3)] {
        let mut session = Session::start(Vec::new(), (ReconnectFailure::Fatal, NEWER));
        tokio::time::sleep(host_delay).await;
        session.send(initialize_line()).await;

        let answer = session.read().await;
        assert_eq!(answer["id"], serde_json::json!(1), "{answer}");
        assert_eq!(answer["error"]["code"], BACKEND_UNAVAILABLE_CODE);
        assert!(
            answer["error"]["message"].as_str().expect("message").contains(NEWER),
            "{answer}"
        );
        assert_eq!(answer["error"]["data"]["retryable"], false);

        assert_eq!(session.attempt_count(), 1);
        let error = session.ended().await.expect_err("the session cannot start");
        assert!(error.to_string().contains(NEWER), "{error}");
    }
}

/// An older daemon that will not stop yet is in the way when the session starts, and leaves inside
/// the hold window. The host must see an ordinary start: its `initialize` reaches the daemon.
#[tokio::test(start_paused = true)]
async fn a_session_blocked_at_start_comes_up_by_itself_once_the_daemon_is_reachable() {
    let (reachable, mut backend) = backend();
    let mut steps = blocked(3, WONT_STOP);
    steps.push(reachable);
    let mut session = Session::start(steps, (ReconnectFailure::Fatal, "unreachable step"));
    session.send(initialize_line()).await;

    assert_eq!(backend_reads(&mut backend).await, initialize_line());
    backend
        .write_all(response_line(1).as_bytes())
        .await
        .expect("initialize response");
    let answer = session.read().await;
    assert_eq!(answer["id"], serde_json::json!(1));
    assert!(answer.get("result").is_some(), "no error may reach the host: {answer}");
    assert!(session.elapsed() < HOLD_WINDOW, "{:?}", session.elapsed());
    assert_eq!(session.attempt_count(), 4);
}

/// A daemon that cannot be reached at all is retried for good, but not silently: once the hold
/// window has passed the host is told why.
#[tokio::test(start_paused = true)]
async fn a_backend_that_stays_away_answers_requests_after_the_hold_window() {
    let mut session = Session::start(Vec::new(), (ReconnectFailure::Transient, REFUSED));
    session.send(request_line(1)).await;

    let refused = session.read().await;
    assert_eq!(refused["id"], serde_json::json!(1));
    assert_eq!(refused["error"]["code"], BACKEND_UNAVAILABLE_CODE, "{refused}");
    assert!(
        refused["error"]["message"].as_str().expect("message").contains(REFUSED),
        "{refused}"
    );
    assert_eq!(refused["error"]["data"]["retryable"], true);
    assert!(session.elapsed() >= HOLD_WINDOW, "{:?}", session.elapsed());
    let gaps: Vec<Duration> = {
        let attempts = session.attempts.lock().expect("attempts lock");
        attempts.windows(2).map(|pair| pair[1] - pair[0]).collect()
    };
    assert!(
        gaps.iter().all(|gap| *gap <= MAX_RECONNECT_DELAY),
        "a restarting backend is retried at the short back-off: {gaps:?}"
    );
}

/// A request that arrives long after the backend went still gets its own window: the daemon may be
/// seconds from reachable, and when the earlier failures began says nothing about that.
#[tokio::test(start_paused = true)]
async fn the_hold_window_of_a_request_starts_when_it_arrives() {
    const QUIET: Duration = Duration::from_secs(30);
    let mut session = Session::start(Vec::new(), (ReconnectFailure::Transient, REFUSED));
    tokio::time::sleep(QUIET).await;
    assert!(session.attempt_count() > 1, "attempts were failing all along");
    session.send(request_line(3)).await;

    let refused = session.read().await;
    assert_eq!(refused["id"], serde_json::json!(3));
    assert!(
        refused["error"]["message"].as_str().expect("message").contains(REFUSED),
        "{refused}"
    );
    let waited = session.elapsed();
    assert!(
        waited >= QUIET + HOLD_WINDOW && waited < QUIET + HOLD_WINDOW + Duration::from_secs(1),
        "held for the window from its arrival: {waited:?}"
    );
}

/// The host going away must end `basemind serve` at once, whatever the relay is in the middle of:
/// an attempt that has not returned, or the wait before the next one.
#[tokio::test(start_paused = true)]
async fn the_relay_ends_when_the_host_closes_its_pipe_during_an_attempt_or_between_attempts() {
    for steps in [vec![Step::Hang], blocked(1, WONT_STOP)] {
        let mut session = Session::start(steps, (ReconnectFailure::Blocked, WONT_STOP));
        session.send(request_line(8)).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Session { to_relay, relay, .. } = session;
        drop(to_relay);

        let ended = tokio::time::timeout(Duration::from_secs(1), relay)
            .await
            .expect("the relay ends within a second of the host closing its pipe")
            .expect("relay join");
        assert!(ended.is_ok(), "a host that closed its pipe is not an error: {ended:?}");
    }
}

/// ADR-0013's relay replay holds while a daemon is in the way: a replayable request in flight
/// when the backend went is neither failed nor lost if a backend arrives inside the hold window.
#[tokio::test(start_paused = true)]
async fn a_request_in_flight_is_held_while_blocked_and_replayed_when_a_backend_arrives_in_time() {
    let (first, mut backend_one) = backend();
    let (second, mut backend_two) = backend();
    let mut steps = vec![first];
    steps.extend(blocked(4, WONT_STOP));
    steps.push(second);
    let mut session = Session::start(steps, (ReconnectFailure::Fatal, "unreachable step"));
    session.send(request_line(4)).await;
    let in_flight = backend_reads(&mut backend_one).await;
    drop(backend_one);

    assert_eq!(
        backend_reads(&mut backend_two).await,
        in_flight,
        "the held request is re-sent"
    );
    backend_two.write_all(response_line(4).as_bytes()).await.expect("reply");
    let answer = session.read().await;
    assert_eq!(answer["id"], serde_json::json!(4));
    assert!(
        answer.get("result").is_some(),
        "the host sees the reply and no error: {answer}"
    );
    assert!(session.elapsed() < HOLD_WINDOW, "{:?}", session.elapsed());
}

/// An older daemon that will not stop keeps the backend away for as long as its sessions last. The
/// relay backs off further than for a restart, holds what it can replay for the hold window, then
/// tells the host why instead of leaving requests unread, and carries on once a backend is there.
#[tokio::test(start_paused = true)]
async fn a_blocked_reconnect_backs_off_refuses_requests_after_the_hold_window_and_then_recovers() {
    const BLOCKED_ATTEMPTS: usize = 12;
    let (first, mut backend_one) = backend();
    let (second, mut backend_two) = backend();
    let mut steps = vec![first];
    steps.extend(blocked(BLOCKED_ATTEMPTS, WONT_STOP));
    steps.push(second);
    let mut session = Session::start(steps, (ReconnectFailure::Fatal, "unreachable step"));
    session.send(request_line(4)).await;
    backend_reads(&mut backend_one).await;
    drop(backend_one);

    let held = session.read().await;
    assert_eq!(held["id"], serde_json::json!(4));
    assert_eq!(held["error"]["code"], BACKEND_UNAVAILABLE_CODE, "{held}");
    assert_eq!(held["error"]["data"]["retryable"], true);
    let waited = session.elapsed();
    assert!(
        waited >= HOLD_WINDOW,
        "a request held for replay is not failed before the hold window ends: {waited:?}"
    );
    assert!(
        waited < HOLD_WINDOW + Duration::from_secs(1),
        "and is not left waiting past it: {waited:?}"
    );

    session.send(request_line(5)).await;
    let refused = session.read().await;
    assert_eq!(refused["id"], serde_json::json!(5));
    assert_eq!(refused["error"]["code"], BACKEND_UNAVAILABLE_CODE);
    assert!(
        refused["error"]["message"]
            .as_str()
            .expect("message")
            .contains(WONT_STOP),
        "{refused}"
    );
    assert_eq!(refused["error"]["data"]["retryable"], true);

    while session.attempt_count() <= BLOCKED_ATTEMPTS + 1 {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    session.send(request_line(6)).await;
    let forwarded = backend_reads(&mut backend_two).await;
    assert_eq!(
        response_id(&forwarded),
        serde_json::json!(6),
        "the failed request is not replayed"
    );
    backend_two.write_all(response_line(6).as_bytes()).await.expect("reply");
    let answer = session.read().await;
    assert_eq!(answer["id"], serde_json::json!(6));
    assert!(answer.get("result").is_some(), "{answer}");

    let attempts = session.attempts.lock().expect("attempts lock").clone();
    assert_eq!(attempts.len(), BLOCKED_ATTEMPTS + 2);
    let gaps: Vec<Duration> = attempts[1..].windows(2).map(|pair| pair[1] - pair[0]).collect();
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
}

/// `initialize` was forwarded and the backend went before answering. A fatal failure must answer
/// it: the host is waiting on exactly that request.
#[tokio::test(start_paused = true)]
async fn a_fatal_failure_answers_an_initialize_still_in_flight() {
    let (first, mut backend) = backend();
    let mut session = Session::start(vec![first], (ReconnectFailure::Fatal, NEWER));
    session.send(initialize_line()).await;
    backend_reads(&mut backend).await;
    drop(backend);

    let answer = session.read().await;
    assert_eq!(answer["id"], serde_json::json!(1), "{answer}");
    assert_eq!(answer["error"]["code"], BACKEND_UNAVAILABLE_CODE);
    assert!(
        answer["error"]["message"].as_str().expect("message").contains(NEWER),
        "{answer}"
    );
    assert_eq!(answer["error"]["data"]["retryable"], false);
    session.ended().await.expect_err("a fatal failure ends the relay");
}

/// `initialize` was forwarded and the backend went before answering; the replacement answers it.
/// The host reads lines, so the answer must end with a newline to be seen at all.
#[tokio::test(start_paused = true)]
async fn an_initialize_in_flight_is_answered_by_the_replacement_backend_as_a_whole_line() {
    let (first, mut backend_one) = backend();
    let (second, mut backend_two) = backend();
    let mut session = Session::start(vec![first, second], (ReconnectFailure::Fatal, "unreachable step"));
    session.send(initialize_line()).await;
    backend_reads(&mut backend_one).await;
    drop(backend_one);

    assert_eq!(backend_reads(&mut backend_two).await, initialize_line());
    backend_two
        .write_all(response_line(1).as_bytes())
        .await
        .expect("initialize response");
    let answer = session.read().await;
    assert_eq!(answer["id"], serde_json::json!(1));
    assert!(answer.get("result").is_some(), "{answer}");
}

#[path = "stdio_relay_offline_tests.rs"]
mod offline;

#[test]
fn stop_may_be_asked_until_one_is_sent_and_then_once_per_interval() {
    let mut pacing = StopPacing::default();
    let start = std::time::Instant::now();

    assert!(pacing.may_ask_at(start), "the first attempt may ask");
    assert!(
        pacing.may_ask_at(start + Duration::from_secs(1)),
        "an attempt that sent no Stop does not use up the interval"
    );
    pacing.asked_at(start + Duration::from_secs(1));
    assert!(!pacing.may_ask_at(start + Duration::from_secs(2)));
    let due = start + Duration::from_secs(1) + STOP_REQUEST_INTERVAL;
    assert!(!pacing.may_ask_at(due - Duration::from_millis(1)));
    assert!(pacing.may_ask_at(due));
}

#[test]
fn takeover_errors_decide_how_the_relay_reconnects() {
    use crate::relay_refusal::RelayRefusal;
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
    let failing = through_context(SingletonError::FailingPredecessor {
        version: "0.1.0".to_string(),
        pid: 1,
        store: "/tmp/store.fjall".into(),
        message: "poisoned".to_string(),
    });
    assert_eq!(failing.failure(), ReconnectFailure::Blocked);
    let too_many = through_context(SingletonError::TooManyDaemons { count: 8, max: 8 });
    assert_eq!(too_many.failure(), ReconnectFailure::Blocked);
    assert!(
        too_many.reason().contains("basemind comms doctor"),
        "{}",
        too_many.reason()
    );
    let no_spawn = through_context(SingletonError::AutospawnDisabled {
        socket: "/tmp/comms.sock".into(),
        env: "BASEMIND_NO_AUTOSPAWN",
    });
    assert_eq!(no_spawn.failure(), ReconnectFailure::Blocked);
    assert_eq!(
        through_context(SingletonError::NoDataDir).failure(),
        ReconnectFailure::Fatal
    );
    #[cfg(unix)]
    assert_eq!(
        through_context(SingletonError::SocketPathTooLong {
            path: "/tmp/long".into(),
            len: 200,
            limit: 103,
        })
        .failure(),
        ReconnectFailure::Fatal,
        "the path is the same on every retry"
    );
    assert_eq!(
        through_context(SingletonError::SpawnTimeout).failure(),
        ReconnectFailure::Transient
    );

    let refusal = |refusal: RelayRefusal| Err::<(), _>(refusal).context("relay handshake").expect_err("an error");
    let newer_relay = refusal(RelayRefusal::ProtoMismatch {
        daemon: 9,
        client: 2,
        daemon_version: "9.9.9".to_string(),
    });
    assert_eq!(newer_relay.failure(), ReconnectFailure::Fatal);
    assert!(
        newer_relay.reason().contains("update this basemind"),
        "{}",
        newer_relay.reason()
    );
    let older_relay = refusal(RelayRefusal::ProtoMismatch {
        daemon: 1,
        client: 2,
        daemon_version: "0.1.0".to_string(),
    });
    assert_eq!(
        older_relay.failure(),
        ReconnectFailure::Blocked,
        "an older daemon goes once its sessions end"
    );
    let declined = refusal(RelayRefusal::Declined {
        code: "view_unsupported".to_string(),
        detail: None,
    });
    assert_eq!(declined.failure(), ReconnectFailure::Fatal);
    assert!(declined.reason().contains("view_unsupported"), "{}", declined.reason());
    assert_eq!(
        anyhow::anyhow!("connection refused").failure(),
        ReconnectFailure::Transient
    );
}
