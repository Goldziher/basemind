//! `basemind workspace` — read-only listings of the daemon's machine registry.
//!
//! CLI only: the registry has no MCP tool.
//!
//! The registry lives in the always-on comms broker daemon (its sole writer). Like the
//! [`agents`](super::agents) group — and unlike the code-map / memory CLI groups — these
//! subcommands connect to the daemon DIRECTLY via [`CommsClient::ensure_and_connect`] rather than
//! building a full [`BasemindServer`](crate::mcp::BasemindServer), so they take no repo index lock
//! and cannot clash with a running `basemind serve`.
//!
//! `--json` emits the structured response for every listing.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::cli::render::render_json;
use crate::comms::client::{CommsClient, scope_context_for};
use crate::comms::ids::AgentId;
use crate::registry::{BranchRecord, WorkspaceKind, WorkspaceRecord, WorktreeRecord};

/// The `workspace` listings; they talk to the broker daemon directly.
#[derive(Subcommand, Debug)]
pub enum WorkspaceCmd {
    /// List every registered workspace in the machine registry (git + plain).
    Workspaces {
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// List the worktrees of a registered repo by id.
    Worktrees {
        /// The repo id (normalized remote URL or `path:<root>`) whose worktrees to list.
        repo_id: String,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// List the local branches of a registered repo by id.
    Branches {
        /// The repo id whose branches to list.
        repo_id: String,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
}

/// Resolve the CLI agent identity through the ONE shared resolver
/// ([`crate::comms::identity::cli_agent_id`]) — the same identity the `serve` session in this
/// workspace uses.
fn cli_agent_id(root: &Path) -> AgentId {
    crate::comms::identity::cli_agent_id(root)
}

/// Connect a [`CommsClient`] to the broker as a resolved identity.
async fn connect_as(root: &Path, as_agent: Option<String>) -> Result<CommsClient> {
    let agent = match as_agent {
        Some(raw) => AgentId::parse(raw.clone()).with_context(|| format!("invalid --as-agent {raw:?}"))?,
        None => cli_agent_id(root),
    };
    let (remote, cwd) = scope_context_for(root);
    CommsClient::ensure_and_connect(agent, remote, cwd)
        .await
        .map_err(|e| anyhow::anyhow!("connect to comms daemon: {e}"))
}

/// Dispatch one `workspace` subcommand. Builds a small current-thread runtime, then runs it.
pub fn run(root: &Path, json: bool, cmd: WorkspaceCmd) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(async move {
        let mut out = std::io::stdout().lock();
        dispatch(root, json, cmd, &mut out).await
    })
}

/// Run the listing: resolve the identity, connect, call the client method, and render to `out`.
async fn dispatch(root: &Path, json: bool, cmd: WorkspaceCmd, out: &mut impl Write) -> Result<()> {
    match cmd {
        WorkspaceCmd::Workspaces { as_agent } => {
            let mut client = connect_as(root, as_agent).await?;
            let workspaces = client
                .list_workspaces()
                .await
                .map_err(|e| anyhow::anyhow!("list workspaces: {e}"))?;
            if json {
                let workspaces: Vec<WorkspaceDto> = workspaces.iter().map(WorkspaceDto::from).collect();
                render_response(
                    &WorkspacesResponse {
                        total: workspaces.len(),
                        workspaces,
                    },
                    out,
                )?;
            } else if workspaces.is_empty() {
                writeln!(out, "no workspaces")?;
            } else {
                for w in &workspaces {
                    writeln!(
                        out,
                        "{}\t{}\t{}\t{}",
                        w.key,
                        kind_label(w.kind),
                        w.root.display(),
                        w.repo_id.as_deref().unwrap_or("-")
                    )?;
                }
            }
        }
        WorkspaceCmd::Worktrees { repo_id, as_agent } => {
            let mut client = connect_as(root, as_agent).await?;
            let label = repo_id.clone();
            let worktrees = client
                .list_worktrees(repo_id)
                .await
                .map_err(|e| anyhow::anyhow!("list worktrees: {e}"))?;
            if json {
                let worktrees: Vec<WorktreeDto> = worktrees.iter().map(WorktreeDto::from).collect();
                render_response(
                    &WorktreesResponse {
                        repo_id: label,
                        total: worktrees.len(),
                        worktrees,
                    },
                    out,
                )?;
            } else if worktrees.is_empty() {
                writeln!(out, "no worktrees")?;
            } else {
                for w in &worktrees {
                    writeln!(
                        out,
                        "{}\t{}\t{}",
                        w.name,
                        w.path.display(),
                        w.branch.as_deref().unwrap_or("-")
                    )?;
                }
            }
        }
        WorkspaceCmd::Branches { repo_id, as_agent } => {
            let mut client = connect_as(root, as_agent).await?;
            let label = repo_id.clone();
            let branches = client
                .list_branches(repo_id)
                .await
                .map_err(|e| anyhow::anyhow!("list branches: {e}"))?;
            if json {
                let branches: Vec<BranchDto> = branches.iter().map(BranchDto::from).collect();
                render_response(
                    &BranchesResponse {
                        repo_id: label,
                        total: branches.len(),
                        branches,
                    },
                    out,
                )?;
            } else if branches.is_empty() {
                writeln!(out, "no branches")?;
            } else {
                for b in &branches {
                    writeln!(out, "{}\t{}", b.name, b.head_sha)?;
                }
            }
        }
    }
    Ok(())
}

/// Print a response type as pretty JSON, the same rendering the tool-backed subcommands use.
fn render_response(value: &impl serde::Serialize, out: &mut impl Write) -> Result<()> {
    render_json(&serde_json::to_value(value)?, out)
}

/// The wire spelling of a workspace kind, shared by the text and JSON renderings.
fn kind_label(kind: WorkspaceKind) -> &'static str {
    match kind {
        WorkspaceKind::Git => "git",
        WorkspaceKind::Plain => "plain",
    }
}

/// One workspace row in a `workspaces --json` response.
#[derive(Debug, serde::Serialize)]
struct WorkspaceDto {
    /// Stable workspace key (blake3 of the canonical root); also the cache-dir identity.
    key: String,
    /// `"git"` or `"plain"`.
    kind: &'static str,
    /// Canonical workspace root.
    root: String,
    /// Owning repo id, absent for a plain workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_id: Option<String>,
    /// Main-worktree root of the owning clone, absent for a plain workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    main_worktree: Option<String>,
    /// Unix micros of the last register/refresh.
    last_seen: i64,
}

impl From<&WorkspaceRecord> for WorkspaceDto {
    fn from(record: &WorkspaceRecord) -> Self {
        Self {
            key: record.key.clone(),
            kind: kind_label(record.kind),
            root: record.root.display().to_string(),
            repo_id: record.repo_id.clone(),
            main_worktree: record.main_worktree.as_ref().map(|p| p.display().to_string()),
            last_seen: record.last_seen,
        }
    }
}

/// `workspaces --json`.
#[derive(Debug, serde::Serialize)]
struct WorkspacesResponse {
    /// Number of workspaces returned.
    total: usize,
    /// The workspace rows, sorted by key.
    workspaces: Vec<WorkspaceDto>,
}

/// One worktree row in a `worktrees --json` response.
#[derive(Debug, serde::Serialize)]
struct WorktreeDto {
    /// Owning repo id.
    repo_id: String,
    /// `"(main)"` or the linked-worktree directory name.
    name: String,
    /// Absolute, canonical checkout root.
    path: String,
    /// Head commit sha, absent on an unborn HEAD.
    #[serde(skip_serializing_if = "Option::is_none")]
    head_sha: Option<String>,
    /// Checked-out branch, absent when detached or unresolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    /// True when HEAD is detached.
    detached: bool,
    /// Unix micros of the last refresh.
    last_seen: i64,
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
            last_seen: record.last_seen,
        }
    }
}

