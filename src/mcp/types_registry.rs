//! Request / response shapes for the machine-registry coordination MCP tools.
//!
//! The registry itself persists msgpack structs ([`WorkspaceRecord`](crate::registry::WorkspaceRecord)
//! et al.) that derive serde but NOT `JsonSchema`. To keep the MCP schema surface honest and stable,
//! this module defines MCP-facing DTOs and maps the registry rows into them at the helper boundary —
//! the raw persistence structs never leak into the tool schema.

#![cfg(all(feature = "comms", any(unix, windows)))]

use serde::{Deserialize, Serialize};

use crate::registry::{BranchRecord, WorkspaceKind, WorkspaceRecord, WorktreeRecord};

/// Params for `workspaces`: list every registered workspace in the machine registry.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WorkspacesParams {
    /// Optional sub-identity to act as; defaults to the server's own agent. Lets one orchestrator
    /// drive many named subagents.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// One workspace row in a `workspaces` response.
#[derive(Debug, Serialize)]
pub(super) struct WorkspaceDto {
    /// Stable workspace key (blake3 of the canonical root); also the cache-dir identity.
    pub key: String,
    /// `"git"` or `"plain"`.
    pub kind: String,
    /// Canonical workspace root.
    pub root: String,
    /// Owning repo id, absent for a plain workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    /// Main-worktree root of the owning clone, absent for a plain workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_worktree: Option<String>,
    /// Unix micros of the last register/refresh.
    pub last_seen: i64,
}

impl From<&WorkspaceRecord> for WorkspaceDto {
    fn from(record: &WorkspaceRecord) -> Self {
        Self {
            key: record.key.clone(),
            kind: match record.kind {
                WorkspaceKind::Git => "git".to_string(),
                WorkspaceKind::Plain => "plain".to_string(),
            },
            root: record.root.display().to_string(),
            repo_id: record.repo_id.clone(),
            main_worktree: record.main_worktree.as_ref().map(|p| p.display().to_string()),
            last_seen: record.last_seen,
        }
    }
}

/// Response for `workspaces`.
#[derive(Debug, Serialize)]
pub(super) struct WorkspacesResponse {
    /// Number of workspaces returned.
    pub total: usize,
    /// The workspace rows, sorted by key.
    pub workspaces: Vec<WorkspaceDto>,
}

/// Params for `worktrees`: list the worktrees of a registered repo.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WorktreesParams {
    /// The repo id (normalized remote URL or `path:<root>`) whose worktrees to list.
    pub repo_id: String,
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// One worktree row in a `worktrees` response.
#[derive(Debug, Serialize)]
pub(super) struct WorktreeDto {
    /// Owning repo id.
    pub repo_id: String,
    /// `"(main)"` or the linked-worktree directory name.
    pub name: String,
    /// Absolute, canonical checkout root.
    pub path: String,
    /// Head commit sha, absent on an unborn HEAD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// Checked-out branch, absent when detached or unresolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// True when HEAD is detached.
    pub detached: bool,
    /// Advisory claimant currently holding this worktree, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    /// Unix micros of the last refresh.
    pub last_seen: i64,
}

impl From<&WorktreeRecord> for WorktreeDto {
    fn from(record: &WorktreeRecord) -> Self {
        Self {
            repo_id: record.repo_id.clone(),
            name: record.name.clone(),
            path: record.path.display().to_string(),
            head_sha: record.head_sha.clone(),
            branch: record.branch.clone(),
            detached: record.detached,
            claimed_by: record.claimed_by.clone(),
            last_seen: record.last_seen,
        }
    }
}

/// Response for `worktrees`.
#[derive(Debug, Serialize)]
pub(super) struct WorktreesResponse {
    /// The repo id queried.
    pub repo_id: String,
    /// Number of worktrees returned.
    pub total: usize,
    /// The worktree rows, sorted by name.
    pub worktrees: Vec<WorktreeDto>,
}

/// Params for `branches`: list the local branches of a registered repo.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BranchesParams {
    /// The repo id whose branches to list.
    pub repo_id: String,
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// One branch row in a `branches` response.
#[derive(Debug, Serialize)]
pub(super) struct BranchDto {
    /// Owning repo id.
    pub repo_id: String,
    /// Short branch name (`refs/heads/` stripped).
    pub name: String,
    /// 40-hex head commit sha.
    pub head_sha: String,
    /// Unix micros of the last refresh.
    pub last_seen: i64,
}

impl From<&BranchRecord> for BranchDto {
    fn from(record: &BranchRecord) -> Self {
        Self {
            repo_id: record.repo_id.clone(),
            name: record.name.clone(),
            head_sha: record.head_sha.clone(),
            last_seen: record.last_seen,
        }
    }
}

/// Response for `branches`.
#[derive(Debug, Serialize)]
pub(super) struct BranchesResponse {
    /// The repo id queried.
    pub repo_id: String,
    /// Number of branches returned.
    pub total: usize,
    /// The branch rows, sorted by name.
    pub branches: Vec<BranchDto>,
}

/// Params for `worktree_claim`: advisory-claim a worktree.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WorktreeClaimParams {
    /// The owning repo id.
    pub repo_id: String,
    /// The worktree name (`"(main)"` or the linked-worktree directory name).
    pub name: String,
    /// Optional claimant sub-identity; defaults to the server's own agent id.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Params for `worktree_release`: release an advisory worktree claim.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WorktreeReleaseParams {
    /// The owning repo id.
    pub repo_id: String,
    /// The worktree name whose claim to release.
    pub name: String,
    /// Optional claimant sub-identity; defaults to the server's own agent id.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Response for `worktree_claim` / `worktree_release`.
#[derive(Debug, Serialize)]
pub(super) struct WorktreeClaimResponse {
    /// The owning repo id.
    pub repo_id: String,
    /// The worktree name acted on.
    pub name: String,
    /// The claimant identity the claim/release ran as.
    pub claimant: String,
    /// For a claim: `true` when the claim is now held by the claimant. For a release: `true` when a
    /// claim by the claimant was cleared. `false` otherwise (unknown worktree, or held by another).
    pub held: bool,
}
