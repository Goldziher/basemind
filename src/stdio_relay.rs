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

#[derive(Default)]
struct SessionState {
    initialize: Option<String>,
    initialized: Option<String>,
    initialize_id: Option<Value>,
    initialize_complete: bool,
    pending: BTreeMap<String, Value>,
}

impl SessionState {
    fn observe_client(&mut self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").filter(|id| !id.is_null());
        if method == Some("initialize") {
            self.initialize = Some(line.to_owned());
            self.initialize_id = id.cloned();
        } else if method == Some("notifications/initialized") {
            self.initialized = Some(line.to_owned());
        }
        if method.is_some()
            && let Some(id) = id
        {
            self.pending.insert(id_key(id), id.clone());
        }
    }

    fn observe_backend(&mut self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if message.get("method").is_none()
            && let Some(id) = message.get("id").filter(|id| !id.is_null())
        {
            self.pending.remove(&id_key(id));
            if self.initialize_id.as_ref() == Some(id) {
                self.initialize_complete = true;
            }
        }
    }

    fn drain_restart_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending)
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
pub(crate) async fn run<R, W, S, C, F, E>(input: R, mut output: W, mut stream: S, mut reconnect: C) -> io::Result<()>
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
                    state.observe_client(&client_line);
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
                    state.observe_backend(&backend_line);
                    output.write_all(backend_line.as_bytes()).await?;
                    output.flush().await?;
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

        let (replacement, initialize_response, buffered) = reconnect_and_replay(&mut reconnect, &mut state).await?;
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
            .write_all(request_line(2).as_bytes())
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
