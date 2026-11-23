//! Pure helper functions used by the tool methods. Kept out of `mod.rs` so the tool impl
//! block stays focused on dispatch logic. Everything here is `pub(super)`.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde::Serialize;

use super::ServerState;

/// Build a successful tool result carrying the answer as `structured_content` and a `to_string()`
/// text block.
///
/// Every tool routes its response through here. In-process callers (the CLI, resources) read
/// `structured_content`; the MCP wire sends the text alone ([`super::tool_contract::single_payload`]). No tool advertises an `output_schema`:
/// one tool answers in several shapes, and `tests/mcp_smoke.rs` holds every verb tool to that.
pub(super) fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let value =
        serde_json::to_value(value).map_err(|e| McpError::internal_error(format!("serialize response: {e}"), None))?;
    Ok(CallToolResult::structured(value))
}

/// Body for the `telemetry_summary` MCP tool. Thin wrapper — the aggregation logic
/// lives in [`super::telemetry::summarize`] so this module stays under the line cap.
pub(super) async fn run_telemetry_summary(
    state: &ServerState,
    params: super::types::TelemetrySummaryParams,
) -> Result<CallToolResult, McpError> {
    let response = super::telemetry::summarize(state.telemetry.path(), params).await?;
    json_result(&response)
}
