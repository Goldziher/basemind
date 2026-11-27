//! A CLI command's call forwarded whole to the daemon (ADR-0039).
//!
//! With the daemon up it holds the workspace's index, so a CLI command does not open the store: it
//! opens one relay session as a [`RelayClient::Cli`], sends the call as `tools/call {name,
//! arguments}`, and renders the `CallToolResult` the daemon's own tool body answered. The session
//! answers a failed call as a JSON-RPC error, which keeps its code, so the exit status is the one
//! the in-process call would give. The exchange is newline-delimited JSON-RPC, as rmcp's stream
//! transport writes it.

use std::path::Path;

use anyhow::{Context, Result};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use super::exit::CliExit;
use crate::comms::relay::{self, RelayClient, RelayHello, RelayWelcome};
use crate::comms::relay_connect::{RelayStream, connect_stream};

/// The request id of `initialize`; the forwarded call is the next one.
const INITIALIZE_ID: u64 = 0;
const CALL_ID: u64 = 1;

/// One relay session to the daemon, carrying this process's tool call.
pub(crate) struct DaemonSession {
    stream: Mutex<BufReader<RelayStream>>,
}

impl DaemonSession {
    /// Open a CLI relay session for the working view of `root` and initialize it. A daemon that
    /// declines, or one too old to take a CLI session, is an error naming what to do.
    pub(crate) async fn open(root: &Path) -> Result<Self> {
        let paths = crate::comms::singleton::resolve_paths().context("resolve comms paths")?;
        let identity =
            crate::comms::v2::identity::resolve_for_root(root).context("resolve this command's agent identity")?;
        let mut stream = connect_stream(&paths.socket_path)
            .await
            .context("connect to the daemon socket")?;
        let hello = RelayHello {
            relay_proto_ver: relay::RELAY_PROTO_VER,
            root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
            view: crate::store::VIEW_WORKING.to_string(),
            principal: identity.principal.clone(),
            host: std::env::var(crate::comms::v2::identity::HOST_ENV)
                .ok()
                .filter(|host| !host.is_empty()),
            notices: identity.notices(),
            client: RelayClient::Cli,
        };
        let welcome = relay::client_handshake(&mut stream, &hello)
            .await
            .context("relay handshake with the daemon")?;
        refusal(&welcome)?;
        let session = Self {
            stream: Mutex::new(BufReader::new(stream)),
        };
        session.initialize().await?;
        Ok(session)
    }

    async fn initialize(&self) -> Result<()> {
        let mut stream = self.stream.lock().await;
        let request = json!({
            "jsonrpc": "2.0",
            "id": INITIALIZE_ID,
            "method": "initialize",
            "params": {
                "protocolVersion": rmcp::model::ProtocolVersion::LATEST,
                "capabilities": {},
                "clientInfo": { "name": "basemind-cli", "version": env!("CARGO_PKG_VERSION") },
            },
        });
        send(&mut stream, &request).await?;
        let response = reply(&mut stream, INITIALIZE_ID).await?;
        if let Some(error) = response.get("error") {
            anyhow::bail!("the daemon refused to initialize this command's session: {error}");
        }
        send(
            &mut stream,
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        )
        .await
    }

    /// Call `name` with `arguments` and return what the daemon's tool body answered: its result, or
    /// its error with the code the CLI maps to an exit status.
    pub(crate) async fn call(&self, name: &str, arguments: Value) -> Result<CallToolResult, McpError> {
        let mut stream = self.stream.lock().await;
        let request = json!({
            "jsonrpc": "2.0",
            "id": CALL_ID,
            "method": "tools/call",
            "params": { "name": name, "arguments": without_nulls(arguments) },
        });
        let lost = |error: anyhow::Error| McpError::internal_error(format!("daemon relay: {error:#}"), None);
        send(&mut stream, &request).await.map_err(lost)?;
        let mut response = reply(&mut stream, CALL_ID).await.map_err(lost)?;
        if let Some(error) = response.get_mut("error").map(Value::take) {
            return Err(serde_json::from_value(error).unwrap_or_else(|error| {
                McpError::internal_error(format!("daemon relay: unreadable error: {error}"), None)
            }));
        }
        let result = response.get_mut("result").map(Value::take).unwrap_or(Value::Null);
        serde_json::from_value(result)
            .map_err(|error| McpError::internal_error(format!("daemon relay: unreadable result: {error}"), None))
    }
}

/// Why the daemon's welcome does not let this command forward, as the error the CLI exits with.
fn refusal(welcome: &RelayWelcome) -> Result<()> {
    if welcome.relay_proto_ver != relay::RELAY_PROTO_VER || (welcome.accepted && !welcome.cli) {
        return Err(CliExit::unavailable(format!(
            "the running basemind daemon ({}) cannot answer a command from this basemind ({}); \
             `basemind comms stop` lets this version's daemon take over",
            welcome.daemon_version,
            env!("CARGO_PKG_VERSION")
        )));
    }
    if welcome.accepted {
        return Ok(());
    }
    let code = welcome.code.as_deref().unwrap_or("unspecified");
    let detail = welcome
        .detail
        .as_deref()
        .map(|detail| format!(": {detail}"))
        .unwrap_or_default();
    let message = format!("the basemind daemon declined this command ({code}){detail}");
    Err(match code {
        relay::decline::WORKSPACE_LOCKED | relay::decline::WORKSPACE_BUSY | relay::decline::DAEMON_DRAINING => {
            CliExit::busy(message)
        }
        _ => anyhow::anyhow!(message),
    })
}

