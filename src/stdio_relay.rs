//! Protocol-aware stdio relay that can replace a failed daemon connection without closing the
//! MCP host's stdin/stdout pipes.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader, Lines};

const BACKEND_RESTARTED_CODE: i64 = -32001;
/// JSON-RPC code answering a request while no backend can be reached; the message says why.
const BACKEND_UNAVAILABLE_CODE: i64 = -32003;
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(250);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// Cap on the back-off while something outside this process keeps the backend away.
const MAX_BLOCKED_RECONNECT_DELAY: Duration = Duration::from_secs(60);
/// How long the relay keeps answering the host after a fatal reconnect failure, so a request
/// already on its way gets the reason rather than a closed pipe.
const FATAL_ANSWER_WINDOW: Duration = Duration::from_secs(1);
/// How long a host that has sent nothing yet is given to send its first request after a fatal
/// failure. A session that cannot start still owes `initialize` the reason.
const FATAL_FIRST_REQUEST_WINDOW: Duration = Duration::from_secs(10);
/// How long requests wait for a backend that keeps failing to come back: the ones held for replay
/// stay held and the host's pipe stays unread, so everything goes through if a backend arrives.
/// Past it, held and arriving requests are answered with the reason while the retries go on.
const HOLD_WINDOW: Duration = Duration::from_secs(10);
/// Failed attempts in a row after which a restarting backend is reported at warn, and how often
/// after that.
const TRANSIENT_WARN_AFTER: u32 = 4;
const TRANSIENT_WARN_EVERY: u32 = 12;
/// The least time between two reconnect attempts that ask an older daemon to stop.
const STOP_REQUEST_INTERVAL: Duration = Duration::from_secs(60);

/// What a failed reconnect means for the relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReconnectFailure {
    /// The backend is restarting; retry soon.
    Transient,
    /// Something outside this process must change first, such as an older daemon that will not
    /// stop. Retried with a long back-off; once [`HOLD_WINDOW`] has passed, host requests are
    /// refused with the reason meanwhile.
    Blocked,
    /// No retry can succeed. The relay answers what is pending with the reason and ends.
    Fatal,
}

/// An error a reconnect attempt can fail with.
pub(crate) trait ReconnectError: std::fmt::Display {
    /// How the relay should treat this failure.
    fn failure(&self) -> ReconnectFailure {
        ReconnectFailure::Transient
    }

    /// The text given to the host and the log.
    fn reason(&self) -> String {
        self.to_string()
    }
}

impl ReconnectError for &str {}

/// The daemon answered the relay handshake and turned this session down.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RelayRefusal {
    /// The daemon and this binary speak different relay protocols.
    #[error(
        "the basemind daemon (v{daemon_version}) speaks relay protocol {daemon} and this basemind \
         speaks {client}; {}",
        if daemon > client {
            "update this basemind, or stop that daemon with `basemind comms stop` once its sessions are done"
        } else {
            "the daemon is from an older build and is replaced once its sessions end; `basemind comms stop` \
             stops it now"
        }
    )]
    ProtoMismatch {
        /// The relay protocol the daemon reported.
        daemon: u32,
        /// This binary's relay protocol.
        client: u32,
        /// The daemon's build version.
        daemon_version: String,
    },
    /// The daemon declined for a reason of its own.
    #[error("the basemind daemon declined this session: {code}")]
    Declined {
        /// The daemon's stable refusal token.
        code: String,
    },
}

impl RelayRefusal {
    fn failure(&self) -> ReconnectFailure {
        match self {
            // A newer daemon comes back newer however often this binary retries.
            Self::ProtoMismatch { daemon, client, .. } if daemon > client => ReconnectFailure::Fatal,
            Self::ProtoMismatch { .. } => ReconnectFailure::Blocked,
            // The daemon judged what this session asked for, which a retry repeats unchanged.
            Self::Declined { .. } => ReconnectFailure::Fatal,
        }
    }
}

