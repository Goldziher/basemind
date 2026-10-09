//! Protocol-aware stdio relay that can replace a failed daemon connection without closing the
//! MCP host's stdin/stdout pipes.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader};

const BACKEND_RESTARTED_CODE: i64 = -32001;
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(250);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(5);
const REPLAY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_JSON_LINE_BYTES: usize = 8 * 1024 * 1024;
/// JSON-RPC code answering a request the backend never replied to within the relay deadline.
const BACKEND_TIMEOUT_CODE: i64 = -32002;
/// Env var overriding the per-request deadline, in seconds. `0` disables it.
pub(crate) const REQUEST_TIMEOUT_ENV: &str = "BASEMIND_RELAY_REQUEST_TIMEOUT_SECS";
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 180;
/// Cap on remembered abandoned request ids (late replies to these are dropped).
const ABANDONED_CAP: usize = 1_024;

/// JSON-RPC methods that only read, so re-sending them on a new backend is harmless.
const REPLAYABLE_METHODS: &[&str] = &[
    "ping",
    "tools/list",
    "resources/list",
    "resources/read",
    "resources/templates/list",
    "prompts/list",
    "prompts/get",
    "completion/complete",
];
/// Tools whose every mode is read-only.
const READ_ONLY_TOOLS: &[&str] = &["code", "git", "graph"];
/// `agents` modes that are reads or naturally idempotent (`post` is made so by a key).
const REPLAYABLE_AGENTS_MODES: &[&str] = &[
    "list",
    "thread_list",
    "members",
    "history",
    "message",
    "inbox",
    "ack",
    "wait",
    "status",
];

/// A request the relay may re-send once after a backend restart.
struct Replayable {
    line: String,
    replayed: bool,
}

/// Decide whether a `tools/call` / read request is safe to replay, rewriting an `agents` `post` to
/// carry a relay-generated `idempotency_key` (when the caller gave none) so the replay cannot store
/// a second copy. Returns the line to forward and whether it is replayable.
fn classify_request(message: &mut Value, line: &str) -> (String, bool) {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return (line.to_owned(), false);
    };
    if REPLAYABLE_METHODS.contains(&method) {
        return (line.to_owned(), true);
    }
    if method != "tools/call" {
        return (line.to_owned(), false);
    }
    let tool = message
        .pointer("/params/name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mode = message
        .pointer("/params/arguments/mode")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if READ_ONLY_TOOLS.contains(&tool) || (tool == "agents" && REPLAYABLE_AGENTS_MODES.contains(&mode)) {
        return (line.to_owned(), true);
    }
    if tool == "agents" && mode == "post" {
        let has_key = message
            .pointer("/params/arguments/idempotency_key")
            .is_some_and(|key| !key.is_null());
        if has_key {
            return (line.to_owned(), true);
        }
        if let Some(arguments) = message.pointer_mut("/params/arguments").and_then(Value::as_object_mut) {
            arguments.insert("idempotency_key".to_owned(), Value::String(relay_idempotency_key()));
            let mut rewritten = message.to_string();
            if line.ends_with('\n') {
                rewritten.push('\n');
            }
            return (rewritten, true);
        }
    }
    (line.to_owned(), false)
}

fn relay_idempotency_key() -> String {
    format!("relay-{}", basemind::comms::new_idempotency_key())
}

