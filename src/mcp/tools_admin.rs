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
use super::types::{
    CacheClearParams, CacheGcParams, CacheStatsParams, RescanParams, TelemetrySummaryParams,
};

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
    pub(crate) async fn rescan(
        &self,
        Parameters(p): Parameters<RescanParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> =
            async { super::helpers::run_rescan(std::sync::Arc::clone(&self.state), p).await }.await;
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
    pub(crate) async fn telemetry_summary(
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

    #[tool(
        description = "Report on-disk size + blob accounting for the `.basemind/` cache. \
            Returns recursive byte sizes per component (blobs / views / lance / git-cache / \
            telemetry), the total blob-file count, the orphaned-blob count (blobs no view \
            references — reclaimable via `cache_gc`), and per-view indexed file counts. \
            Read-only; safe to run anytime."
    )]
    pub(crate) async fn cache_stats(
        &self,
        Parameters(p): Parameters<CacheStatsParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            super::helpers_admin::run_cache_stats(std::sync::Arc::clone(&self.state), p).await
        }
        .await;
        record_call(
            &self.state,
            "cache_stats",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Garbage-collect orphaned extraction blobs from `.basemind/blobs/`. \
            Blobs are content-addressed and shared across views; re-scans and branch switches \
            leave behind blobs that no view's index references anymore. This mark-and-sweep \
            reclaims exactly those orphans (referenced blobs are never touched). Runs \
            in-process under the live server's lock — safe to run anytime, including against a \
            busy server. Returns scanned / removed / bytes_freed counts."
    )]
    pub(crate) async fn cache_gc(
        &self,
        Parameters(p): Parameters<CacheGcParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            super::helpers_admin::run_cache_gc(std::sync::Arc::clone(&self.state), p).await
        }
        .await;
        record_call(
            &self.state,
            "cache_gc",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(description = "Clear a whole `.basemind/` cache component: \
            `blobs|views|lance|git-cache|telemetry|all`. The non-live caches \
            (`git-cache`, `telemetry`, `lance`) clear freely. `blobs` backs the code map but is \
            content-addressed files (not an open handle), so it requires `confirm=true` and an \
            in-process rescan rebuilds it afterwards. `views` and `all` remove the live Fjall \
            index (and, for `all`, the lock) out from under the running server, so they are \
            refused in-process — stop the server and run \
            `basemind cache clear --component views|all` instead. Returns the targeted \
            component and whether it was cleared.")]
    pub(crate) async fn cache_clear(
        &self,
        Parameters(p): Parameters<CacheClearParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            super::helpers_admin::run_cache_clear(std::sync::Arc::clone(&self.state), p).await
        }
        .await;
        record_call(
            &self.state,
            "cache_clear",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }
}