impl ReconnectError for anyhow::Error {
    fn failure(&self) -> ReconnectFailure {
        use basemind::comms::singleton::SingletonError;
        if let Some(refusal) = self.chain().find_map(|cause| cause.downcast_ref::<RelayRefusal>()) {
            return refusal.failure();
        }
        match self.chain().find_map(|cause| cause.downcast_ref::<SingletonError>()) {
            // Nothing a retry does changes the installed version, the path or the platform.
            Some(SingletonError::NewerDaemon { .. } | SingletonError::NoDataDir) => ReconnectFailure::Fatal,
            #[cfg(unix)]
            Some(SingletonError::SocketPathTooLong { .. }) => ReconnectFailure::Fatal,
            // These clear when a daemon stops, exits or is started by whoever owns that.
            Some(
                SingletonError::BusyPredecessor { .. }
                | SingletonError::StalePredecessor { .. }
                | SingletonError::FailingPredecessor { .. }
                | SingletonError::TooManyDaemons { .. }
                | SingletonError::AutospawnDisabled { .. },
            ) => ReconnectFailure::Blocked,
            _ => ReconnectFailure::Transient,
        }
    }

    fn reason(&self) -> String {
        format!("{self:#}")
    }
}

/// Decides which reconnect attempts may ask an older daemon to stop: any until one has sent
/// `Stop`, and then one per [`STOP_REQUEST_INTERVAL`], so a daemon that declined is not sent `Stop`
/// on every retry. Only a `Stop` that was sent counts; an attempt that found nothing to ask does
/// not use up the interval.
#[derive(Debug, Default)]
pub(crate) struct StopPacing {
    last_asked: Option<std::time::Instant>,
}

impl StopPacing {
    /// Whether the attempt starting now may ask.
    pub(crate) fn may_ask(&self) -> bool {
        self.may_ask_at(std::time::Instant::now())
    }

    /// Record that an attempt sent `Stop` just now.
    pub(crate) fn asked(&mut self) {
        self.asked_at(std::time::Instant::now());
    }

    fn may_ask_at(&self, now: std::time::Instant) -> bool {
        self.last_asked
            .is_none_or(|asked| now.duration_since(asked) >= STOP_REQUEST_INTERVAL)
    }

    fn asked_at(&mut self, now: std::time::Instant) {
        self.last_asked = Some(now);
    }
}
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
/// Verb tools that write, and so must never be re-sent: every other verb tool is a read.
const WRITING_TOOLS: &[&str] = &["memory_save", "index_rescan"];

/// Whether `tool` is a read-only verb tool, safe to replay after a backend restart.
fn is_read_only_tool(tool: &str) -> bool {
    basemind::mcp::params::VERB_TOOLS.contains(&tool) && !WRITING_TOOLS.contains(&tool)
}
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
    if is_read_only_tool(tool) || (tool == "agents" && REPLAYABLE_AGENTS_MODES.contains(&mode)) {
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
    /// Whether the host has sent anything yet.
    heard_from_host: bool,
}

impl SessionState {
    /// Track a host frame; returns the line to forward (rewritten for a keyless `post`).
    fn observe_client(&mut self, line: &str) -> String {
        self.heard_from_host = true;
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

fn unavailable_error(id: &Value, reason: &str, retryable: bool) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": BACKEND_UNAVAILABLE_CODE,
            "message": format!("backend_unavailable: {reason}"),
            "data": { "retryable": retryable }
        }
    })
    .to_string()
}

