//! SEP-2663 Tasks extension (`io.modelcontextprotocol/tasks`) wiring for [`BasemindServer`].
//!
//! Results are returned inline. A handful of tools can run for seconds (a full-corpus rescan, a
//! cold embedding search), and blocking the transport that long starves every other request on the
//! connection, so for a client that declared the tasks extension those calls are started on their
//! own task and given [`TASK_THRESHOLD`] to finish. One that does is answered as an ordinary
//! `tools/call` result; only a call still running past the threshold is handed to the
//! [`TaskManager`](rmcp::task_manager::TaskManager) and answered with a pollable handle
//! (`tasks/get`). Wrapping by tool name alone used to return a 6 ms search as a background task.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult, CreateTaskResult};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::task_manager::{TaskContext, TaskExit, TaskOptions};

use super::BasemindServer;

/// How long an offload-eligible call may run before it is answered with a task handle instead of
/// its result.
pub(super) const TASK_THRESHOLD: Duration = Duration::from_secs(3);

/// The task a slow tool was handed to, filled in once (and only if) the call outlives
/// [`TASK_THRESHOLD`].
pub(super) type TaskSlot = Arc<OnceLock<TaskContext>>;

tokio::task_local! {
    /// The slot of the call a slow tool is running inside, set around the router call by
    /// [`run_slow_tool`]. Lets [`super::notifications::emit_progress`] mirror each progress message
    /// onto the task's `statusMessage` once the call has become a task, so a client polling
    /// `tasks/get` sees the same counter a progress-token subscriber does.
    pub(super) static CURRENT_TASK: TaskSlot;
}

/// Pure read-only calls that are safe to abandon mid-flight: they only read the in-RAM map / index
/// (or the vector store) and write nothing durable, so dropping the handler future on client
/// cancellation loses only recomputable work. NEVER add a call that writes (`index_rescan`,
/// `memory_save`, a registry mutation): dropping those at an await point could strand a
/// half-applied write. Keyed like [`SLOW_CALLS`].
pub(super) const CANCEL_AWARE_READS: &[&str] = &["code_grep", "code_search", "docs_search"];

/// Whether this call is one of the [`CANCEL_AWARE_READS`].
pub(super) fn is_cancel_aware_read(name: &str, arguments: Option<&serde_json::Map<String, serde_json::Value>>) -> bool {
    matches_call(CANCEL_AWARE_READS, name, arguments)
}

/// JSON-RPC code for a request the client cancelled (the LSP `RequestCancelled` code).
pub(super) const REQUEST_CANCELLED_CODE: i32 = -32800;

pub(super) fn request_cancelled() -> McpError {
    McpError::new(
        rmcp::model::ErrorCode(REQUEST_CANCELLED_CODE),
        "request_cancelled",
        None,
    )
}

/// JSON-RPC code for a cancel-aware read stopped at its deadline
/// ([`super::admission::HeavyAdmission::deadline`]).
const DEADLINE_EXCEEDED_CODE: i32 = -32004;

/// The error a cancel-aware read stopped at `deadline` answers with.
pub(super) fn deadline_exceeded(deadline: Duration) -> McpError {
    McpError::new(
        rmcp::model::ErrorCode(DEADLINE_EXCEEDED_CODE),
        format!(
            "deadline_exceeded: the call ran past its {} s deadline and was stopped. Nothing was changed. \
             Narrow the query, or retry it.",
            deadline.as_secs()
        ),
        Some(serde_json::json!({ "deadline_secs": deadline.as_secs(), "retryable": true })),
    )
}

/// How a task stopped at its deadline ends: an agent reads the stop as a tool result, like the direct
/// path and `server_busy`; a CLI keeps the JSON-RPC error.
fn deadline_stop(cli: bool, deadline: Duration) -> Result<CallToolResult, TaskExit> {
    let stopped = deadline_exceeded(deadline);
    if cli {
        return Err(TaskExit::Error(stopped));
    }
    Ok(super::tool_contract::tool_error(stopped.message))
}

/// Whether `error` is a [`deadline_exceeded`] stop.
pub(super) fn is_deadline_exceeded(error: &McpError) -> bool {
    error.code == rmcp::model::ErrorCode(DEADLINE_EXCEEDED_CODE)
}

