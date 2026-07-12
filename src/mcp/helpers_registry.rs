//! Helper bodies for the machine-registry coordination MCP tools.
//!
//! Each `run_<tool>` resolves the lazily-connected
//! [`CommsClient`](crate::comms::client::CommsClient) via [`resolve_comms_client`], calls the
//! matching client method against the daemon's machine registry, maps the returned registry rows
//! into the MCP-facing DTOs, and `json_result`s the response. Worktree claims are ADVISORY: they
//! record intent in the registry but enforce nothing — a claim is a coordination hint, not a lock.

#![cfg(all(feature = "comms", any(unix, windows)))]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::json_result;
use super::helpers_comms::{comms_err, resolve_comms_client};
use super::types_registry::{
    BranchDto, BranchesParams, BranchesResponse, WorkspaceDto, WorkspacesParams, WorkspacesResponse,
    WorktreeClaimParams, WorktreeClaimResponse, WorktreeDto, WorktreeReleaseParams, WorktreesParams, WorktreesResponse,
};

pub(super) async fn run_workspaces(state: &ServerState, params: WorkspacesParams) -> Result<CallToolResult, McpError> {
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let records = client.list_workspaces().await.map_err(comms_err)?;
    let workspaces: Vec<WorkspaceDto> = records.iter().map(WorkspaceDto::from).collect();
    json_result(&WorkspacesResponse {
        total: workspaces.len(),
        workspaces,
    })
}

pub(super) async fn run_worktrees(state: &ServerState, params: WorktreesParams) -> Result<CallToolResult, McpError> {
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

pub(super) async fn run_branches(state: &ServerState, params: BranchesParams) -> Result<CallToolResult, McpError> {
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

pub(super) async fn run_worktree_claim(
    state: &ServerState,
    params: WorktreeClaimParams,
) -> Result<CallToolResult, McpError> {
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

pub(super) async fn run_worktree_release(
    state: &ServerState,
    params: WorktreeReleaseParams,
) -> Result<CallToolResult, McpError> {
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
