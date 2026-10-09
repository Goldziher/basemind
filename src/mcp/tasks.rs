//! SEP-2663 Tasks extension (`io.modelcontextprotocol/tasks`) wiring for [`BasemindServer`].
//!
//! A handful of basemind tools routinely run for seconds — a full-corpus rescan, document / web
//! ingestion. Blocking the MCP transport for that long starves every other request on the same
//! connection. When the client has declared the tasks extension, `call_tool` hands those tools off
//! here: the work is spawned onto the server's [`TaskManager`], and the caller gets a pollable task
//! handle (`tasks/get`) instead of a stalled `tools/call`. Clients that did not declare the
//! extension keep the synchronous path unchanged.

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolRequestParams, CallToolResponse, CreateTaskResult};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::task_manager::{TaskExit, TaskOptions};

use super::BasemindServer;

tokio::task_local! {
    /// The task a slow tool is running inside, set around the router call by [`spawn_slow_tool`].
    /// Lets [`super::notifications::emit_progress`] mirror each progress message onto the task's
    /// `statusMessage`, so a client polling `tasks/get` sees the same counter a progress-token
    /// subscriber does.
    pub(super) static CURRENT_TASK: rmcp::task_manager::TaskContext;
}

/// Pure read-only modes that are safe to abandon mid-flight: they only read the in-RAM map / index
/// (or the vector store) and write nothing durable, so dropping the handler future on client
/// cancellation loses only recomputable work. NEVER add a mode that writes (rescan, put, accept,
/// cache or registry mutation): dropping those at an await point could strand a half-applied write.
/// Keyed like [`SLOW_CALLS`] on `domain:mode`.
pub(super) const CANCEL_AWARE_READS: &[&str] = &[
    "code:grep",
    "code:semantic",
    "graph:map",
    "graph:calls",
    "memory:documents",
];

/// Whether this call is one of the [`CANCEL_AWARE_READS`].
pub(super) fn is_cancel_aware_read(name: &str, arguments: Option<&serde_json::Map<String, serde_json::Value>>) -> bool {
    let Some(mode) = arguments
        .and_then(|args| args.get("mode"))
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    CANCEL_AWARE_READS.iter().any(|read| {
        read.split_once(':')
            .is_some_and(|(tool, read_mode)| tool == name && read_mode == mode)
    })
}

/// JSON-RPC code for a request the client cancelled (the LSP `RequestCancelled` code).
const REQUEST_CANCELLED_CODE: i32 = -32800;

/// Race `work` against the request's cancellation signal. On cancellation the work future is dropped
/// (releasing every guard it holds) and a `request_cancelled` error is returned immediately.
pub(super) async fn run_until_cancelled<T>(
    cancelled: impl std::future::Future<Output = ()>,
    work: impl std::future::Future<Output = Result<T, McpError>>,
) -> Result<T, McpError> {
    tokio::select! {
        biased;
        () = cancelled => Err(McpError::new(
            rmcp::model::ErrorCode(REQUEST_CANCELLED_CODE),
            "request_cancelled",
            None,
        )),
        result = work => result,
    }
}

/// Calls whose work can dominate the transport for long enough that a task-capable client is better
/// served an async handle it can poll than a blocked `tools/call`. Kept deliberately small and
/// centralized: only operations that routinely run for seconds belong here. Feature-gated entries
/// drop out of the slice when their tool is not compiled in, so the set never names a call the
/// router does not advertise.
///
/// Entries are keyed the way telemetry is: a bare tool name matches every call of that tool, and a
/// `domain:mode` key matches one mode of a consolidated domain tool. The distinction is load-bearing
/// — consolidation put fast and slow operations behind one tool name, so keying on the name alone
/// would make offload all-or-nothing per domain (every `web` call offloaded, or none).
pub(super) const SLOW_CALLS: &[&str] = &[
    "admin:rescan",
    "graph:map",
    "code:semantic",
    #[cfg(feature = "documents")]
    "memory:documents",
    #[cfg(feature = "crawl")]
    "web:scrape",
    #[cfg(feature = "crawl")]
    "web:crawl",
    #[cfg(feature = "crawl")]
    "web:map",
];