impl SessionState {
    /// Fail every request still waiting for a backend, the ones held for replay included, with
    /// `reason`; `fatal` says no backend will ever come, which makes the errors final. An
    /// `initialize` still in flight is answered too and then forgotten, so a later backend is not
    /// asked to answer a request the host has already been given an error for.
    fn drain_unavailable_errors(&mut self, reason: &str, fatal: bool) -> Vec<String> {
        self.replay.clear();
        self.started.clear();
        let mut ids: Vec<Value> = std::mem::take(&mut self.pending).into_values().collect();
        if !self.initialize_complete
            && let Some(id) = self.initialize_id.take()
        {
            self.initialize = None;
            self.initialized = None;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids.iter().map(|id| unavailable_error(id, reason, !fatal)).collect()
    }

    /// The answer to a host frame that arrives while no backend is connected: an error for a
    /// request, nothing for a notification. `notifications/initialized` is still remembered, since
    /// a later backend needs it replayed.
    fn refuse_client(&mut self, line: &str, reason: &str, retryable: bool) -> Option<String> {
        self.heard_from_host = true;
        let message = serde_json::from_str::<Value>(line).ok()?;
        let method = message.get("method").and_then(Value::as_str)?;
        if method == "notifications/initialized" {
            self.initialized = Some(format!("{line}\n"));
            return None;
        }
        let id = message.get("id").filter(|id| !id.is_null())?;
        Some(unavailable_error(id, reason, retryable))
    }
}

/// Relay newline-framed MCP messages until the client closes stdin. Every backend, the first
/// included, comes from `reconnect`, so a daemon that cannot be reached when the session starts is
/// handled exactly like one lost later: the host's pipes stay open and its requests are answered.
/// When the backend disappears, every in-flight request that cannot be replayed fails once with
/// `backend_restarted`; the proxy reconnects, silently replays MCP initialization, and forwards
/// later requests over the replacement connection.
pub(crate) async fn run<R, W, S, C, F, E>(input: R, output: W, reconnect: C) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
    C: FnMut() -> F,
    F: Future<Output = Result<S, E>>,
    E: ReconnectError,
{
    run_with_deadline(input, output, reconnect, request_deadline_from_env()).await
}

/// [`run`] with an explicit per-request `deadline`: a request still unanswered after it gets a
/// retryable `backend_timeout` error instead of hanging the host silently.
async fn run_with_deadline<R, W, S, C, F, E>(
    input: R,
    mut output: W,
    mut reconnect: C,
    deadline: Option<Duration>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
    C: FnMut() -> F,
    F: Future<Output = Result<S, E>>,
    E: ReconnectError,
{
    let mut input = BufReader::new(input).lines();
    let mut state = SessionState::default();
    let mut connected_before = false;
    loop {
        let reconnected = reconnect_and_replay(&mut reconnect, &mut state, &mut input, &mut output).await?;
        let (mut stream, initialize_response, buffered) = match reconnected {
            Reconnected::Backend {
                stream,
                initialize_response,
                buffered,
            } => (stream, initialize_response, buffered),
            Reconnected::HostClosed => return Ok(()),
            Reconnected::Fatal(reason) => {
                tracing::error!(%reason, "daemon relay cannot connect; ending the session");
                write_lines(&mut output, state.drain_unavailable_errors(&reason, true)).await?;
                answer_before_ending(&mut input, &mut output, &mut state, &reason).await?;
                return Err(io::Error::other(reason));
            }
        };
        for line in state.take_replays() {
            // A write failure here just means the replacement died too; the read half sees EOF and
            // the loop reconnects, failing the (now already replayed) requests.
            if stream.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
        let _ = stream.flush().await;
        if let Some(response) = initialize_response {
            output.write_all(response.as_bytes()).await?;
            output.write_all(b"\n").await?;
        }
        for frame in buffered {
            output.write_all(frame.as_bytes()).await?;
            output.write_all(b"\n").await?;
        }
        output.flush().await?;
        if connected_before {
            tracing::info!("daemon relay backend reconnected");
        }
        connected_before = true;

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
        write_lines(&mut output, state.drain_restart_errors()).await?;
    }
}

/// How an attempt to replace the backend ended.
enum Reconnected<S> {
    /// A replacement backend, initialized.
    Backend {
        stream: S,
        initialize_response: Option<String>,
        buffered: Vec<String>,
    },
    /// The host closed its end while the relay was waiting to retry.
    HostClosed,
    /// A failure no retry can cure, with the reason to give the host.
    Fatal(String),
}

/// Obtain a backend, retrying for as long as a retry can help. For [`HOLD_WINDOW`] after the first
/// failure nothing is answered: the requests held for replay stay held and the host's pipe is left
/// unread, so a backend that arrives in time gets all of it. After that the held requests and
/// every request that arrives are answered with the reason for the latest failure.
async fn reconnect_and_replay<R, W, S, C, F, E>(
    reconnect: &mut C,
    state: &mut SessionState,
    input: &mut Lines<BufReader<R>>,
    output: &mut W,
) -> io::Result<Reconnected<S>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
    C: FnMut() -> F,
    F: Future<Output = Result<S, E>>,
    E: ReconnectError,
{
    let mut delay = INITIAL_RECONNECT_DELAY;
    let mut hold_until = None;
    let mut transient_failures = 0u32;
    loop {
        let (reason, cap) = match reconnect().await {
            Ok(mut stream) => match replay_initialization(&mut stream, state).await {
                Ok((initialize_response, buffered)) => {
                    state.initialize_complete |= initialize_response.is_some();
                    return Ok(Reconnected::Backend {
                        stream,
                        initialize_response,
                        buffered,
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "daemon relay initialization failed; retrying");
                    (
                        format!("the daemon did not complete MCP initialization: {error}"),
                        MAX_RECONNECT_DELAY,
                    )
                }
            },
            Err(error) => match error.failure() {
                ReconnectFailure::Fatal => return Ok(Reconnected::Fatal(error.reason())),
                ReconnectFailure::Blocked => {
                    let reason = error.reason();
                    tracing::warn!(%reason, retry_in_ms = delay.as_millis(), "daemon relay is blocked; retrying");
                    (reason, MAX_BLOCKED_RECONNECT_DELAY)
                }
                ReconnectFailure::Transient => {
                    transient_failures += 1;
                    let reason = error.reason();
                    if transient_failures == TRANSIENT_WARN_AFTER
                        || transient_failures.is_multiple_of(TRANSIENT_WARN_EVERY)
                    {
                        tracing::warn!(%reason, attempts = transient_failures, "daemon relay still cannot connect; retrying");
                    } else {
                        tracing::debug!(%reason, "daemon relay connect failed; retrying");
                    }
                    (reason, MAX_RECONNECT_DELAY)
                }
            },
        };
        let wake = tokio::time::Instant::now() + delay;
        let hold_until = *hold_until.get_or_insert_with(|| tokio::time::Instant::now() + HOLD_WINDOW);
        tokio::time::sleep_until(wake.min(hold_until)).await;
        if wake >= hold_until {
            write_lines(output, state.drain_unavailable_errors(&reason, false)).await?;
            if !refuse_until(input, output, state, wake, &reason, true, None).await? {
                return Ok(Reconnected::HostClosed);
            }
        }
        delay = delay.saturating_mul(2).min(cap);
    }
}

async fn write_lines<W: AsyncWrite + Unpin>(output: &mut W, lines: Vec<String>) -> io::Result<()> {
    for line in lines {
        output.write_all(line.as_bytes()).await?;
        output.write_all(b"\n").await?;
    }
    output.flush().await
}

/// After a fatal failure, give the host the reason on what it sends next: for
/// [`FATAL_ANSWER_WINDOW`], or, when it has sent nothing yet, until that long after its first frame
/// and at most [`FATAL_FIRST_REQUEST_WINDOW`].
async fn answer_before_ending<R, W>(
    input: &mut Lines<BufReader<R>>,
    output: &mut W,
    state: &mut SessionState,
    reason: &str,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (window, after_first) = if state.heard_from_host {
        (FATAL_ANSWER_WINDOW, None)
    } else {
        (FATAL_FIRST_REQUEST_WINDOW, Some(FATAL_ANSWER_WINDOW))
    };
    let wake = tokio::time::Instant::now() + window;
    refuse_until(input, output, state, wake, reason, false, after_first).await?;
    Ok(())
}

/// Wait until `wake` with no backend, answering each host request that arrives with `reason`
/// instead of leaving it unread. With `after_first`, the wait ends that long after the first frame
/// if that is sooner. Returns `false` when the host closed its end.
async fn refuse_until<R, W>(
    input: &mut Lines<BufReader<R>>,
    output: &mut W,
    state: &mut SessionState,
    mut wake: tokio::time::Instant,
    reason: &str,
    retryable: bool,
    mut after_first: Option<Duration>,
) -> io::Result<bool>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(wake) => return Ok(true),
            read = input.next_line() => {
                let Some(line) = read? else {
                    return Ok(false);
                };
                if let Some(error) = state.refuse_client(&line, reason, retryable) {
                    write_lines(output, vec![error]).await?;
                }
                if let Some(grace) = after_first.take() {
                    wake = wake.min(tokio::time::Instant::now() + grace);
                }
            }
        }
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
#[path = "stdio_relay_tests.rs"]
mod tests;
