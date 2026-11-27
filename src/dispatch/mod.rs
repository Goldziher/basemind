//! What the two adapters of the engine share: the MCP server (`src/mcp`) and the `basemind` commands
//! (`src/cli`).
//!
//! An engine operation ([`Op`]) is synchronous and names no rmcp type (ADR-0039). This module runs
//! one on a blocking thread, binds its cursor to the query, records its telemetry row, words its
//! error as MCP words errors and encodes its answer as the tool's wire form. The server's `#[tool]`
//! shims and the commands reach an operation through the same functions here, so the two answer from
//! the same body. `LocalEngine` is the in-process handle a command runs them on.

pub(crate) mod history_sync;
#[cfg(feature = "memory")]
pub(crate) mod identity;
mod local;
mod open;
mod record;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::{Value, json};

use crate::engine::error_kind::{ERROR_KEY, ErrorKind};
use crate::engine::paging::{Paged, ToolArgs, bound_value};
use crate::engine::telemetry::Telemetry;
use crate::engine::{BUSY_DATA, EngineError, Workspace};

pub(crate) use local::LocalEngine;
pub(crate) use open::{open_git_history, open_options};
pub(crate) use record::record_call_to;

/// One engine operation: a verb tool's body over a [`Workspace`].
pub(crate) type Op<P, R> = fn(&Workspace, P) -> Result<R, EngineError>;

/// JSON-RPC code for a request the client cancelled (the LSP `RequestCancelled` code).
pub(crate) const REQUEST_CANCELLED_CODE: i32 = -32800;

/// The error of a request the client cancelled.
pub(crate) fn request_cancelled() -> McpError {
    McpError::new(
        rmcp::model::ErrorCode(REQUEST_CANCELLED_CODE),
        "request_cancelled",
        None,
    )
}

/// The `data` that marks an error as `kind`, for a kind its JSON-RPC code does not tell apart.
pub(crate) fn kind_data(kind: ErrorKind) -> Value {
    json!({ ERROR_KEY: kind.code() })
}

/// A wrapped store, index, git or I/O error is first worded for the caller (`EngineError::settled`);
/// then each variant keeps its message, and the code is what the CLI turns into an exit status. An
/// unavailable index shares its JSON-RPC code with an unavailable operation, so its `data` names its
/// `ErrorKind`.
pub(crate) fn mcp_error(error: EngineError) -> McpError {
    match error.settled() {
        EngineError::InvalidArgument(message) => McpError::invalid_params(message, None),
        EngineError::Unavailable(message) => McpError::invalid_request(message, None),
        EngineError::IndexUnavailable(message) => {
            McpError::invalid_request(message, Some(kind_data(ErrorKind::IndexUnavailable)))
        }
        EngineError::Busy(message) => McpError::invalid_request(message, Some(serde_json::json!({ BUSY_DATA: true }))),
        EngineError::Cancelled => request_cancelled(),
        other => McpError::internal_error(other.to_string(), None),
    }
}

/// Build a successful tool result carrying the answer as `structured_content` and a `to_string()`
/// text block.
///
/// Every tool routes its response through here. In-process callers (the CLI, resources) read
/// `structured_content`; the MCP wire sends the text alone. No tool advertises an `output_schema`:
/// one tool answers in several shapes, and `tests/mcp_smoke.rs` holds every verb tool to that.
pub(crate) fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let value =
        serde_json::to_value(value).map_err(|e| McpError::internal_error(format!("serialize response: {e}"), None))?;
    Ok(CallToolResult::structured(value))
}