fn request_deadline_from_env() -> Option<Duration> {
    let secs = std::env::var(REQUEST_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

#[derive(Default)]
struct SessionState {
    initialize: Option<String>,
    initialized: Option<String>,
    initialize_id: Option<Value>,
    initialize_complete: bool,
    pending: BTreeMap<String, Value>,
    /// When each pending request was first forwarded, for the per-request deadline.
    started: BTreeMap<String, tokio::time::Instant>,
    /// Pending requests that may be re-sent once to a replacement backend instead of failing.
    replay: BTreeMap<String, Replayable>,
    /// Ids already answered with a timeout error; the backend's late reply is dropped so the host
    /// never sees two responses for one request.
    abandoned: std::collections::VecDeque<String>,
}

impl SessionState {
    /// Track a host frame; returns the line to forward (rewritten for a keyless `post`).
    fn observe_client(&mut self, line: &str) -> String {
        let Ok(mut message) = serde_json::from_str::<Value>(line) else {
            return line.to_owned();
        };
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").filter(|id| !id.is_null());
        if method == Some("initialize") {
            self.initialize = Some(line.to_owned());
            self.initialize_id = id.cloned();
        } else if method == Some("notifications/initialized") {
            self.initialized = Some(line.to_owned());
        }
        if method == Some("notifications/cancelled")
            && let Some(request_id) = message.pointer("/params/requestId")
        {
            self.replay.remove(&id_key(request_id));
        }
        let mut forward = line.to_owned();
        if method.is_some()
            && let Some(id) = id.cloned()
        {
            let key = id_key(&id);
            if method != Some("initialize") {
                let (rewritten, replayable) = classify_request(&mut message, line);
                if replayable {
                    self.replay.insert(
                        key.clone(),
                        Replayable {
                            line: rewritten.clone(),
                            replayed: false,
                        },
                    );
                }
                forward = rewritten;
            }
            self.pending.insert(key.clone(), id);
            self.started.insert(key, tokio::time::Instant::now());
        }
        forward
    }

    /// Track a backend frame; returns `false` when it is the late reply to a request the relay
    /// already answered with a timeout error, which must not reach the host.
    fn observe_backend(&mut self, line: &str) -> bool {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return true;
        };
        if message.get("method").is_none()
            && let Some(id) = message.get("id").filter(|id| !id.is_null())
        {
            let key = id_key(id);
            if let Some(at) = self.abandoned.iter().position(|abandoned| *abandoned == key) {
                self.abandoned.remove(at);
                return false;
            }
            self.pending.remove(&key);
            self.started.remove(&key);
            self.replay.remove(&key);
            if self.initialize_id.as_ref() == Some(id) {
                self.initialize_complete = true;
            }
        }
        true
    }

    /// The instant the oldest pending request (other than `initialize`) hits `deadline`.
    fn next_expiry(&self, deadline: Duration) -> Option<tokio::time::Instant> {
        self.started
            .iter()
            .filter(|(key, _)| self.initialize_id.as_ref().map(id_key).as_ref() != Some(*key))
            .map(|(_, at)| *at + deadline)
            .min()
    }

    /// Fail every pending request older than `deadline` with a retryable timeout error.
    fn expire(&mut self, deadline: Duration) -> Vec<String> {
        let now = tokio::time::Instant::now();
        let init_key = self.initialize_id.as_ref().map(id_key);
        let expired: Vec<String> = self
            .started
            .iter()
            .filter(|(key, at)| Some(*key) != init_key.as_ref() && now.duration_since(**at) >= deadline)
            .map(|(key, _)| key.clone())
            .collect();
        let mut errors = Vec::with_capacity(expired.len());
        for key in expired {
            self.started.remove(&key);
            self.replay.remove(&key);
            let Some(id) = self.pending.remove(&key) else {
                continue;
            };
            if self.abandoned.len() >= ABANDONED_CAP {
                self.abandoned.pop_front();
            }
            self.abandoned.push_back(key);
            errors.push(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": BACKEND_TIMEOUT_CODE,
                        "message": format!(
                            "backend_timeout: no reply within {}s; the request may or may not have been applied",
                            deadline.as_secs()
                        ),
                        "data": { "retryable": true }
                    }
                })
                .to_string(),
            );
        }
        errors
    }

    /// Fail every in-flight request that cannot be replayed; replayable ones that have not yet been
    /// re-sent stay pending for [`Self::take_replays`]. A request already replayed once fails too,
    /// so a backend that keeps dying cannot loop it forever.
    /// Lines to re-send on the replacement backend, marking each as replayed and restarting its
    /// deadline clock.
    fn take_replays(&mut self) -> Vec<String> {
        let now = tokio::time::Instant::now();
        let mut lines = Vec::with_capacity(self.replay.len());
        for (key, entry) in &mut self.replay {
            entry.replayed = true;
            self.started.insert(key.clone(), now);
            lines.push(entry.line.clone());
        }
        lines
    }

    fn drain_restart_errors(&mut self) -> Vec<String> {
        let keep: Vec<String> = self
            .replay
            .iter()
            .filter(|(key, entry)| !entry.replayed && self.pending.contains_key(*key))
            .map(|(key, _)| key.clone())
            .collect();
        self.replay.retain(|key, _| keep.contains(key));
        let mut failing = std::mem::take(&mut self.pending);
        let mut kept = BTreeMap::new();
        for key in &keep {
            if let Some(id) = failing.remove(key) {
                kept.insert(key.clone(), id);
            }
        }
        self.pending = kept;
        self.started.retain(|key, _| self.pending.contains_key(key));
        failing
            .into_values()
            .filter(|id| self.initialize_id.as_ref() != Some(id))
            .map(|id| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": BACKEND_RESTARTED_CODE,
                        "message": "backend_restarted",
                        "data": { "retryable": true }
                    }
                })
                .to_string()
            })
            .collect()
    }
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

/// Relay newline-framed MCP messages until the client closes stdin. When the backend disappears,
/// every in-flight request fails once with `backend_restarted`; the proxy reconnects, silently
/// replays MCP initialization, and forwards later requests over the replacement connection.
pub(crate) async fn run<R, W, S, C, F, E>(input: R, output: W, stream: S, reconnect: C) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
    C: FnMut() -> F,
    F: Future<Output = Result<S, E>>,
    E: std::fmt::Display,
{
    run_with_deadline(input, output, stream, reconnect, request_deadline_from_env()).await
}

