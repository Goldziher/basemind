//! Governance tool shims for `BasemindServer`: `memory_audit`, `proposals_mine`,
//! `proposals_list`, `proposal_accept`, `proposal_reject`.
//!
//! Kept separate from `tools_memory.rs` so both files stay under the 1000-line cap.
//! Each shim delegates to `helpers_governance` / `helpers_proposals` and returns a graceful
//! MCP error when the `memory` feature is not compiled in.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::types_governance::{
    MemoryAuditParams, ProposalAcceptParams, ProposalRejectParams, ProposalsListParams, ProposalsMineParams,
};

fn not_enabled(feature: &'static str) -> Result<CallToolResult, McpError> {
    Err(McpError::invalid_request(
        format!(
            "this tool requires the `{feature}` feature, which is not compiled into this \
             basemind binary. Rebuild with `--features {feature}` (the published release \
             binary includes it)."
        ),
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
        Capped at `limit` records (default 100, max 1000). Needs --features memory.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
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
        record_call(&self.state, "memory_audit", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Mine co-change skill proposals from recent git history using association-rule \
        analysis. For each commit, counts pairs of files that changed together; emits a candidate when \
        `support` (co-change count) >= `min_support` (default 5) AND `confidence` \
        (support / anchor_freq) >= `min_confidence` (default 0.6). Skips commits touching more than \
        `max_files_per_commit` files (default 25) to avoid bulk/vendor commits dominating. \
        Proposals are content-addressed (blake3 of the sorted file-set) so re-mining is idempotent. \
        Previously rejected proposals are suppressed via tombstone. \
        Returns counts only — use `proposals_list` to browse, `proposal_accept` / \
        `proposal_reject` to act. Requires git + --features memory.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn proposals_mine(
        &self,
        Parameters(p): Parameters<ProposalsMineParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            #[cfg(feature = "memory")]
            {
                return super::helpers_proposals::run_proposals_mine(&self.state, p).await;
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
        record_call(&self.state, "proposals_mine", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "List pending governance proposals for this repo scope. \
        Optional `kind` filter: `\"skill\"` (co-change) or `\"memory\"` (future use); omit for all. \
        Capped at `limit` results (default 100, max 1000). \
        Pass `cursor` from a previous response's `next_cursor` for the next page; cursors are \
        Fjall-backed and stable across rescans. Propose-don't-commit: proposals are not yet \
        searchable — use `proposal_accept` to promote to memory or `proposal_reject` to suppress. \
        Needs --features memory.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn proposals_list(
        &self,
        Parameters(p): Parameters<ProposalsListParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            #[cfg(feature = "memory")]
            {
                return super::helpers_proposals::run_proposals_list(&self.state, p).await;
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
        record_call(&self.state, "proposals_list", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Accept a co-change proposal: promote it to a searchable, LanceDB-embedded \
        memory record tagged `[\"skill\",\"cochange\"]` with file provenance set from the proposal's \
        file-set. Stamps `verified` via the W10 audit engine (file-existence check against the live \
        index) so a later `memory_audit` will mark it Stale if any referenced file disappears. \
        The proposal is deleted from the proposals keyspace after promotion. \
        Optional `key` overrides the auto-derived `\"skill/cochange-<short_id>\"` memory key. \
        Returns `{ accepted: true, memory_key }`. Needs --features memory.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn proposal_accept(
        &self,
        Parameters(p): Parameters<ProposalAcceptParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            #[cfg(feature = "memory")]
            {
                return super::helpers_proposals::run_proposal_accept(&self.state, p).await;
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
        record_call(&self.state, "proposal_accept", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Reject a pending proposal: delete it from the proposals keyspace and write \
        a tombstone so `proposals_mine` will not resurface the same candidate in future runs. \
        Optional `reason` is logged but not persisted. \
        Returns `{ rejected: true }`. Idempotent — calling on an already-rejected id is safe. \
        Needs --features memory.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn proposal_reject(
        &self,
        Parameters(p): Parameters<ProposalRejectParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = async {
            #[cfg(feature = "memory")]
            {
                return super::helpers_proposals::run_proposal_reject(&self.state, p).await;
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
        record_call(&self.state, "proposal_reject", &__params_json, __started, &__result);
        __result
    }
}
