//! Governance tool shims for `BasemindServer` — currently just `memory_audit`.
//!
//! Kept separate from `tools_memory.rs` so both files stay under the 1000-line cap.
//! Each shim delegates to `helpers_governance::run_memory_audit` and returns a graceful
//! MCP error when the `memory` feature is not compiled in.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::types_governance::MemoryAuditParams;

fn not_enabled(feature: &'static str) -> Result<CallToolResult, McpError> {
    Err(McpError::invalid_request(
        format!("{feature} feature not enabled — rebuild with --features {feature}"),
        None,
    ))
}

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_governance")]
impl BasemindServer {
    #[tool(
        description = "Verify stored memories' code references against the live index. \
        Checks file provenance (file deleted → Stale), symbol provenance (symbol missing or \
        body changed via structural hash → Stale), and command provenance (advisory only). \
        On Stale: decays `importance` by 50% and updates the `verified` field. \
        Auto-archives records continuously Stale for > 90 days (moved to `memory_archive`, \
        never deleted). `dry_run=true` previews verdicts without mutations. \
        `key` audits one specific record; omit for a full scope range scan. \
        Capped at `limit` records (default 100, max 1000). Needs --features memory."
    )]
    pub(crate) async fn memory_audit(
        &self,
        Parameters(p): Parameters<MemoryAuditParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            #[cfg(feature = "memory")]
            {
                return super::helpers_governance::run_memory_audit(&self.state, p).await;
            }
            #[cfg(not(feature = "memory"))]
            {
                let _ = p;
                return not_enabled("memory");
            }
            #[allow(unreachable_code)]
            not_enabled("memory")
        }
        .await;
        record_call(
            &self.state,
            "memory_audit",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }
}
