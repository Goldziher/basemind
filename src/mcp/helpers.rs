//! Pure helper functions used by the tool methods. Kept out of `mod.rs` so the tool impl
//! block stays focused on dispatch logic. Everything here is `pub(super)`.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde::Serialize;

use super::ServerState;

/// Build a successful tool result carrying both a text mirror and SEP-2106 `structured_content`.
///
/// Every tool routes its response through here, so populating `structured_content` in one place
/// gives the whole MCP surface typed output: `CallToolResult::structured` sets the parsed JSON on
/// `structured_content` (for clients that consume typed results) and keeps a `to_string()` text
/// block in `content` (for clients that only read text). No tool advertises an `output_schema`:
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