/// Numbers the failures [`task_failed`] logs, so a reported message finds its log line.
static FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The answer of a tool body that panicked or was stopped by a runtime shutdown. The payload can carry
/// host paths and library internals, so it goes to the log, under a reference the message names.
pub(crate) fn task_failed(join: &tokio::task::JoinError) -> McpError {
    let reference = format!(
        "{}-{}",
        std::process::id(),
        FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    tracing::error!(reference = %reference, error = %join, "a tool task failed");
    McpError::internal_error(
        format!(
            "basemind failed inside this call; the server log holds the cause as failure {reference} \
             (`basemind comms doctor` prints the daemon log's path)"
        ),
        None,
    )
}

/// `op`'s answer as a tool result.
pub(crate) fn answer<P, R: Serialize>(
    ws: &Workspace,
    p: P,
    op: impl FnOnce(&Workspace, P) -> Result<R, EngineError>,
) -> Result<CallToolResult, McpError> {
    json_result(&op(ws, p).map_err(mcp_error)?)
}

/// The telemetry row of one call, costed against the workspace's corpus.
fn record_row(
    ws: &Workspace,
    telemetry: &Telemetry,
    tool: &'static str,
    params: &Value,
    started: std::time::Instant,
    result: &Result<CallToolResult, McpError>,
) {
    let corpus = ws.corpus_bytes.load(std::sync::atomic::Ordering::Relaxed);
    record_call_to(telemetry, corpus, tool, params, started, result);
}

/// `result` with its text block written in `tool`'s encoding ([`crate::wire::encode`]); the
/// structured content stays JSON. After cursor binding, so the text carries the bound cursor, and
/// before the telemetry row, so the row costs the text sent.
pub(crate) fn as_text(tool: &str, mut result: CallToolResult) -> CallToolResult {
    if let Some(value) = &result.structured_content {
        result.content = vec![rmcp::model::ContentBlock::text(crate::wire::encode(tool, value))];
    }
    result
}

/// A verb-tool call, on the calling thread: refuse a zero `limit`, unbind the cursor, run `op`, bind
/// the next cursor, and record the telemetry row under the tool's name.
pub(crate) fn paged_call<P, R>(
    ws: &Workspace,
    telemetry: &Telemetry,
    p: P,
    op: impl FnOnce(&Workspace, P) -> Result<R, EngineError>,
) -> Result<CallToolResult, McpError>
where
    P: Paged,
    R: Serialize,
{
    let started = std::time::Instant::now();
    let params = serde_json::to_value(&p).unwrap_or(Value::Null);
    let result = bound_value(p, |p| op(ws, p))
        .map(|value| as_text(P::TOOL, CallToolResult::structured(value)))
        .map_err(mcp_error);
    record_row(ws, telemetry, P::TOOL, &params, started, &result);
    result
}

/// [`paged_call`] for a verb tool that pages nothing: no `limit` to refuse, no cursor to bind; the
/// same telemetry row.
pub(crate) fn unpaged_call<P, R>(
    ws: &Workspace,
    telemetry: &Telemetry,
    p: P,
    op: impl FnOnce(&Workspace, P) -> Result<R, EngineError>,
) -> Result<CallToolResult, McpError>
where
    P: ToolArgs + Serialize,
    R: Serialize,
{
    let started = std::time::Instant::now();
    let params = serde_json::to_value(&p).unwrap_or(Value::Null);
    let result = answer(ws, p, op).map(|result| as_text(P::TOOL, result));
    record_row(ws, telemetry, P::TOOL, &params, started, &result);
    result
}

/// `op`'s answer with no cursor binding and no telemetry row, its text compact JSON: a CLI-only
/// command is not a tool call.
pub(crate) fn plain_call<P, R: Serialize>(
    ws: &Workspace,
    p: P,
    op: impl FnOnce(&Workspace, P) -> Result<R, EngineError>,
) -> Result<CallToolResult, McpError> {
    answer(ws, p, op)
}

/// [`plain_call`] for a resource read: it answers as the tool it mirrors, its text in that tool's
/// encoding ([`crate::wire::encode`]), and is not a tool call.
pub(crate) fn mirror_call<P: ToolArgs, R: Serialize>(
    ws: &Workspace,
    p: P,
    op: Op<P, R>,
) -> Result<CallToolResult, McpError> {
    plain_call(ws, p, op).map(|result| as_text(P::TOOL, result))
}
