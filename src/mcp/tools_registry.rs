//! The `workspace` domain tool shim for `BasemindServer`.
//!
//! One tool, one required `mode` — `workspaces` / `worktrees` / `branches` / `claim` / `release` —
//! dispatched to `helpers_registry::run_workspace`. The whole module is gated on
//! `feature = "comms"`: the registry data lives in the comms daemon (its sole writer), so with the
//! feature off there is nothing to read and the tool is never registered rather than answering with
//! a `not_enabled` stub.

#![cfg(all(feature = "comms", any(unix, windows)))]

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::lenient::Lenient;
use super::types_registry::WorkspaceParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_registry")]
impl BasemindServer {
    // No `output_schema`: the five modes return three different response shapes, and SEP-2106 allows
    // exactly one per tool. Declaring a union would mean nested structs, which schemars emits as
    // `$ref` into `$defs` — the construct that silently dropped the whole registry in GH #50. The
    // per-mode shapes are documented in the description instead. ~keep
    //
    // The annotations are the union of the five modes': `claim`/`release` write, so the tool cannot
    // claim `read_only_hint`, but every mode is idempotent and none destroys data. ~keep
    #[tool(
        description = "Find out which repos, worktrees and branches this machine has, and who is \
        already working them, before you edit a shared checkout. `mode` is required. `workspaces` \
        lists every workspace the daemon has seen — git checkouts and plain directories — with its \
        stable key, kind, root, owning repo id and last-seen time; start here to learn the `repo_id` \
        the other modes take. `worktrees` lists the git worktrees of one repo by `repo_id` (a \
        normalized remote URL, else `path:<root>`), each with its name (`(main)` or the linked \
        directory name), checkout path, head sha, branch, and the agent currently claiming it — use \
        it to check whether another session already owns the tree you were about to edit. \
        `branches` lists that repo's local branches with their 40-hex head shas. `claim` takes an \
        ADVISORY claim on a worktree (`repo_id` + `name`) to tell peers you are working it, and \
        `release` gives up a claim you hold. A claim is a COORDINATION HINT recorded in the \
        registry: it enforces nothing, locks nothing, and blocks no file access — `held: true` \
        means the claim is yours (claim) or yours was cleared (release), `false` means another \
        agent holds it or the worktree is unknown. Reads the comms daemon's machine registry, \
        populated as serve sessions connect; an unknown repo id returns an empty list. `repo_id` is \
        required by every mode but `workspaces`, `name` additionally by `claim` and `release`; \
        parameters that belong to another mode are rejected, not ignored. Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn workspace(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<WorkspaceParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __key = p.mode.telemetry_key();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = super::helpers_registry::run_workspace(&self.state, p).await;
        record_call(&self.state, __key, &__params_json, __started, &__result);
        __result
    }
}
