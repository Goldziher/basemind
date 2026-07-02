//! Opt-in "lean" MCP tool surface (W5 slice 3).
//!
//! basemind normally advertises its full ~45-tool surface; an agent pays for every tool's
//! JSON schema on each session. When `BASEMIND_MCP_LEAN` is set to a truthy value, the server
//! instead advertises just three wrapper tools and defers the real schemas until requested:
//!
//! * `list_tools` — a compressed `name + one-line description` listing of every real tool.
//! * `get_tool_schema { tool_name }` — the full input JSON schema for one real tool.
//! * `invoke_tool { tool_name, tool_input }` — dispatches to the real tool handler and returns
//!   its result verbatim.
//!
//! This is STRICTLY opt-in. With the env var unset (or `0`/`off`/`false`/empty), the server
//! behaves exactly as before: every real tool is listed and dispatched through the generated
//! [`ToolRouter`]. The lean path reuses that same router — it never duplicates tool logic.

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, JsonObject, ListToolsResult, Tool, ToolAnnotations, object,
};
use rmcp::service::RequestContext;
use serde_json::{Value, json};

use super::BasemindServer;

/// Environment variable toggling the lean three-tool surface. Mirrors the `BASEMIND_GUARD`
/// opt-in convention: unset / `0` / `off` / `false` / empty = default full surface; any other
/// value = lean surface.
const LEAN_ENV: &str = "BASEMIND_MCP_LEAN";

/// The three wrapper tool names exposed in lean mode.
const TOOL_LIST: &str = "list_tools";
const TOOL_GET_SCHEMA: &str = "get_tool_schema";
const TOOL_INVOKE: &str = "invoke_tool";

/// Returns `true` when the lean surface is enabled via `BASEMIND_MCP_LEAN`.
///
/// Read on every `list_tools` / `call_tool` so the mode is decided per process from the
/// environment the server was launched with; the value does not change mid-session in practice
/// but re-reading keeps the helper free of cached global state.
pub(super) fn lean_mode_enabled() -> bool {
    std::env::var(LEAN_ENV).is_ok_and(|v| {
        let v = v.trim();
        !(v.is_empty()
            || v.eq_ignore_ascii_case("0")
            || v.eq_ignore_ascii_case("off")
            || v.eq_ignore_ascii_case("false"))
    })
}

/// Build the input schema for a wrapper tool from a small JSON Schema literal.
fn wrapper_schema(value: Value) -> Arc<JsonObject> {
    Arc::new(object(value))
}

/// The three wrapper tool definitions advertised in lean mode.
fn lean_tool_definitions() -> Vec<Tool> {
    vec![
        Tool::new(
            TOOL_LIST,
            "Lean-mode discovery: list every real basemind tool as a compressed \
             name + one-line description. Call get_tool_schema to fetch a tool's full input \
             schema, then invoke_tool to run it.",
            wrapper_schema(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
        )
        .annotate(ToolAnnotations::new().read_only(true).open_world(false)),
        Tool::new(
            TOOL_GET_SCHEMA,
            "Lean-mode schema fetch: return the full input JSON schema (and description) for \
             one real basemind tool by name.",
            wrapper_schema(json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "type": "string",
                        "description": "Name of the real basemind tool to describe."
                    }
                },
                "required": ["tool_name"],
                "additionalProperties": false
            })),
        )
        .annotate(ToolAnnotations::new().read_only(true).open_world(false)),
        Tool::new(
            TOOL_INVOKE,
            "Lean-mode dispatch: run a real basemind tool. Pass its name and the arguments \
             object it expects (the shape get_tool_schema returns); the result is returned \
             verbatim.",
            wrapper_schema(json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "type": "string",
                        "description": "Name of the real basemind tool to invoke."
                    },
                    "tool_input": {
                        "type": "object",
                        "description": "Arguments object passed through to the real tool."
                    }
                },
                "required": ["tool_name"],
                "additionalProperties": false
            })),
        )
        // Dispatches to ANY real tool (read or mutating, incl. web), so it cannot be marked
        // read-only — clients should gate it like a mutating tool; the target's own annotation
        // governs once resolved.
        .annotate(ToolAnnotations::new().read_only(false).open_world(true)),
    ]
}

/// Lean-mode `get_tool`: report only the three wrapper tools so rmcp's task-support
/// introspection sees the surface the client actually sees. Returns `None` for everything else.
pub(super) fn lean_get_tool(name: &str) -> Option<Tool> {
    lean_tool_definitions().into_iter().find(|t| t.name == name)
}

