//! Admin / housekeeping tool shims for `BasemindServer`.
//!
//! These are operations that mutate basemind's own on-disk state (index,
//! caches) rather than just querying it. Kept in a separate file so
//! `tools.rs` stays under the 1000-line cap.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::types::{RescanParams, TelemetrySummaryParams};

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_admin")]
impl BasemindServer {
    #[tool(
        description = "Refresh basemind's index by running the scanner in-process. \
            Walks the working tree (or only the supplied `paths`), re-parses changed files, \
            updates the Fjall index, and rebuilds the in-RAM map cache. \
            Holds an exclusive lock for the duration of the scan — other MCP queries block \
            until it returns. Cheap on small repos (<1s for ~100 files). Use after editing \
            code when you need new symbols / calls / outlines to show up without restarting \
            the MCP server. Returns scanned / updated / removed counts and elapsed time."
    )]
    async fn rescan(
        &self,
        Parameters(p): Parameters<RescanParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> =
            async { super::helpers::run_rescan(&self.state, p).await }.await;
        record_call(&self.state, "rescan", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Aggregate `.basemind/telemetry.jsonl` into a usage summary: \
            total tool calls, per-tool histogram, total response bytes, and \
            estimated tokens saved vs the disclosed grep+Read baseline. Optional \
            `window` (`today` default, `1h`, `24h`, `all`) and `tool` filter. \
            The `est_tokens_saved` numbers are heuristics — every row carries a \
            `saved_baseline` label disclosing the assumption. Pairs with the \
            shipped `plugins/basemind/statusline.sh` and the `/basemind-stats` skill."
    )]
    async fn telemetry_summary(
        &self,
        Parameters(p): Parameters<TelemetrySummaryParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> =
            async { super::helpers::run_telemetry_summary(&self.state, p).await }.await;
        record_call(
            &self.state,
            "telemetry_summary",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }
}
