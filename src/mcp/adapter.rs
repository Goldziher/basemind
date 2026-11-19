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
use super::cursor_bind::{Paged, bound};
use super::helpers::json_result;
use super::strict::ToolArgs;
use super::telemetry::Telemetry;
use crate::engine::{EngineError, Workspace};

/// One engine operation: a verb tool's body over a [`Workspace`].
pub(crate) type Op<P, R> = fn(&Workspace, P) -> Result<R, EngineError>;

/// Key of the `data` field that marks a tool error as contention for the workspace writer lock.
pub(crate) const BUSY_DATA: &str = "busy";

/// Each variant keeps its message; the code is what the CLI turns into an exit status.
impl From<EngineError> for McpError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::InvalidArgument(message) => McpError::invalid_params(message, None),
            EngineError::Unavailable(message) | EngineError::IndexUnavailable(message) => {
                McpError::invalid_request(message, None)
            }
            EngineError::Busy(message) => {
                McpError::invalid_request(message, Some(serde_json::json!({ BUSY_DATA: true })))
            }
            EngineError::Cancelled => super::tasks::request_cancelled(),
            other => McpError::internal_error(other.to_string(), None),
        }
    }
}

/// Run `body` on a blocking thread: an operation reads the store, the disk and the in-RAM map
/// synchronously, which must not park a runtime worker. A panic in it resumes on the caller.
///
/// The body holds the admission of the call it runs for ([`super::admission::held`]): dropping the
/// awaiting future does not stop a blocking thread, so the permit must outlive the work, not the
/// handler.
pub(super) async fn blocking<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    let held = super::admission::held();
    match tokio::task::spawn_blocking(move || {
        let _held = held;
        body()
    })
    .await
    {
        Ok(value) => value,
        Err(join) => std::panic::resume_unwind(join.into_panic()),
    }
}

/// [`blocking`] for a tool body: a panic in it, or a runtime shutting down under it, comes back as an
/// internal error the shim reports as an `isError` result, rather than unwinding into the request
/// handler and leaving the client waiting on a response that never comes.
async fn blocking_answer(
    body: impl FnOnce() -> Result<CallToolResult, McpError> + Send + 'static,
) -> Result<CallToolResult, McpError> {
    let held = super::admission::held();
    tokio::task::spawn_blocking(move || {
        let _held = held;
        body()
    })
    .await
    .unwrap_or_else(|join| Err(McpError::internal_error(format!("tool task failed: {join}"), None)))
}

/// `op`'s answer as a tool result.
fn answer<P, R: Serialize>(
    ws: &Workspace,
    p: P,
    op: impl FnOnce(&Workspace, P) -> Result<R, EngineError>,
) -> Result<CallToolResult, McpError> {
    json_result(&op(ws, p)?)
}

/// A verb-tool call: refuse a zero `limit`, unbind the cursor, run `op`, bind the next cursor, and
/// record the telemetry row under the tool's name. One `spawn_blocking` for the whole call.
pub(super) async fn call<P, R, O>(state: &ServerState, p: P, op: O) -> Result<CallToolResult, McpError>
where
    P: Paged + Send + 'static,
    R: Serialize + 'static,
    O: FnOnce(&Workspace, P) -> Result<R, EngineError> + Send + 'static,
{
    let ws = Arc::clone(&state.shared);
    let telemetry = Arc::clone(&state.telemetry);
    blocking_answer(move || call_blocking(&ws, &telemetry, p, op)).await
}

