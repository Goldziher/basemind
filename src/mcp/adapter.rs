//! The MCP side of an engine operation: run it once on a blocking thread, report its error as MCP
//! reports errors, bind its cursor to the query and record its telemetry row.
//!
//! An engine operation ([`Op`]) is synchronous and names no rmcp type (ADR-0039). The `#[tool]` shim
//! and the matching `basemind` command both reach it through [`call`], so the two adapters answer
//! from the same body.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde::Serialize;

use super::ServerState;
use super::cursor_bind::{Paged, bound_blocking};
use super::helpers::json_result;
use super::strict::ToolArgs;
use super::telemetry::Telemetry;
use crate::engine::{EngineError, Workspace};

/// One engine operation: a verb tool's body over a [`Workspace`].
pub(crate) type Op<P, R> = fn(&Workspace, P) -> Result<R, EngineError>;

/// Each variant keeps its message; the code is what the CLI turns into an exit status.
impl From<EngineError> for McpError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::InvalidArgument(message) => McpError::invalid_params(message, None),
            EngineError::Unavailable(message) | EngineError::IndexUnavailable(message) => {
                McpError::invalid_request(message, None)
            }
            other => McpError::internal_error(other.to_string(), None),
        }
    }
}

/// Run `body` on a blocking thread: an operation reads the store, the disk and the in-RAM map
/// synchronously, which must not park a runtime worker. A panic in it resumes on the caller.
async fn blocking<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(body).await {
        Ok(value) => value,
        Err(join) => std::panic::resume_unwind(join.into_panic()),
    }
}

/// `op`'s answer as a tool result.
fn answer<P, R: Serialize>(ws: &Workspace, p: P, op: Op<P, R>) -> Result<CallToolResult, McpError> {
    json_result(&op(ws, p)?)
}

/// A verb-tool call: refuse a zero `limit`, unbind the cursor, run `op`, bind the next cursor, and
/// record the telemetry row under the tool's name. One `spawn_blocking` for the whole call.
pub(super) async fn call<P, R>(state: &ServerState, p: P, op: Op<P, R>) -> Result<CallToolResult, McpError>
where
    P: Paged + Send + 'static,
    R: Serialize + 'static,
{
    let ws = Arc::clone(&state.shared);
    let telemetry = Arc::clone(&state.telemetry);
    blocking(move || call_blocking(&ws, &telemetry, p, op)).await
}

/// [`call`] for a verb tool that pages nothing: no `limit` to refuse, no cursor to bind; the same
/// telemetry row.
pub(super) async fn call_unpaged<P, R>(state: &ServerState, p: P, op: Op<P, R>) -> Result<CallToolResult, McpError>
where
    P: ToolArgs + Serialize + Send + 'static,
    R: Serialize + 'static,
{
    let ws = Arc::clone(&state.shared);
    let telemetry = Arc::clone(&state.telemetry);
    blocking(move || {
        let started = std::time::Instant::now();
        let params = serde_json::to_value(&p).unwrap_or(serde_json::Value::Null);
        let result = answer(&ws, p, op);
        record(&ws, &telemetry, P::TOOL, &params, started, &result);
        result
    })
    .await
}

/// The telemetry row of one call, costed against the workspace's corpus.
fn record(
    ws: &Workspace,
    telemetry: &Telemetry,
    tool: &'static str,
    params: &serde_json::Value,
    started: std::time::Instant,
    result: &Result<CallToolResult, McpError>,
) {
    let corpus = ws.corpus_bytes.load(std::sync::atomic::Ordering::Relaxed);
    super::helpers_telemetry::record_call_to(telemetry, corpus, tool, params, started, result);
}

fn call_blocking<P: Paged, R: Serialize>(
    ws: &Workspace,
    telemetry: &Telemetry,
    p: P,
    op: Op<P, R>,
) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let params = serde_json::to_value(&p).unwrap_or(serde_json::Value::Null);
    let result = bound_blocking(p, |p| answer(ws, p, op));
    record(ws, telemetry, P::TOOL, &params, started, &result);
    result
}

/// `op`'s answer with no cursor binding and no telemetry row: a resource read answers as the tool it
/// mirrors, and is not a tool call; nor is a CLI-only command.
pub(super) async fn run<P, R>(ws: &Arc<Workspace>, p: P, op: Op<P, R>) -> Result<CallToolResult, McpError>
where
    P: Send + 'static,
    R: Serialize + 'static,
{
    let ws = Arc::clone(ws);
    blocking(move || answer(&ws, p, op)).await
}

#[cfg(test)]
mod tests {
    use rmcp::model::ErrorCode;

    use super::*;

    #[test]
    fn each_engine_error_keeps_its_message_and_maps_to_the_code_the_cli_reads() {
        let cases = [
            (EngineError::invalid_argument("bad path"), ErrorCode::INVALID_PARAMS),
            (EngineError::unavailable("no git"), ErrorCode::INVALID_REQUEST),
            (EngineError::index_unavailable("busy"), ErrorCode::INVALID_REQUEST),
            (EngineError::internal("broken"), ErrorCode::INTERNAL_ERROR),
        ];
        for (error, code) in cases {
            let message = error.to_string();
            let mapped = McpError::from(error);
            assert_eq!(mapped.code, code, "{message}");
            assert_eq!(mapped.message, message);
            assert!(mapped.data.is_none());
        }
        let io = McpError::from(EngineError::from(std::io::Error::other("disk gone")));
        assert_eq!(io.code, ErrorCode::INTERNAL_ERROR);
        assert_eq!(io.message, "disk gone");
    }
}
