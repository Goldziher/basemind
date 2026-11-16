//! Daemon-management and registry RPCs of [`CommsClient`](super::CommsClient). Split out of
//! `client.rs` to keep it under the 1000-line `rust-max-lines` cap.

use super::*;
use crate::comms::workspace_pool::AccessedWorkspace;

impl CommsClient {
    /// Ask the daemon whether it answers at all.
    pub async fn ping(&mut self) -> Result<(), CommsClientError> {
        match self.request(CommsRequest::Ping).await? {
            CommsResponse::Pong => Ok(()),
            other => Err(self.shape_err(other, "ping")),
        }
    }

    /// Ask the daemon for its status snapshot.
    pub async fn status(&mut self) -> Result<StatusReport, CommsClientError> {
        match self.request(CommsRequest::Status).await? {
            CommsResponse::Status(s) => Ok(s),
            other => Err(self.shape_err(other, "status")),
        }
    }

    /// Ask the daemon to drain and stop.
    pub async fn stop(&mut self) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::Stop, "stop").await
    }

    /// List the workspaces the daemon currently holds hot (drives the `basemind statusline` CLI).
    pub async fn accessed_paths(&mut self) -> Result<Vec<AccessedWorkspace>, CommsClientError> {
        match self.request(CommsRequest::AccessedPaths).await? {
            CommsResponse::Accessed { workspaces } => Ok(workspaces),
            other => Err(self.shape_err(other, "accessed_paths")),
        }
    }

    /// List every registered workspace in the daemon's machine registry (git + plain). Read-only.
    pub async fn list_workspaces(&mut self) -> Result<Vec<crate::registry::WorkspaceRecord>, CommsClientError> {
        match self.request(CommsRequest::WorkspacesList).await? {
            CommsResponse::Workspaces { workspaces } => Ok(workspaces),
            other => Err(self.shape_err(other, "list_workspaces")),
        }
    }

    /// List the worktrees of a registered repo by id. An unknown repo id returns an empty list.
    pub async fn list_worktrees(
        &mut self,
        repo_id: String,
    ) -> Result<Vec<crate::registry::WorktreeRecord>, CommsClientError> {
        match self.request(CommsRequest::WorktreesList { repo_id }).await? {
            CommsResponse::Worktrees { worktrees } => Ok(worktrees),
            other => Err(self.shape_err(other, "list_worktrees")),
        }
    }

    /// List the local branches of a registered repo by id. An unknown repo id returns an empty list.
    pub async fn list_branches(
        &mut self,
        repo_id: String,
    ) -> Result<Vec<crate::registry::BranchRecord>, CommsClientError> {
        match self.request(CommsRequest::BranchesList { repo_id }).await? {
            CommsResponse::Branches { branches } => Ok(branches),
            other => Err(self.shape_err(other, "list_branches")),
        }
    }
}