/// [`call`] for a verb tool that pages nothing: no `limit` to refuse, no cursor to bind; the same
/// telemetry row.
pub(super) async fn call_unpaged<P, R, O>(state: &ServerState, p: P, op: O) -> Result<CallToolResult, McpError>
where
    P: ToolArgs + Serialize + Send + 'static,
    R: Serialize + 'static,
    O: FnOnce(&Workspace, P) -> Result<R, EngineError> + Send + 'static,
{
    let ws = Arc::clone(&state.shared);
    let telemetry = Arc::clone(&state.telemetry);
    blocking_answer(move || {
        let started = std::time::Instant::now();
        let params = serde_json::to_value(&p).unwrap_or(serde_json::Value::Null);
        let result = answer(&ws, p, op).map(|result| as_text(P::TOOL, result));
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
    op: impl FnOnce(&Workspace, P) -> Result<R, EngineError>,
) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let params = serde_json::to_value(&p).unwrap_or(serde_json::Value::Null);
    let result = bound(p, |p| answer(ws, p, op)).map(|result| as_text(P::TOOL, result));
    record(ws, telemetry, P::TOOL, &params, started, &result);
    result
}

/// `result` with its text block written in `tool`'s encoding ([`crate::wire::encode`]); the
/// structured content stays JSON. After cursor binding, so the text carries the bound cursor, and
/// before the telemetry row, so the row costs the text sent.
fn as_text(tool: &str, mut result: CallToolResult) -> CallToolResult {
    if let Some(value) = &result.structured_content {
        result.content = vec![rmcp::model::ContentBlock::text(crate::wire::encode(tool, value))];
    }
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
    blocking_answer(move || answer(&ws, p, op)).await
}

#[cfg(test)]
mod tests {
    use rmcp::model::ErrorCode;

    use super::*;

    /// A panic in a tool body (the `index_rescan` scan, say) is an internal error the shim reports as
    /// an `isError` result, not an unwind into the request handler.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_tool_body_is_an_internal_error() {
        let failed = blocking_answer(|| panic!("resolve pass exploded"))
            .await
            .expect_err("a panic is an error");
        assert_eq!(failed.code, ErrorCode::INTERNAL_ERROR);
        assert!(failed.message.starts_with("tool task failed: "), "{}", failed.message);
        assert!(failed.message.contains("resolve pass exploded"), "{}", failed.message);
        let answered = blocking_answer(|| Ok(CallToolResult::success(Vec::new())))
            .await
            .expect("a body that returns is answered");
        assert_eq!(answered.is_error, Some(false));
    }

    /// A cancelled `code_search` drops its handler future, but the body keeps running on the
    /// blocking pool. Its heavy permit must stay taken until the body ends, or each retry of a
    /// timed-out call would start one more concurrent model run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_call_keeps_its_heavy_permit_until_its_blocking_body_ends() {
        use super::super::admission::{HeavyAdmission, WorkClass, holding};

        let admission = HeavyAdmission::new(1, std::time::Duration::from_millis(50));
        let permit = admission.admit(WorkClass::Heavy).await.expect("the one permit");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let call = tokio::spawn(holding(
            permit,
            blocking_answer(move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
                Ok(CallToolResult::success(Vec::new()))
            }),
        ));
        started_rx.await.expect("the body started");
        call.abort();
        assert!(call.await.is_err(), "the handler was dropped");

        assert!(
            admission.admit(WorkClass::Heavy).await.is_err(),
            "the body still runs, so its permit is still taken"
        );
        release_tx.send(()).expect("release the body");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while admission.admit(WorkClass::Heavy).await.is_err() {
            assert!(std::time::Instant::now() < deadline, "the permit was never released");
        }
    }

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
        let busy = McpError::from(EngineError::busy("held"));
        assert_eq!(busy.code, ErrorCode::INVALID_REQUEST);
        assert_eq!(busy.message, "held");
        assert_eq!(
            busy.data.as_ref().and_then(|data| data.get(BUSY_DATA)),
            Some(&serde_json::Value::Bool(true)),
            "busy, so the CLI exits 3"
        );
        let cancelled = McpError::from(EngineError::Cancelled);
        assert_eq!(cancelled.code, ErrorCode(-32800));
        assert_eq!(cancelled.message, "request_cancelled");
        let io = McpError::from(EngineError::from(std::io::Error::other("disk gone")));
        assert_eq!(io.code, ErrorCode::INTERNAL_ERROR);
        assert_eq!(io.message, "disk gone");
    }
}
