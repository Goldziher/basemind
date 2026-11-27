//! The MCP side of an engine operation: run it once on a blocking thread that holds the call's
//! admission, and answer it as MCP answers (cursor binding, the telemetry row and the wire encoding
//! live in [`crate::dispatch`], which the `basemind` commands run too).
//!
//! An engine operation ([`Op`]) is synchronous and names no rmcp type (ADR-0039). The `#[tool]` shim
//! and the matching `basemind` command both reach it through [`crate::dispatch`], so the two
//! adapters answer from the same body.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde::Serialize;

use super::ServerState;
use crate::dispatch;
use crate::engine::paging::{Paged, ToolArgs};
use crate::engine::{EngineError, Workspace};

pub(crate) use crate::dispatch::Op;
pub(crate) use crate::engine::BUSY_DATA;

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
    .unwrap_or_else(|join| Err(dispatch::task_failed(&join)))
}

/// A verb-tool call ([`dispatch::paged_call`]) on a blocking thread, with the server's telemetry
/// writer. One `spawn_blocking` for the whole call.
pub(super) async fn call<P, R, O>(state: &ServerState, p: P, op: O) -> Result<CallToolResult, McpError>
where
    P: Paged + Send + 'static,
    R: Serialize + 'static,
    O: FnOnce(&Workspace, P) -> Result<R, EngineError> + Send + 'static,
{
    let ws = Arc::clone(&state.shared);
    let telemetry = Arc::clone(&state.telemetry);
    blocking_answer(move || dispatch::paged_call(&ws, &telemetry, p, op)).await
}

/// [`call`] for a verb tool that pages nothing ([`dispatch::unpaged_call`]).
pub(super) async fn call_unpaged<P, R, O>(state: &ServerState, p: P, op: O) -> Result<CallToolResult, McpError>
where
    P: ToolArgs + Serialize + Send + 'static,
    R: Serialize + 'static,
    O: FnOnce(&Workspace, P) -> Result<R, EngineError> + Send + 'static,
{
    let ws = Arc::clone(&state.shared);
    let telemetry = Arc::clone(&state.telemetry);
    blocking_answer(move || dispatch::unpaged_call(&ws, &telemetry, p, op)).await
}

/// `op`'s answer with no cursor binding and no telemetry row, its text compact JSON: a CLI-only
/// command is not a tool call.
pub(super) async fn run<P, R>(ws: &Arc<Workspace>, p: P, op: Op<P, R>) -> Result<CallToolResult, McpError>
where
    P: Send + 'static,
    R: Serialize + 'static,
{
    let ws = Arc::clone(ws);
    blocking_answer(move || dispatch::plain_call(&ws, p, op)).await
}

/// [`run`] for a resource read: it answers as the tool it mirrors, its text in that tool's encoding
/// ([`crate::wire::encode`]), and is not a tool call.
pub(super) async fn mirror<P, R>(ws: &Arc<Workspace>, p: P, op: Op<P, R>) -> Result<CallToolResult, McpError>
where
    P: ToolArgs + Send + 'static,
    R: Serialize + 'static,
{
    let ws = Arc::clone(ws);
    blocking_answer(move || dispatch::mirror_call(&ws, p, op)).await
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
        let reference = failed
            .message
            .strip_prefix("basemind failed inside this call; the server log holds the cause as failure ")
            .and_then(|rest| rest.strip_suffix(" (`basemind comms doctor` prints the daemon log's path)"))
            .unwrap_or_else(|| panic!("a fixed message: {}", failed.message));
        let (pid, number) = reference.split_once('-').expect("pid-number");
        assert_eq!(pid, std::process::id().to_string());
        assert!(number.parse::<u64>().is_ok(), "{reference}");
        assert!(
            !failed.message.contains("exploded"),
            "the payload stays in the log: {}",
            failed.message
        );
        let answered = blocking_answer(|| Ok(CallToolResult::success(Vec::new())))
            .await
            .expect("a body that returns is answered");
        assert_eq!(answered.is_error, Some(false));
    }

    /// An aborted handler future leaves its body running on the blocking pool. Unless the call is
    /// abandoned ([`super::super::admission::Admission::abandon`]), its heavy permit stays taken
    /// until the body ends, or each retry of a dropped call would start one more concurrent run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_call_keeps_its_heavy_permit_until_its_blocking_body_ends() {
        use super::super::admission::{HeavyAdmission, WorkClass, holding};

        let admission = HeavyAdmission::new(1, 1, std::time::Duration::from_millis(50));
        let permit = admission.admit(WorkClass::Heavy).await.expect("the one permit");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let call = tokio::spawn(holding(
            Arc::new(permit),
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

    /// A `docs_search` that hangs past its deadline must not hold the heavy lane: the next heavy
    /// call is admitted while the expired body still runs, and the body was told to stop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_heavy_read_past_its_deadline_lets_the_next_heavy_call_in() {
        use super::super::admission::{Admission, HeavyAdmission, WorkClass};
        use super::super::tasks::run_bounded_read;

        let deadline = std::time::Duration::from_millis(100);
        let admission = HeavyAdmission::new(1, 1, std::time::Duration::from_millis(50)).with_deadline(deadline);
        let permit = admission.admit(WorkClass::Heavy).await.expect("the one permit");
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let ct = tokio_util::sync::CancellationToken::new();

        let expired = run_bounded_read(
            permit,
            ct.clone(),
            admission.deadline(),
            blocking_answer(move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
                Ok(CallToolResult::success(Vec::new()))
            }),
        )
        .await
        .expect_err("the call outlived its deadline");

        assert!(
            expired.message.starts_with("deadline_exceeded: "),
            "{}",
            expired.message
        );
        assert!(super::super::tasks::is_deadline_exceeded(&expired));
        assert_eq!(expired.code, ErrorCode(-32004));
        assert_eq!(
            expired.data,
            Some(serde_json::json!({ "deadline_secs": 0, "retryable": true }))
        );
        assert!(ct.is_cancelled(), "the expired work was told to stop");
        started_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the body is still running");
        let next = admission
            .admit(WorkClass::Heavy)
            .await
            .expect("the next heavy call is admitted while the expired body still runs");
        assert!(matches!(next, Admission::Heavy(_)));
        release_tx.send(()).expect("release the body");
    }

    #[test]
    fn each_engine_error_keeps_its_message_and_maps_to_the_code_the_cli_reads() {
        let cases = [
            (EngineError::invalid_argument("bad path"), ErrorCode::INVALID_PARAMS),
            (EngineError::unavailable("no git"), ErrorCode::INVALID_REQUEST),
            (EngineError::internal("broken"), ErrorCode::INTERNAL_ERROR),
        ];
        for (error, code) in cases {
            let message = error.to_string();
            let mapped = McpError::from(error);
            assert_eq!(mapped.code, code, "{message}");
            assert_eq!(mapped.message, message);
            assert!(mapped.data.is_none());
        }
        let unreadable = McpError::from(EngineError::index_unavailable("the index is empty"));
        assert_eq!(unreadable.code, ErrorCode::INVALID_REQUEST);
        assert_eq!(unreadable.message, "the index is empty");
        assert_eq!(
            unreadable.data,
            Some(serde_json::json!({ "error": "index_unavailable" }))
        );
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
