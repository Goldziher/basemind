//! `basemind workspace` — the CLI half of the `workspace` domain.
//!
//! Real clap subcommands rather than a `--mode` flag, so each operation keeps its own `--help` and
//! its own argument validation; they map one-to-one onto the MCP `workspace` tool's
//! [`WorkspaceMode`](crate::mcp::mode::WorkspaceMode) values, which is what `tests/cli_parity.rs`
//! asserts.
//!
//! The registry lives in the always-on comms broker daemon (its sole writer). Like the
//! [`agents`](super::agents) group — and unlike the code-map / memory CLI groups — these
//! subcommands connect to the daemon DIRECTLY via [`CommsClient::ensure_and_connect`] rather than
//! building a full [`BasemindServer`](crate::mcp::BasemindServer), so they take no repo index lock
//! and cannot clash with a running `basemind serve`.
//!
//! `--json` emits the structured response for every mode. Worktree claims are ADVISORY: they record
//! intent in the registry but enforce nothing.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::cli::render::render_json;
use crate::comms::client::{CommsClient, scope_context_for};
use crate::comms::ids::AgentId;
use crate::mcp::types_registry::{
    BranchDto, BranchesResponse, WorkspaceDto, WorkspacesResponse, WorktreeClaimResponse, WorktreeDto,
    WorktreesResponse,
};

/// The `workspace` domain's CLI subcommands, one per MCP mode; they talk to the broker daemon
/// directly.
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
    /// Advisory-claim a worktree for this agent (a coordination hint; enforces nothing).
    Claim {
        /// The owning repo id.
        repo_id: String,
        /// The worktree name (`(main)` or the linked-worktree directory name).
        name: String,
        /// Claim as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// Release YOUR advisory claim on a worktree.
    Release {
        /// The owning repo id.
        repo_id: String,
        /// The worktree name whose claim to release.
        name: String,
        /// Release as this sub-identity instead of the CLI's default agent id.
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

/// Run the mode: resolve the identity, connect, call the client method, and render to `out`.
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
                    let kind = if w.kind == crate::registry::WorkspaceKind::Git {
                        "git"
                    } else {
                        "plain"
                    };
                    writeln!(
                        out,
                        "{}\t{}\t{}\t{}",
                        w.key,
                        kind,
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
                        "{}\t{}\t{}\t{}",
                        w.name,
                        w.path.display(),
                        w.branch.as_deref().unwrap_or("-"),
                        w.claimed_by.as_deref().unwrap_or("-")
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
        WorkspaceCmd::Claim {
            repo_id,
            name,
            as_agent,
        } => {
            let mut client = connect_as(root, as_agent).await?;
            let claimant = client.agent().as_str().to_string();
            let (repo_label, name_label) = (repo_id.clone(), name.clone());
            let held = client
                .claim_worktree(repo_id, name, claimant.clone())
                .await
                .map_err(|e| anyhow::anyhow!("claim worktree: {e}"))?;
            render_claim(json, out, &repo_label, &name_label, &claimant, held, "claimed")?;
        }
        WorkspaceCmd::Release {
            repo_id,
            name,
            as_agent,
        } => {
            let mut client = connect_as(root, as_agent).await?;
            let claimant = client.agent().as_str().to_string();
            let (repo_label, name_label) = (repo_id.clone(), name.clone());
            let held = client
                .release_worktree(repo_id, name, claimant.clone())
                .await
                .map_err(|e| anyhow::anyhow!("release worktree: {e}"))?;
            render_claim(json, out, &repo_label, &name_label, &claimant, held, "released")?;
        }
    }
    Ok(())
}

/// Print a response type as pretty JSON, the same rendering the tool-backed subcommands use.
fn render_response(value: &impl serde::Serialize, out: &mut impl Write) -> Result<()> {
    render_json(&serde_json::to_value(value)?, out)
}

/// Render a claim / release outcome, honoring `--json`. `held` reflects whether the claim is now
/// held (claim) or was cleared (release) by `claimant`; `verb` is the human label.
#[allow(clippy::too_many_arguments)]
fn render_claim(
    json: bool,
    out: &mut impl Write,
    repo_id: &str,
    name: &str,
    claimant: &str,
    held: bool,
    verb: &str,
) -> Result<()> {
    if json {
        render_response(
            &WorktreeClaimResponse {
                repo_id: repo_id.to_string(),
                name: name.to_string(),
                claimant: claimant.to_string(),
                held,
            },
            out,
        )?;
    } else if held {
        writeln!(out, "{verb} {name} in {repo_id} (as {claimant})")?;
    } else {
        writeln!(out, "not {verb}: {name} in {repo_id} (held by another or unknown)")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_json_matches_the_mcp_response_and_is_pretty() {
        let mut buf = Vec::new();
        render_claim(true, &mut buf, "repo", "wt", "alice", true, "claimed").expect("render");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.contains("\n  \"held\": true"), "uniform pretty JSON: {text}");
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(
            value,
            serde_json::json!({"repo_id": "repo", "name": "wt", "claimant": "alice", "held": true})
        );
    }
}
