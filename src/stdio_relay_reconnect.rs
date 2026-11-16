//! The relay with no backend: obtaining one, holding what the host sends meanwhile, and
//! answering it when the wait has gone on too long.

use std::future::Future;
use std::io;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader, Lines};

use super::{
    BACKEND_UNAVAILABLE_CODE, FATAL_ANSWER_WINDOW, FATAL_FIRST_REQUEST_WINDOW, HOLD_WINDOW, INITIAL_RECONNECT_DELAY,
    MAX_BLOCKED_RECONNECT_DELAY, MAX_JSON_LINE_BYTES, MAX_RECONNECT_DELAY, NOT_REACHED_YET, REPLAY_TIMEOUT,
    ReconnectError, ReconnectFailure, SessionState, TRANSIENT_WARN_AFTER, TRANSIENT_WARN_EVERY,
};

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
    /// Keep a host frame that arrived with no backend connected. Returns whether it is a request,
    /// which the host expects an answer to.
    fn hold_client(&mut self, line: String) -> bool {
        self.heard_from_host = true;
        let is_request = serde_json::from_str::<Value>(&line)
            .is_ok_and(|message| message.get("method").is_some() && message.get("id").is_some_and(|id| !id.is_null()));
        self.held.push_back(line);
        is_request
    }

    /// The frames kept by [`Self::hold_client`], oldest first.
    pub(super) fn take_held(&mut self) -> Vec<String> {
        self.held.drain(..).collect()
    }

    /// Whether the host is waiting on something only a backend can answer.
    fn awaits_backend(&self) -> bool {
        !self.pending.is_empty() || !self.held.is_empty() || (self.initialize_id.is_some() && !self.initialize_complete)
    }

    /// Fail every request still waiting for a backend, the ones held for replay and the ones that
    /// arrived with no backend included, with `reason`; `fatal` says no backend will ever come,
    /// which makes the errors final. An `initialize` still in flight is answered too and then
    /// forgotten, so a later backend is not asked to answer a request the host has already been
    /// given an error for.
    pub(super) fn drain_unavailable_errors(&mut self, reason: &str, fatal: bool) -> Vec<String> {
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
        let mut errors: Vec<String> = ids.iter().map(|id| unavailable_error(id, reason, !fatal)).collect();
        for line in self.take_held() {
            errors.extend(self.refuse_client(&line, reason, !fatal));
        }
        errors
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

/// How an attempt to replace the backend ended.
pub(super) enum Reconnected<S> {
    /// A replacement backend, initialized.
    Backend {
        stream: S,
        initialize_response: Option<String>,
        buffered: Vec<String>,
    },
    /// The host closed its end while the relay was connecting or waiting to retry.
    HostClosed,
    /// A failure no retry can cure, with the reason to give the host.
    Fatal(String),
}

/// What becomes of host requests while no backend is connected.
struct Hold {
    /// When the oldest waiting request stops waiting; `None` while nothing waits.
    until: Option<tokio::time::Instant>,
    /// Set once the window has ended: from then on a request is answered as it arrives.
    refusing: bool,
    /// Why the latest attempt failed, as the host is told.
    reason: String,
}

/// Obtain a backend, retrying for as long as a retry can help. The host's pipe is read throughout,
/// during an attempt as well as between attempts, so the relay ends as soon as the host closes it
/// and a request's [`HOLD_WINDOW`] runs from its arrival. Only the replay of MCP initialization on
/// a fresh connection, bounded by [`REPLAY_TIMEOUT`] per frame, goes unwatched.
pub(super) async fn reconnect_and_replay<R, W, S, C, F, E>(
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
    let mut transient_failures = 0u32;
    let mut hold = Hold {
        until: state
            .awaits_backend()
            .then(|| tokio::time::Instant::now() + HOLD_WINDOW),
        refusing: false,
        reason: NOT_REACHED_YET.to_owned(),
    };
    loop {
        let Some(attempt) = watching_host(reconnect(), input, output, state, &mut hold).await? else {
            return Ok(Reconnected::HostClosed);
        };
        let (reason, cap) = match attempt {
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
        hold.reason = reason;
        if watching_host(tokio::time::sleep(delay), input, output, state, &mut hold)
            .await?
            .is_none()
        {
            return Ok(Reconnected::HostClosed);
        }
        delay = delay.saturating_mul(2).min(cap);
    }
}

/// Run `work` with no backend connected while still serving the host's pipe. A frame that arrives
/// is kept, and starts the hold window if nothing was waiting; when the window ends everything
/// waiting is answered with the latest reason, and later requests are answered as they arrive.
/// Returns `None`, dropping `work`, when the host closes its end.
async fn watching_host<T, R, W>(
    work: impl Future<Output = T>,
    input: &mut Lines<BufReader<R>>,
    output: &mut W,
    state: &mut SessionState,
    hold: &mut Hold,
) -> io::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    tokio::pin!(work);
    loop {
        let window_end = hold.until.filter(|_| !hold.refusing);
        tokio::select! {
            done = &mut work => return Ok(Some(done)),
            read = input.next_line() => {
                let Some(line) = read? else {
                    return Ok(None);
                };
                if hold.refusing {
                    if let Some(error) = state.refuse_client(&line, &hold.reason, true) {
                        write_lines(output, vec![error]).await?;
                    }
                } else if state.hold_client(line) {
                    hold.until.get_or_insert_with(|| tokio::time::Instant::now() + HOLD_WINDOW);
                }
            }
            () = async {
                match window_end {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                hold.refusing = true;
                write_lines(output, state.drain_unavailable_errors(&hold.reason, false)).await?;
            }
        }
    }
}

pub(super) async fn write_lines<W: AsyncWrite + Unpin>(output: &mut W, lines: Vec<String>) -> io::Result<()> {
    for line in lines {
        output.write_all(line.as_bytes()).await?;
        output.write_all(b"\n").await?;
    }
    output.flush().await
}

/// After a fatal failure, give the host the reason on what it sends next: for
/// [`FATAL_ANSWER_WINDOW`], or, when it has sent nothing yet, until that long after its first frame
/// and at most [`FATAL_FIRST_REQUEST_WINDOW`].
pub(super) async fn answer_before_ending<R, W>(
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
