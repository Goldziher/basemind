//! Machine-registry coordination tool shims for `BasemindServer`.
//!
//! Five thin `#[tool]` wrappers over the daemon's machine registry (workspaces / worktrees /
//! branches / advisory worktree claim + release). Each delegates to a `helpers_registry::run_*`
//! body and records telemetry. Registered only under `#[cfg(feature = "comms")]`; the registry data
//! lives in the comms daemon (the sole writer), so these tools read/mutate it over the broker link.

#![cfg(all(feature = "comms", any(unix, windows)))]

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::types_registry::{
    BranchesParams, WorkspacesParams, WorktreeClaimParams, WorktreeReleaseParams, WorktreesParams,
};

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_registry")]
impl BasemindServer {
    #[tool(
        description = "List every workspace the machine registry has seen: git worktrees and plain \
        (non-git) directories, each with its stable key, kind, root, owning repo id, and last-seen \
        time. Reads the comms daemon's machine registry (populated as serve sessions connect). \
        Needs --features comms.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn workspaces(
        &self,
        Parameters(p): Parameters<WorkspacesParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_registry::run_workspaces(&self.state, p).await;
        record_call(&self.state, "workspaces", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "List the worktrees of a registered repo by its `repo_id` (a normalized remote \
        URL, else `path:<root>`; see `workspaces` for known ids). Each row carries the worktree name \
        (`(main)` or the linked directory name), checkout path, head sha, branch, and any advisory \
        claimant. Reads the machine registry; an unknown repo id returns an empty list. \
        Needs --features comms.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn worktrees(
        &self,
        Parameters(p): Parameters<WorktreesParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_registry::run_worktrees(&self.state, p).await;
        record_call(&self.state, "worktrees", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "List the local branches of a registered repo by its `repo_id`, each with its \
        short name and 40-hex head sha. Reads the machine registry; an unknown repo id returns an \
        empty list. Needs --features comms.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn branches(&self, Parameters(p): Parameters<BranchesParams>) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_registry::run_branches(&self.state, p).await;
        record_call(&self.state, "branches", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "ADVISORY-claim a worktree of a registered repo for this agent, to signal to \
        peers that you're working it. The claim is a COORDINATION HINT recorded in the machine \
        registry — it enforces NOTHING and blocks no file access. Returns `held: true` when the \
        claim is now yours (freshly taken or already yours), `false` when another agent holds it or \
        the worktree is unknown. Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn worktree_claim(
        &self,
        Parameters(p): Parameters<WorktreeClaimParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_registry::run_worktree_claim(&self.state, p).await;
        record_call(&self.state, "worktree_claim", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Release YOUR advisory claim on a worktree (the inverse of `worktree_claim`). \
        Only clears a claim held by this agent. Returns `held: true` when a claim of yours was \
        cleared, `false` when the worktree is unknown or held by someone else / no one. \
        Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn worktree_release(
        &self,
        Parameters(p): Parameters<WorktreeReleaseParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_registry::run_worktree_release(&self.state, p).await;
        record_call(&self.state, "worktree_release", &__params_json, __started, &__result);
        __result
    }
}