/// Whether this call is one of the [`SLOW_CALLS`] eligible for task offload.
///
/// `arguments` is the raw request payload; a consolidated domain tool carries its operation in a
/// `mode` string, so the lookup tries `name:mode` before falling back to the bare tool name.
pub(super) fn is_slow_tool(name: &str, arguments: Option<&serde_json::Map<String, serde_json::Value>>) -> bool {
    if SLOW_CALLS.contains(&name) {
        return true;
    }
    let Some(mode) = arguments
        .and_then(|args| args.get("mode"))
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    SLOW_CALLS.iter().any(|slow| {
        slow.split_once(':')
            .is_some_and(|(tool, slow_mode)| tool == name && slow_mode == mode)
    })
}

/// Spawn a slow tool's invocation as a SEP-2663 task and return the seed [`CreateTaskResult`].
///
/// The spawned future runs the SAME work the synchronous path would: it rebuilds a
/// [`ToolCallContext`](rmcp::handler::server::tool::ToolCallContext) for the real tool and delegates
/// to the identical static router, so results are byte-for-byte what a blocking `tools/call` would
/// have produced. The router's terminal [`CallToolResponse::Complete`] is unwrapped into the task's
/// `Completed` payload (`result_to_object` in the task manager serializes it); a router error settles
/// the task as `failed`.
///
/// Cancellation abandons only the RESULT we report, never the in-flight work. The tool runs on its
/// OWN [`tokio::spawn`]ed task; a `tasks/cancel` settles the task as `cancelled` and drops that task's
/// [`JoinHandle`](tokio::task::JoinHandle) — which DETACHES (never aborts) the tokio task. So a
/// mutating tool like `rescan` always runs both its on-disk write AND its in-RAM cache refresh to
/// completion, and the served state stays coherent even when the client cancels. (Dropping the future
/// directly would instead cancel it at its next await point, stranding the write's cache-refresh
/// continuation and desyncing the in-RAM map from disk.) The tool bodies themselves are not
/// cancel-aware — this is a first-cut whole-call offload — so a cancelled long tool still consumes its
/// CPU/IO to completion; only the reported outcome is discarded.
pub(super) fn spawn_slow_tool(
    server: &BasemindServer,
    request: CallToolRequestParams,
    context: RequestContext<RoleServer>,
    admission: super::admission::Admission,
) -> CreateTaskResult {
    let server = server.clone();
    // Clone the manager handle out so the spawned closure can move `server` wholesale (it needs the
    // router by value for `'static`); `TaskManager` is a cheap Arc clone that shares the same store.
    let manager = server.tasks.clone();
    let task = manager.spawn(TaskOptions::new(), move |ctx| {
        Box::pin(async move {
            // The real work runs on a detached-on-cancel child task (see the fn-level note); the
            // outer future only races the tool's completion against cancellation.
            let task_ctx = ctx.clone();
            let mut work = tokio::spawn(async move {
                let _admission = admission;
                let tcc = rmcp::handler::server::tool::ToolCallContext::new(&server, request, context);
                CURRENT_TASK.scope(task_ctx, server.tool_router.call(tcc)).await
            });
            let outcome = tokio::select! {
                biased;
                () = ctx.cancelled() => return Err(TaskExit::Cancelled),
                joined = &mut work => joined,
            };
            match outcome {
                Ok(Ok(CallToolResponse::Complete(result))) => Ok(result),
                Ok(Ok(_)) => Err(TaskExit::Error(McpError::internal_error(
                    "tool returned a non-terminal response inside a task",
                    None,
                ))),
                Ok(Err(error)) => Err(TaskExit::Error(error)),
                Err(join_error) => Err(TaskExit::Error(McpError::internal_error(
                    format!("slow tool task failed to complete: {join_error}"),
                    None,
                ))),
            }
        })
    });
    CreateTaskResult::new(task)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        json.as_object().expect("object").clone()
    }

    #[test]
    fn should_offload_the_slow_admin_mode_and_not_its_fast_siblings() {
        assert!(is_slow_tool(
            "admin",
            Some(&args(serde_json::json!({ "mode": "rescan", "paths": ["src"] })))
        ));
        assert!(!is_slow_tool(
            "admin",
            Some(&args(serde_json::json!({ "mode": "status" })))
        ));
        assert!(!is_slow_tool("admin", None));
    }

    #[test]
    fn should_not_offload_a_tool_that_is_not_listed() {
        assert!(!is_slow_tool("outline", None));
    }

    /// The reason the table is keyed on `domain:mode` at all: `web` carries both a multi-page crawl
    /// and a body-less sitemap lookup, so a name-only key would offload every call of the domain or
    /// none of them.
    #[cfg(feature = "crawl")]
    #[test]
    fn should_offload_a_consolidated_domain_only_for_the_modes_that_are_slow() {
        assert!(is_slow_tool("web", Some(&args(serde_json::json!({ "mode": "crawl" })))));
        assert!(is_slow_tool(
            "web",
            Some(&args(serde_json::json!({ "mode": "scrape" })))
        ));
    }

    /// A `mode` belonging to some other domain must not match, or one domain's slow mode would
    /// offload another domain's fast operation of the same name.
    #[cfg(feature = "crawl")]
    #[test]
    fn should_not_match_a_mode_across_domains() {
        assert!(!is_slow_tool(
            "memory",
            Some(&args(serde_json::json!({ "mode": "map" })))
        ));
    }

    #[cfg(feature = "crawl")]
    #[test]
    fn should_not_offload_a_domain_call_with_an_absent_or_unknown_mode() {
        assert!(!is_slow_tool("web", None));
        assert!(!is_slow_tool(
            "web",
            Some(&args(serde_json::json!({ "mode": "sniff" })))
        ));
        assert!(!is_slow_tool("web", Some(&args(serde_json::json!({ "mode": 7 })))));
    }

    #[test]
    fn should_race_cancellation_only_for_the_pure_read_modes() {
        for (tool, mode) in [
            ("code", "grep"),
            ("code", "semantic"),
            ("graph", "map"),
            ("graph", "calls"),
            ("memory", "documents"),
        ] {
            assert!(is_cancel_aware_read(
                tool,
                Some(&args(serde_json::json!({ "mode": mode })))
            ));
        }
        for (tool, mode) in [
            ("admin", "rescan"),
            ("memory", "put"),
            ("memory", "accept"),
            ("code", "outline"),
            ("web", "crawl"),
        ] {
            assert!(!is_cancel_aware_read(
                tool,
                Some(&args(serde_json::json!({ "mode": mode })))
            ));
        }
        assert!(!is_cancel_aware_read("code", None));
    }

    /// Holds a heavy admission permit; dropping it (as a cancelled handler future does) frees it.
    #[tokio::test]
    async fn cancelling_a_read_returns_promptly_and_releases_the_permit() {
        use super::super::admission::{HeavyAdmission, WorkClass};
        let admission = HeavyAdmission::new(1, std::time::Duration::from_millis(50));
        let permit = admission.admit(WorkClass::Heavy).await.expect("permit");
        let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
        let work = async move {
            let _held = permit;
            std::future::pending::<Result<(), McpError>>().await
        };
        let call = tokio::spawn(run_until_cancelled(
            async {
                let _ = cancelled.await;
            },
            work,
        ));
        assert!(
            admission.admit(WorkClass::Heavy).await.is_err(),
            "permit is held while the read runs"
        );
        cancel.send(()).expect("send cancel");
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

    #[tokio::test]
    async fn an_uncancelled_read_returns_its_result() {
        let value = run_until_cancelled(std::future::pending::<()>(), async { Ok::<_, McpError>(7) })
            .await
            .expect("result");
        assert_eq!(value, 7);
    }
}
