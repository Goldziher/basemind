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
    "rescan",
    #[cfg(feature = "documents")]
    "search_documents",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        json.as_object().expect("object").clone()
    }

    #[test]
    fn should_offload_a_bare_named_slow_tool_whatever_its_arguments() {
        assert!(is_slow_tool("rescan", None));
        assert!(is_slow_tool(
            "rescan",
            Some(&args(serde_json::json!({ "paths": ["src"] })))
        ));
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
}