/// `worktrees --json`.
#[derive(Debug, serde::Serialize)]
struct WorktreesResponse {
    /// The repo id queried.
    repo_id: String,
    /// Number of worktrees returned.
    total: usize,
    /// The worktree rows, sorted by name.
    worktrees: Vec<WorktreeDto>,
}

/// One branch row in a `branches --json` response.
#[derive(Debug, serde::Serialize)]
struct BranchDto {
    /// Owning repo id.
    repo_id: String,
    /// Short branch name (`refs/heads/` stripped).
    name: String,
    /// 40-hex head commit sha.
    head_sha: String,
    /// Unix micros of the last refresh.
    last_seen: i64,
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

/// `branches --json`.
#[derive(Debug, serde::Serialize)]
struct BranchesResponse {
    /// The repo id queried.
    repo_id: String,
    /// Number of branches returned.
    total: usize,
    /// The branch rows, sorted by name.
    branches: Vec<BranchDto>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_json_carries_no_claim_field() {
        let record = WorktreeRecord {
            repo_id: "repo".to_string(),
            name: "(main)".to_string(),
            path: std::path::PathBuf::from("/tmp/repo"),
            head_sha: None,
            branch: Some("main".to_string()),
            detached: false,
            last_seen: 7,
        };
        let value = serde_json::to_value(WorktreeDto::from(&record)).expect("serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "repo_id": "repo",
                "name": "(main)",
                "path": "/tmp/repo",
                "branch": "main",
                "detached": false,
                "last_seen": 7
            })
        );
    }
}