/// [`run`] with an explicit per-request `deadline`: a request still unanswered after it gets a
/// retryable `backend_timeout` error instead of hanging the host silently.
async fn run_with_deadline<R, W, S, C, F, E>(
    input: R,
    mut output: W,
    mut stream: S,
    mut reconnect: C,
    deadline: Option<Duration>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
    C: FnMut() -> F,
    F: Future<Output = Result<S, E>>,
    E: std::fmt::Display,
{
    let mut input = BufReader::new(input).lines();
    let mut state = SessionState::default();
    loop {
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut backend = BufReader::new(read_half).lines();
        loop {
            tokio::select! {
                read = input.next_line() => {
                    let Some(mut client_line) = read? else {
                        write_half.shutdown().await?;
                        return Ok(());
                    };
                    client_line.push('\n');
                    let client_line = state.observe_client(&client_line);
                    if write_half.write_all(client_line.as_bytes()).await.is_err()
                        || write_half.flush().await.is_err()
                    {
                        break;
                    }
                }
                read = backend.next_line() => {
                    let mut backend_line = match read {
                        Ok(Some(line)) => line,
                        Ok(None) | Err(_) => break,
                    };
                    backend_line.push('\n');
                    if state.observe_backend(&backend_line) {
                        output.write_all(backend_line.as_bytes()).await?;
                        output.flush().await?;
                    }
                }
                () = async {
                    match deadline.and_then(|limit| state.next_expiry(limit)) {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(limit) = deadline {
                        for error in state.expire(limit) {
                            output.write_all(error.as_bytes()).await?;
                            output.write_all(b"\n").await?;
                        }
                        output.flush().await?;
                    }
                }
            }
        }
        tracing::warn!(
            in_flight = state.pending.len(),
            "daemon relay backend disconnected; reconnecting"
        );
        for error in state.drain_restart_errors() {
            output.write_all(error.as_bytes()).await?;
            output.write_all(b"\n").await?;
        }
        output.flush().await?;

        let (mut replacement, initialize_response, buffered) = reconnect_and_replay(&mut reconnect, &mut state).await?;
        for line in state.take_replays() {
            // A write failure here just means the replacement died too; the read half sees EOF and
            // the loop reconnects, failing the (now already replayed) requests.
            if replacement.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
        let _ = replacement.flush().await;
        if let Some(response) = initialize_response {
            output.write_all(response.as_bytes()).await?;
        }
        for frame in buffered {
            output.write_all(frame.as_bytes()).await?;
            output.write_all(b"\n").await?;
        }
        output.flush().await?;
        tracing::info!("daemon relay backend reconnected");
        stream = replacement;
    }
}

async fn reconnect_and_replay<S, C, F, E>(
    reconnect: &mut C,
    state: &mut SessionState,
) -> io::Result<(S, Option<String>, Vec<String>)>
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: FnMut() -> F,
    F: Future<Output = Result<S, E>>,
    E: std::fmt::Display,
{
    let mut delay = INITIAL_RECONNECT_DELAY;
    loop {
        match reconnect().await {
            Ok(mut stream) => match replay_initialization(&mut stream, state).await {
                Ok((initialize_response, buffered)) => {
                    state.initialize_complete |= initialize_response.is_some();
                    return Ok((stream, initialize_response, buffered));
                }
                Err(error) => tracing::warn!(%error, "daemon reconnect initialization failed; retrying"),
            },
            Err(error) => tracing::debug!(%error, "daemon reconnect failed; retrying"),
        }
        tokio::time::sleep(delay).await;
        delay = delay.saturating_mul(2).min(MAX_RECONNECT_DELAY);
    }
}

async fn replay_initialization<S>(stream: &mut S, state: &SessionState) -> io::Result<(Option<String>, Vec<String>)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (Some(initialize), Some(initialize_id)) = (&state.initialize, &state.initialize_id) else {
        return Ok((None, Vec::new()));
    };
    stream.write_all(initialize.as_bytes()).await?;
    stream.flush().await?;

    let mut buffered = Vec::new();
    let response = loop {
        let frame = tokio::time::timeout(REPLAY_TIMEOUT, read_json_line(stream))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out replaying MCP initialize"))??;
        let message =
            serde_json::from_str::<Value>(&frame).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if message.get("id").filter(|id| !id.is_null()) == Some(initialize_id) {
            break frame;
        }
        buffered.push(frame);
    };
    if let Some(initialized) = &state.initialized {
        stream.write_all(initialized.as_bytes()).await?;
        stream.flush().await?;
    }
    if state.initialize_complete {
        Ok((None, buffered))
    } else {
        Ok((Some(response), buffered))
    }
}

async fn read_json_line<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<String> {
    let mut bytes = Vec::new();
    while bytes.len() < MAX_JSON_LINE_BYTES {
        let byte = stream.read_u8().await?;
        if byte == b'\n' {
            return String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        }
        bytes.push(byte);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "MCP JSON line exceeds relay limit",
    ))
}

#[cfg(test)]
mod tests {
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
        state.observe_client(&call_line(2, "code", "outline"));
        state.observe_client(&call_line(3, "agents", "inbox"));
        state.observe_client(&call_line(4, "agents", "thread_start"));
        state.observe_client(&call_line(5, "shell", "run"));
        state.observe_client(&call_line(6, "memory", "store"));
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
}
