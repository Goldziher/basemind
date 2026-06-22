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
        description = "Re-scan the working tree (or only `paths`) in-process: re-parses changed \
            files, updates the Fjall index, rebuilds the in-RAM cache. Holds an exclusive lock — \
            other MCP queries block until it returns (<1s for ~100 files). Use after editing code \
            to surface new symbols/calls/outlines without restarting. `full: true` forces a \
            complete re-index (overrides `paths`) when the index is stale or 'no indexed files'. \
            Returns scanned / updated / removed counts + elapsed time.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn rescan(
        &self,
        Parameters(p): Parameters<RescanParams>,
        peer: rmcp::Peer<rmcp::RoleServer>,
        meta: rmcp::model::Meta,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let progress_token = meta.get_progress_token();
        let __result: Result<CallToolResult, McpError> = async {
            super::helpers_admin::run_rescan(
                std::sync::Arc::clone(&self.state),
                p,
                &peer,
                progress_token,
            )
            .await
        }
        .await;
        record_call(&self.state, "rescan", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Aggregate `.basemind/telemetry.jsonl` into a usage summary: total tool \
            calls, per-tool histogram, total response bytes, estimated tokens saved vs the \
            grep+Read baseline. Optional `window` (`today` default, `1h`, `24h`, `all`) and `tool` \
            filter. `est_tokens_saved` is heuristic — each row carries a `saved_baseline` label.",
        annotations(read_only_hint = true, open_world_hint = false)
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
        description = "On-disk size + blob accounting for the `.basemind/` cache: recursive byte \
            sizes per component (blobs / views / lance / git-cache / telemetry), total blob-file \
            count, orphaned-blob count (unreferenced, reclaimable via `cache_gc`), and per-view \
            indexed file counts. Read-only; safe anytime.",
        annotations(read_only_hint = true, open_world_hint = false)
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
        description = "Garbage-collect orphaned extraction blobs from `.basemind/blobs/`. Blobs are \
            content-addressed and shared across views; re-scans and branch switches orphan blobs \
            no view references. Mark-and-sweep reclaims only those (referenced blobs untouched). \
            Runs under the live server's lock — safe anytime. Returns scanned / removed / \
            bytes_freed counts.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
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

    #[tool(
        description = "Clear a `.basemind/` cache component: \
            `blobs|views|lance|git-cache|telemetry|all`. Non-live caches (`git-cache`, \
            `telemetry`, `lance`) clear freely. `blobs` needs `confirm=true` (an in-process \
            rescan rebuilds it). `views`/`all` would yank the live Fjall index from under the \
            server, so they are refused in-process — stop it and run `basemind cache clear \
            --component views|all`. Returns the component and whether it was cleared.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
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