/// A client leaves an unset argument out rather than sending `null`.
fn without_nulls(arguments: Value) -> Value {
    match arguments {
        Value::Object(map) => Value::Object(map.into_iter().filter(|(_, value)| !value.is_null()).collect()),
        other => other,
    }
}

async fn send(stream: &mut BufReader<RelayStream>, message: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(message).context("encode a JSON-RPC message")?;
    line.push(b'\n');
    let stream = stream.get_mut();
    stream.write_all(&line).await.context("write to the daemon")?;
    stream.flush().await.context("write to the daemon")
}

/// The response to request `id`; anything else the daemon sends first (a notification or a request
/// of its own) is skipped.
async fn reply(stream: &mut BufReader<RelayStream>, id: u64) -> Result<Value> {
    let mut line = String::new();
    loop {
        line.clear();
        if stream.read_line(&mut line).await.context("read from the daemon")? == 0 {
            anyhow::bail!("the daemon closed the session before answering");
        }
        let Ok(message) = serde_json::from_str::<Value>(line.trim_end()) else {
            continue;
        };
        if is_reply_to(&message, id) {
            return Ok(message);
        }
    }
}

/// Whether `message` answers request `id`: a response, not a request of the daemon's own, whose id
/// is drawn from the daemon's sequence and may equal the call's.
fn is_reply_to(message: &Value, id: u64) -> bool {
    message.get("method").is_none() && message.get("id").and_then(Value::as_u64) == Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn welcome(accepted: bool, cli: bool, code: Option<&str>) -> RelayWelcome {
        RelayWelcome {
            relay_proto_ver: relay::RELAY_PROTO_VER,
            daemon_version: "0.33.0".to_string(),
            accepted,
            code: code.map(str::to_string),
            detail: None,
            cli,
        }
    }

    /// The daemon may send a request of its own (`roots/list`, `sampling/createMessage`) whose id
    /// equals the call's: JSON-RPC ids are per sender. Only a response answers the call.
    #[test]
    fn a_server_request_with_the_calls_id_is_not_its_reply() {
        let request = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "roots/list" });
        assert!(!is_reply_to(&request, 1));
        let result = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": {} });
        assert!(is_reply_to(&result, 1));
        let error = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32603, "message": "x" } });
        assert!(is_reply_to(&error, 1));
        assert!(!is_reply_to(&result, 2));
        let notification = serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/progress" });
        assert!(!is_reply_to(&notification, 1));
    }

    /// An older daemon accepts a CLI's hello as an agent session and does not acknowledge the kind;
    /// the command refuses rather than reading `isError` answers as successes.
    #[test]
    fn a_daemon_that_does_not_acknowledge_the_cli_kind_is_refused_as_unavailable() {
        let error = refusal(&welcome(true, false, None)).expect_err("an older daemon");
        assert_eq!(
            super::super::exit::exit_code_for(&error),
            super::super::exit::UNAVAILABLE
        );
        assert_eq!(
            error.to_string(),
            format!(
                "the running basemind daemon (0.33.0) cannot answer a command from this basemind ({}); \
                 `basemind comms stop` lets this version's daemon take over",
                env!("CARGO_PKG_VERSION")
            )
        );
        refusal(&welcome(true, true, None)).expect("a daemon that took the CLI session");
    }

    /// A forwarded failure crosses the relay as a JSON-RPC error and keeps its code and data, so the
    /// exit status is the in-process one: usage `2`, busy `3`.
    #[test]
    fn a_forwarded_error_keeps_the_exit_status_the_in_process_one_gives() {
        let over_the_relay = |error: McpError| -> McpError {
            serde_json::from_value(serde_json::to_value(&error).expect("encode")).expect("decode")
        };
        let usage = super::super::run_tool(
            "code_outline",
            Err(over_the_relay(McpError::invalid_params("bad path", None))),
        )
        .expect_err("refused");
        assert_eq!(super::super::exit::exit_code_for(&usage), super::super::exit::USAGE);
        assert_eq!(usage.to_string(), "code_outline: bad path");

        let busy = rmcp::ErrorData::from(crate::engine::scan::rescan_error(&crate::scanner::ScanError::Store(
            crate::store::StoreError::Locked {
                path: std::path::PathBuf::from(".lock"),
                holder: None,
            },
        )));
        let busy = super::super::run_tool("rescan", Err(over_the_relay(busy))).expect_err("refused");
        assert_eq!(super::super::exit::exit_code_for(&busy), super::super::exit::BUSY);

        let overloaded = rmcp::ErrorData::invalid_request(
            "busy: basemind is running as many calls of this kind as it allows and this one was not started.",
            Some(serde_json::json!({ crate::engine::BUSY_DATA: true })),
        );
        let overloaded = super::super::run_tool("code_grep", Err(over_the_relay(overloaded))).expect_err("refused");
        assert_eq!(super::super::exit::exit_code_for(&overloaded), super::super::exit::BUSY);
    }

    #[test]
    fn a_decline_that_clears_on_its_own_exits_busy() {
        let busy = refusal(&welcome(false, false, Some(relay::decline::WORKSPACE_LOCKED))).expect_err("declined");
        assert_eq!(super::super::exit::exit_code_for(&busy), super::super::exit::BUSY);
        let failed = refusal(&welcome(false, false, Some(relay::decline::ROOT_REFUSED))).expect_err("declined");
        assert_eq!(super::super::exit::exit_code_for(&failed), super::super::exit::ERROR);
        assert_eq!(
            failed.to_string(),
            "the basemind daemon declined this command (root_refused)"
        );
    }
}
