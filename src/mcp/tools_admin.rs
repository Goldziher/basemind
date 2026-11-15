//! The index verb tools of `BasemindServer`: `index_status` and `index_rescan` (ADR-0018).
//!
//! `index_rescan` is the only code tool that writes. Telemetry has no MCP tool and is reached
//! through `basemind admin telemetry`; the cache operations run offline in `basemind cache …`. See
//! `tools.rs` for the shim contract.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;

use super::BasemindServer;
use super::helpers_admin;
use super::strict::Strict;
use super::types::TelemetrySummaryParams;
use super::types_tools::{IndexRescanParams, IndexStatusParams};

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_admin")]
impl BasemindServer {
    #[tool(
        title = "Index status",
        description = "Use when a result is unexpectedly empty or says \"not indexed\": reports how \
        many files are indexed per language, whether a scan is running or the index is degraded, \
        the cache directory, and the repository's branch and HEAD. \
        Example: {}",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn index_status(
        &self,
        Parameters(args): Parameters<Strict<IndexStatusParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.index_status_call(p)).await
    }

    #[tool(
        title = "Re-index",
        description = "Use after editing files, or when the index is empty or stale: re-indexes the \
        given `paths`, or with no arguments the whole workspace, skipping unchanged files. Writes \
        only basemind's cache, never the repository. \
        Example: {\"paths\":[\"src/store.rs\"]}",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn index_rescan(
        &self,
        Parameters(args): Parameters<Strict<IndexRescanParams>>,
        peer: rmcp::Peer<rmcp::RoleServer>,
        meta: rmcp::model::RequestMetaObject,
    ) -> Result<CallToolResult, McpError> {
        let progress = Some((&peer, meta.get_progress_token()));
        self.answer(args, |p| self.index_rescan_with(p, progress)).await
    }
}

impl BasemindServer {
    /// Body of `index_status`, shared with `basemind status`.
    pub(crate) async fn index_status_call(&self, p: IndexStatusParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "index_status",
            p,
            helpers_admin::index_status(&self.state).await
        )
    }

    /// Body of `index_rescan`. `basemind rescan` reaches the same scanner through its own
    /// lock-aware driver (`src/main_rescan.rs`), which also owns the daemon hand-off.
    async fn index_rescan_with(
        &self,
        p: IndexRescanParams,
        progress: helpers_admin::Progress<'_>,
    ) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "index_rescan",
            p,
            helpers_admin::index_rescan(std::sync::Arc::clone(&self.state), p, progress).await
        )
    }

    /// Usage and token-savings summary. No MCP tool: `basemind admin telemetry`.
    pub(crate) async fn telemetry_call(&self, p: TelemetrySummaryParams) -> Result<CallToolResult, McpError> {
        super::helpers::run_telemetry_summary(&self.state, p).await
    }
}