/// Lean-mode `list_tools`: advertise only the three wrapper tools.
pub(super) fn lean_list_tools() -> ListToolsResult {
    ListToolsResult {
        tools: lean_tool_definitions(),
        meta: None,
        next_cursor: None,
    }
}

/// Extract a required `&str` field from the wrapper arguments object.
fn required_str(args: Option<&JsonObject>, field: &str) -> Result<String, McpError> {
    args.and_then(|o| o.get(field))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| McpError::invalid_params(format!("missing required string `{field}`"), None))
}

/// Reject a request that targets one of the wrapper tools as a real tool, preventing recursion
/// (`invoke_tool { tool_name: "invoke_tool" }`) and confusing routing.
fn reject_wrapper_target(tool_name: &str) -> Result<(), McpError> {
    if matches!(tool_name, TOOL_LIST | TOOL_GET_SCHEMA | TOOL_INVOKE) {
        return Err(McpError::invalid_params(
            format!("`{tool_name}` is a lean wrapper tool, not a real basemind tool"),
            None,
        ));
    }
    Ok(())
}

/// Lean-mode `call_tool`: dispatch the three wrapper tools.
///
/// `list_tools` and `get_tool_schema` are answered from the router's static tool table.
/// `invoke_tool` rebuilds a [`ToolCallContext`] for the real tool and delegates to the SAME
/// [`ToolRouter::call`] the default surface uses — no tool logic is duplicated.
pub(super) async fn lean_call_tool(
    server: &BasemindServer,
    router: &ToolRouter<BasemindServer>,
    request: CallToolRequestParams,
    context: RequestContext<rmcp::RoleServer>,
) -> Result<CallToolResult, McpError> {
    match request.name.as_ref() {
        TOOL_LIST => {
            // Compressed listing: one row per real tool, sorted by name (list_all sorts).
            let rows: Vec<Value> = router
                .list_all()
                .into_iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description.unwrap_or(Cow::Borrowed("")),
                    })
                })
                .collect();
            structured_ok(json!({ "tools": rows }))
        }
        TOOL_GET_SCHEMA => {
            let tool_name = required_str(request.arguments.as_ref(), "tool_name")?;
            reject_wrapper_target(&tool_name)?;
            let tool = router
                .get(&tool_name)
                .ok_or_else(|| McpError::invalid_params(format!("unknown tool `{tool_name}`"), None))?;
            structured_ok(json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            }))
        }
        TOOL_INVOKE => {
            let tool_name = required_str(request.arguments.as_ref(), "tool_name")?;
            reject_wrapper_target(&tool_name)?;
            if !router.has_route(&tool_name) {
                return Err(McpError::invalid_params(format!("unknown tool `{tool_name}`"), None));
            }
            // `tool_input` is optional: a no-arg tool may be invoked with it omitted.
            let arguments = match request.arguments.as_ref().and_then(|o| o.get("tool_input")) {
                Some(Value::Object(map)) => Some(map.clone()),
                Some(Value::Null) | None => None,
                Some(other) => {
                    return Err(McpError::invalid_params(
                        format!("`tool_input` must be an object, got {other}"),
                        None,
                    ));
                }
            };
            // Rewrite the request to target the REAL tool and dispatch through the shared
            // router. `CallToolRequestParams` is `#[non_exhaustive]`, so mutate in place rather
            // than re-struct it — this also forwards `meta` / `task` untouched.
            let mut inner = request;
            inner.name = Cow::Owned(tool_name);
            inner.arguments = arguments;
            let tcc = ToolCallContext::new(server, inner, context);
            router.call(tcc).await
        }
        other => Err(McpError::invalid_params(
            format!(
                "lean mode exposes only `{TOOL_LIST}`, `{TOOL_GET_SCHEMA}`, `{TOOL_INVOKE}`; \
                 got `{other}`"
            ),
            None,
        )),
    }
}

/// Wrap a JSON value as a successful structured `CallToolResult` (text mirror + structured body),
/// matching how the real tool helpers shape their responses.
fn structured_ok(value: Value) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string(&value)
        .map_err(|e| McpError::internal_error(format!("serialize lean response: {e}"), None))?;
    let mut result = CallToolResult::success(vec![Content::text(text)]);
    result.structured_content = Some(value);
    Ok(result)
}