/// Run a cancel-aware read under `admission` until it answers, `ct` is cancelled, or `deadline`
/// passes. On either of the last two the work future is dropped (releasing every guard it holds),
/// `ct` is cancelled so a body that checks it stops, and the admission is abandoned
/// ([`super::admission::Admission::abandon`]): blocking work still running no longer holds the
/// heavy lane, so a hung read cannot keep the next heavy call out.
pub(super) async fn run_bounded_read<T>(
    admission: super::admission::Admission,
    ct: tokio_util::sync::CancellationToken,
    deadline: Duration,
    work: impl std::future::Future<Output = Result<T, McpError>>,
) -> Result<T, McpError> {
    let admission = Arc::new(admission);
    let stopped = tokio::select! {
        biased;
        () = ct.cancelled() => request_cancelled(),
        () = tokio::time::sleep(deadline) => deadline_exceeded(deadline),
        result = super::admission::holding(Arc::clone(&admission), work) => return result,
    };
    ct.cancel();
    admission.abandon();
    Err(stopped)
}

/// Calls that CAN dominate the transport for seconds, and so are eligible to become a task when
/// they outlive [`TASK_THRESHOLD`]. Eligibility only decides whether the call runs on its own task
/// with a deadline; a fast one is still answered inline.
///
/// Entries are keyed the way telemetry is: a bare tool name matches every call of that tool, and a
/// `domain:mode` key matches one mode of the mode-dispatched domain tool (`agents`).
pub(super) const SLOW_CALLS: &[&str] = &["index_rescan", "code_search", "docs_search"];

/// Whether this call is one of the [`SLOW_CALLS`] eligible for task offload.
pub(super) fn is_slow_tool(name: &str, arguments: Option<&serde_json::Map<String, serde_json::Value>>) -> bool {
    matches_call(SLOW_CALLS, name, arguments)
}

/// Whether the call `name` (with its `mode` argument, for a domain tool) is listed in `table`.
fn matches_call(table: &[&str], name: &str, arguments: Option<&serde_json::Map<String, serde_json::Value>>) -> bool {
    if table.contains(&name) {
        return true;
    }
    let Some(mode) = arguments
        .and_then(|args| args.get("mode"))
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    table.iter().any(|entry| {
        entry
            .split_once(':')
            .is_some_and(|(tool, entry_mode)| tool == name && entry_mode == mode)
    })
}

/// Dispose of the tool's child task after a cancellation or its deadline. The tool's token is always
/// cancelled so a cancel-aware handler unwinds on its own; a pure read ([`CANCEL_AWARE_READS`]) is
/// also aborted so its guards (comms link) drop immediately instead of at its next poll, and its
/// admission is abandoned so blocking work it left running no longer holds the heavy lane. Anything
/// else (rescan) is left detached to finish its write, holding its permit until it does.
fn settle_cancelled<T>(
    work: tokio::task::JoinHandle<T>,
    tool_ct: &tokio_util::sync::CancellationToken,
    abort: bool,
    admission: &super::admission::Admission,
) {
    tool_ct.cancel();
    if abort {
        work.abort();
        admission.abandon();
    }
}

/// What the tool's own task produced, as a `tools/call` outcome.
fn joined_response(
    joined: Result<Result<CallToolResponse, McpError>, tokio::task::JoinError>,
) -> Result<CallToolResponse, McpError> {
    match joined {
        Ok(response) => response,
        Err(join_error) => Err(McpError::internal_error(
            format!("tool task failed to complete: {join_error}"),
            None,
        )),
    }
}

