//! Helper bodies for the headless agent-shell MCP tools.
//!
//! Each helper resolves the embedded [`crate::shells::ShellRuntime`] off
//! `ServerState`, drives one rmux operation (spawn / send / capture / kill), and
//! returns a JSON [`CallToolResult`]. The whole module is gated on
//! `feature = "shells"`.

#![cfg(feature = "shells")]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::json_result;
use super::types_shells::{
    ShellCaptureParams, ShellCaptureResponse, ShellKillParams, ShellKillResponse, ShellSendParams,
    ShellSpawnParams, ShellSpawnResponse,
};
use crate::shells::SessionId;
use crate::shells::session::ShellCommand;

/// Map an internal `anyhow` failure to an MCP internal error with a prefix.
fn mcp_internal(prefix: &str, err: impl std::fmt::Display) -> McpError {
    McpError::internal_error(format!("{prefix}: {err}"), None)
}

/// Reconstruct a [`SessionId`] from the caller-supplied string. The id is opaque
/// to the client, so any non-empty string is accepted here and resolution
/// against the in-process map decides validity.
fn parse_session_id(raw: &str) -> Result<SessionId, McpError> {
    if raw.trim().is_empty() {
        return Err(McpError::invalid_params(
            "session_id must not be empty",
            None,
        ));
    }
    Ok(SessionId::new(raw))
}

/// Resolve a `session_id` to its rmux session name, erroring when unknown.
async fn require_session(
    state: &ServerState,
    raw: &str,
) -> Result<(SessionId, rmux_sdk::SessionName), McpError> {
    let id = parse_session_id(raw)?;
    match state.shell_runtime.resolve(&id).await {
        Some(name) => Ok((id, name)),
        None => Err(McpError::invalid_params(
            format!("unknown session_id {raw:?}; it may have been killed or never existed"),
            None,
        )),
    }
}

/// `shell_spawn`: create a detached headless shell session.
pub(super) async fn run_shell_spawn(
    state: &ServerState,
    params: ShellSpawnParams,
) -> Result<CallToolResult, McpError> {
    let cwd = match params.cwd {
        Some(rel) => Some(
            rel.as_str()
                .ok_or_else(|| McpError::invalid_params("cwd is not valid UTF-8", None))?
                .to_string(),
        ),
        None => None,
    };
    let environment: Vec<String> = params
        .env
        .unwrap_or_default()
        .into_iter()
        .map(|kv| format!("{}={}", kv.key, kv.value))
        .collect();

    let (session_id, name) = state
        .shell_runtime
        .spawn(ShellCommand::Shell(params.command), cwd, environment)
        .await
        .map_err(|e| mcp_internal("spawn shell session", e))?;

    let response = ShellSpawnResponse {
        session_id: session_id.to_string(),
        attach_command: format!("rmux attach -t {}", name.as_str()),
    };
    json_result(&response)
}

/// `shell_send`: write text (optionally with a newline) to a session's stdin.
pub(super) async fn run_shell_send(
    state: &ServerState,
    params: ShellSendParams,
) -> Result<CallToolResult, McpError> {
    let (id, name) = require_session(state, &params.session_id).await?;
    let session = state
        .shell_runtime
        .rmux()
        .await
        .map_err(|e| mcp_internal("connect embedded shell daemon", e))?
        .session(name)
        .await
        .map_err(|e| mcp_internal("open shell session", e))?;
    crate::shells::session::send_text(&session, &params.text, params.enter)
        .await
        .map_err(|e| mcp_internal("send to shell session", e))?;
    json_result(&serde_json::json!({ "session_id": id.to_string(), "sent": true }))
}

/// `shell_capture`: return the visible screen text of a session's primary pane.
pub(super) async fn run_shell_capture(
    state: &ServerState,
    params: ShellCaptureParams,
) -> Result<CallToolResult, McpError> {
    let (_id, name) = require_session(state, &params.session_id).await?;
    let session = state
        .shell_runtime
        .rmux()
        .await
        .map_err(|e| mcp_internal("connect embedded shell daemon", e))?
        .session(name)
        .await
        .map_err(|e| mcp_internal("open shell session", e))?;
    let text = crate::shells::session::capture(&session, params.lines)
        .await
        .map_err(|e| mcp_internal("capture shell output", e))?;
    json_result(&ShellCaptureResponse { text })
}

/// `shell_kill`: terminate a session and forget its mapping.
pub(super) async fn run_shell_kill(
    state: &ServerState,
    params: ShellKillParams,
) -> Result<CallToolResult, McpError> {
    let (id, name) = require_session(state, &params.session_id).await?;
    let session = state
        .shell_runtime
        .rmux()
        .await
        .map_err(|e| mcp_internal("connect embedded shell daemon", e))?
        .session(name)
        .await
        .map_err(|e| mcp_internal("open shell session", e))?;
    let killed = crate::shells::session::kill_session(&session)
        .await
        .map_err(|e| mcp_internal("kill shell session", e))?;
    state.shell_runtime.forget(&id).await;
    json_result(&ShellKillResponse {
        session_id: id.to_string(),
        killed,
    })
}
