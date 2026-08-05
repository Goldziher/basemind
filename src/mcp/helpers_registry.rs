//! Helper bodies for the `workspace` domain tool's five modes.
//!
//! [`run_workspace`] validates the flat wire params and dispatches; each `run_<mode>` resolves the
//! lazily-connected [`CommsClient`](crate::comms::client::CommsClient) via [`resolve_comms_client`],
//! calls the matching client method against the daemon's machine registry, maps the returned
//! registry rows into the MCP-facing DTOs, and `json_result`s the response. Worktree claims are
//! ADVISORY: they record intent in the registry but enforce nothing — a claim is a coordination
//! hint, not a lock.

#![cfg(all(feature = "comms", any(unix, windows)))]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::json_result;
use super::helpers_comms::{comms_err, resolve_comms_client};
use super::mode::{WorkspaceMode, reject_unsupported};
use super::types_registry::{
    BranchDto, BranchesParams, BranchesResponse, WorkspaceDto, WorkspaceParams, WorkspacesParams, WorkspacesResponse,
    WorktreeClaimParams, WorktreeClaimResponse, WorktreeDto, WorktreeReleaseParams, WorktreesParams, WorktreesResponse,
};

/// Fail a mode that needs a sibling field the caller omitted, naming the exact pair.
///
/// "missing parameter" would leave the agent guessing which of the four optional siblings the mode
/// it picked actually needs, so the message quotes both the mode and the field.
fn require(mode: WorkspaceMode, field: &str, value: Option<String>) -> Result<String, McpError> {
    value.ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "`{}` mode=\"{}\" requires `{field}`",
                WorkspaceMode::DOMAIN,
                mode.as_str()
            ),
            None,
        )
    })
}

/// Dispatch the single `workspace` tool onto the per-mode helper its `mode` selects.
///
/// Validation runs before the daemon connection so a malformed call costs no broker round-trip, and
/// fields belonging to another mode are rejected rather than dropped: a silently ignored `name` on a
/// `worktrees` call reads to an agent as a successful filtered listing.
pub(super) async fn run_workspace(state: &ServerState, params: WorkspaceParams) -> Result<CallToolResult, McpError> {
    let WorkspaceParams {
        mode,
        repo_id,
        name,
        as_agent,
    } = params;
    let reject = |present: &[(&str, bool)]| reject_unsupported(WorkspaceMode::DOMAIN, mode.as_str(), present);

    match mode {
        WorkspaceMode::Workspaces => {
            reject(&[("repo_id", repo_id.is_some()), ("name", name.is_some())])?;
            run_workspaces(state, WorkspacesParams { as_agent }).await
        }
        WorkspaceMode::Worktrees => {
            reject(&[("name", name.is_some())])?;
            let repo_id = require(mode, "repo_id", repo_id)?;
            run_worktrees(state, WorktreesParams { repo_id, as_agent }).await
        }
        WorkspaceMode::Branches => {
            reject(&[("name", name.is_some())])?;
            let repo_id = require(mode, "repo_id", repo_id)?;
            run_branches(state, BranchesParams { repo_id, as_agent }).await
        }
        WorkspaceMode::Claim => {
            let repo_id = require(mode, "repo_id", repo_id)?;
            let name = require(mode, "name", name)?;
            run_worktree_claim(
                state,
                WorktreeClaimParams {
                    repo_id,
                    name,
                    as_agent,
                },
            )
            .await
        }
        WorkspaceMode::Release => {
            let repo_id = require(mode, "repo_id", repo_id)?;
            let name = require(mode, "name", name)?;
            run_worktree_release(
                state,
                WorktreeReleaseParams {
                    repo_id,
                    name,
                    as_agent,
                },
            )
            .await
        }
    }
}

async fn run_workspaces(state: &ServerState, params: WorkspacesParams) -> Result<CallToolResult, McpError> {
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let records = client.list_workspaces().await.map_err(comms_err)?;
    let workspaces: Vec<WorkspaceDto> = records.iter().map(WorkspaceDto::from).collect();
    json_result(&WorkspacesResponse {
        total: workspaces.len(),
        workspaces,
    })
}

async fn run_worktrees(state: &ServerState, params: WorktreesParams) -> Result<CallToolResult, McpError> {
    let repo_id = params.repo_id.clone();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let records = client.list_worktrees(params.repo_id).await.map_err(comms_err)?;
    let worktrees: Vec<WorktreeDto> = records.iter().map(WorktreeDto::from).collect();
    json_result(&WorktreesResponse {
        repo_id,
        total: worktrees.len(),
        worktrees,
    })
}

async fn run_branches(state: &ServerState, params: BranchesParams) -> Result<CallToolResult, McpError> {
    let repo_id = params.repo_id.clone();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let records = client.list_branches(params.repo_id).await.map_err(comms_err)?;
    let branches: Vec<BranchDto> = records.iter().map(BranchDto::from).collect();
    json_result(&BranchesResponse {
        repo_id,
        total: branches.len(),
        branches,
    })
}

async fn run_worktree_claim(state: &ServerState, params: WorktreeClaimParams) -> Result<CallToolResult, McpError> {
    let repo_id = params.repo_id.clone();
    let name = params.name.clone();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let claimant = client.agent().as_str().to_string();
    let held = client
        .claim_worktree(params.repo_id, params.name, claimant.clone())
        .await
        .map_err(comms_err)?;
    json_result(&WorktreeClaimResponse {
        repo_id,
        name,
        claimant,
        held,
    })
}

async fn run_worktree_release(state: &ServerState, params: WorktreeReleaseParams) -> Result<CallToolResult, McpError> {
    let repo_id = params.repo_id.clone();
    let name = params.name.clone();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let claimant = client.agent().as_str().to_string();
    let held = client
        .release_worktree(params.repo_id, params.name, claimant.clone())
        .await
        .map_err(comms_err)?;
    json_result(&WorktreeClaimResponse {
        repo_id,
        name,
        claimant,
        held,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_name_the_mode_and_field_when_a_required_sibling_is_missing() {
        let error = require(WorkspaceMode::Claim, "repo_id", None).expect_err("a missing repo_id must fail");
        let message = error.message.to_string();
        assert_eq!(message, "`workspace` mode=\"claim\" requires `repo_id`");
    }

    #[test]
    fn should_pass_a_supplied_sibling_through_untouched() {
        let value = require(WorkspaceMode::Worktrees, "repo_id", Some("path:/tmp/repo".to_string()))
            .expect("a supplied repo_id must pass");
        assert_eq!(value, "path:/tmp/repo");
    }
}