/// Run an offload-eligible call: inline if it finishes within `threshold`, otherwise as a SEP-2663
/// task the client polls.
///
/// The tool runs on its OWN [`tokio::spawn`]ed task from the start and through the SAME static
/// router the synchronous path uses, so the result is byte-for-byte what a blocking `tools/call`
/// produces whichever way it is delivered. Handing a still-running call to the task manager only
/// moves its [`JoinHandle`](tokio::task::JoinHandle); the work is never restarted.
///
/// Cancellation (of the request while it is still inline, or `tasks/cancel` afterwards) abandons
/// only the RESULT, never a write in flight: dropping the handle DETACHES the tokio task, so a
/// mutating tool like `index_rescan` always runs both its on-disk write and its in-RAM cache
/// refresh to completion. A pure read is aborted instead, releasing its guards at once, and is also
/// stopped once it has run for the admission deadline, however it is being delivered.
pub(super) async fn run_slow_tool(
    server: &BasemindServer,
    request: CallToolRequestParams,
    mut context: RequestContext<RoleServer>,
    admission: super::admission::Admission,
    threshold: Duration,
) -> Result<CallToolResponse, McpError> {
    let server = server.clone();
    // The tool observes `context.ct`, but `tasks/cancel` only fires the task's own signal. Give the
    // tool a child of the request token that both cancellation paths cancel.
    let request_ct = context.ct.clone();
    let tool_ct = request_ct.child_token();
    context.ct = tool_ct.clone();
    let cancel_aware = is_cancel_aware_read(&request.name, request.arguments.as_ref());
    let cli = server.cli;
    let deadline = server.heavy_admission.deadline();
    let expires = tokio::time::Instant::now() + deadline;
    let admission = Arc::new(admission);
    let work_admission = Arc::clone(&admission);
    // `TaskManager` is a cheap Arc clone sharing the same store; taken out before `server` moves.
    let manager = server.tasks.clone();
    let slot = TaskSlot::default();
    let work_slot = Arc::clone(&slot);
    let mut work = tokio::spawn(async move {
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(&server, request, context);
        super::admission::holding(
            work_admission,
            CURRENT_TASK.scope(work_slot, server.tool_router.call(tcc)),
        )
        .await
    });

    tokio::select! {
        biased;
        () = request_ct.cancelled() => {
            settle_cancelled(work, &tool_ct, cancel_aware, &admission);
            return Err(request_cancelled());
        }
        joined = &mut work => return joined_response(joined),
        () = tokio::time::sleep(threshold) => {}
    }

    let task = manager.spawn(TaskOptions::new(), move |ctx| {
        Box::pin(async move {
            let _ = slot.set(ctx.clone());
            let joined = tokio::select! {
                biased;
                () = ctx.cancelled() => {
                    settle_cancelled(work, &tool_ct, cancel_aware, &admission);
                    return Err(TaskExit::Cancelled);
                }
                () = tokio::time::sleep_until(expires), if cancel_aware => {
                    settle_cancelled(work, &tool_ct, true, &admission);
                    return deadline_stop(cli, deadline);
                }
                joined = &mut work => joined,
            };
            match joined_response(joined) {
                Ok(CallToolResponse::Complete(result)) => Ok(result),
                Ok(_) => Err(TaskExit::Error(McpError::internal_error(
                    "tool returned a non-terminal response inside a task",
                    None,
                ))),
                Err(error) => Err(TaskExit::Error(error)),
            }
        })
    });
    Ok(CallToolResponse::Task(CreateTaskResult::new(task)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        json.as_object().expect("object").clone()
    }

    #[test]
    fn should_offer_offload_only_to_the_tools_that_can_run_long() {
        for tool in ["index_rescan", "code_search", "docs_search"] {
            assert!(is_slow_tool(tool, None), "{tool}");
        }
        for tool in ["index_status", "code_outline", "code_grep", "git_log", "memory_search"] {
            assert!(!is_slow_tool(tool, None), "{tool}");
        }
    }

    /// A `mode` only selects within a mode-dispatched domain tool; it must not turn a verb tool
    /// that is not listed into a slow one.
    #[test]
    fn should_not_match_a_mode_argument_on_a_verb_tool() {
        assert!(!is_slow_tool(
            "code_outline",
            Some(&args(serde_json::json!({ "mode": "index_rescan" })))
        ));
    }

    #[test]
    fn should_race_cancellation_only_for_the_pure_reads() {
        for tool in ["code_grep", "code_search", "docs_search"] {
            assert!(is_cancel_aware_read(tool, None), "{tool}");
        }
        // A waiting agent_inbox marks what it returns read, so dropping it is not free.
        for tool in [
            "index_rescan",
            "memory_save",
            "code_outline",
            "agent_inbox",
            "agent_post",
        ] {
            assert!(!is_cancel_aware_read(tool, None), "{tool}");
        }
    }

    /// Holds a heavy admission permit; dropping it (as a cancelled handler future does) frees it.
    #[tokio::test]
    async fn cancelling_a_read_returns_promptly_and_releases_the_permit() {
        use super::super::admission::{HeavyAdmission, WorkClass};
        let admission = HeavyAdmission::new(1, std::time::Duration::from_millis(50));
        let permit = admission.admit(WorkClass::Heavy).await.expect("permit");
        let ct = tokio_util::sync::CancellationToken::new();
        let call = tokio::spawn(run_bounded_read(
            permit,
            ct.clone(),
            std::time::Duration::from_secs(60),
            std::future::pending::<Result<(), McpError>>(),
        ));
        assert!(
            admission.admit(WorkClass::Heavy).await.is_err(),
            "permit is held while the read runs"
        );
        ct.cancel();
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(std::time::Duration::from_secs(2), call)
            .await
            .expect("cancelled call returns promptly")
            .expect("join")
            .expect_err("cancelled call is an error");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(error.code, rmcp::model::ErrorCode(REQUEST_CANCELLED_CODE));
        admission
            .admit(WorkClass::Heavy)
            .await
            .expect("permit released after cancel");
    }

    /// Drops its flag on drop, standing in for the comms link a `wait` holds.
    struct LinkGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for LinkGuard {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Task cancellation must reach a long-running cancel-aware read: its token fires and (for the
    /// pure reads) the work is aborted, so the guard it holds (the subscriber link) is released at
    /// once, and its admission is abandoned, so the heavy lane admits the next call.
    #[tokio::test]
    async fn cancelling_a_task_releases_a_cancel_aware_reads_link() {
        use super::super::admission::{HeavyAdmission, WorkClass};
        let admission = HeavyAdmission::new(1, std::time::Duration::from_millis(50));
        let permit = admission.admit(WorkClass::Heavy).await.expect("permit");
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tool_ct = tokio_util::sync::CancellationToken::new();
        let guard = LinkGuard(released.clone());
        let work = tokio::spawn(async move {
            let _link = guard;
            std::future::pending::<()>().await;
        });
        settle_cancelled(work, &tool_ct, true, &permit);
        assert!(tool_ct.is_cancelled());
        admission
            .admit(WorkClass::Heavy)
            .await
            .expect("the abandoned read no longer holds the lane");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !released.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("link released promptly after task cancel");
    }

    /// A write tool (rescan) is detached, not aborted: it keeps its guard and its heavy permit and
    /// runs to completion.
    #[tokio::test]
    async fn cancelling_a_task_detaches_a_mutating_tool() {
        use super::super::admission::{HeavyAdmission, WorkClass};
        let admission = HeavyAdmission::new(1, std::time::Duration::from_millis(50));
        let permit = admission.admit(WorkClass::Heavy).await.expect("permit");
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tool_ct = tokio_util::sync::CancellationToken::new();
        let guard = LinkGuard(released.clone());
        let work = tokio::spawn(async move {
            let _link = guard;
            std::future::pending::<()>().await;
        });
        settle_cancelled(work, &tool_ct, false, &permit);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!released.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            admission.admit(WorkClass::Heavy).await.is_err(),
            "the write still holds its lane permit"
        );
    }

    #[test]
    fn a_task_stopped_at_its_deadline_is_a_tool_error_outside_the_cli() {
        let deadline = std::time::Duration::from_secs(150);
        let agent = deadline_stop(false, deadline).expect("a tool result");
        assert_eq!(agent.is_error, Some(true));
        let cli = deadline_stop(true, deadline);
        assert!(
            matches!(&cli, Err(TaskExit::Error(error)) if is_deadline_exceeded(error)),
            "the CLI keeps the deadline_exceeded error"
        );
    }

    #[tokio::test]
    async fn an_uncancelled_read_returns_its_result() {
        let value = run_bounded_read(
            super::super::admission::Admission::Control,
            tokio_util::sync::CancellationToken::new(),
            std::time::Duration::from_secs(60),
            async { Ok::<_, McpError>(7) },
        )
        .await
        .expect("result");
        assert_eq!(value, 7);
    }
}
