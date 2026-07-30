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

/// Tools whose work can dominate the transport for long enough that a task-capable client is better
/// served an async handle it can poll than a blocked `tools/call`. Kept deliberately small and
/// centralized: only tools that routinely run for seconds belong here. Feature-gated entries drop
/// out of the slice when their tool is not compiled in, so the set never names a tool the router
/// does not advertise.
pub(super) const SLOW_TOOLS: &[&str] = &[
    "rescan",
    #[cfg(feature = "documents")]
    "search_documents",
    #[cfg(feature = "crawl")]
    "web_scrape",
    #[cfg(feature = "crawl")]
    "web_crawl",
    #[cfg(feature = "crawl")]
    "web_map",
];

/// Whether `name` is one of the [`SLOW_TOOLS`] eligible for task offload.
pub(super) fn is_slow_tool(name: &str) -> bool {
    SLOW_TOOLS.contains(&name)
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
) -> CreateTaskResult {
    let server = server.clone();
    // Clone the manager handle out so the spawned closure can move `server` wholesale (it needs the
    // router by value for `'static`); `TaskManager` is a cheap Arc clone that shares the same store.
    let manager = server.tasks.clone();
    let task = manager.spawn(TaskOptions::new(), move |ctx| {
        Box::pin(async move {
            // The real work runs on a detached-on-cancel child task (see the fn-level note); the
            // outer future only races the tool's completion against cancellation.
            let mut work = tokio::spawn(async move {
                let tcc = rmcp::handler::server::tool::ToolCallContext::new(&server, request, context);
                server.tool_router.call(tcc).await
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
