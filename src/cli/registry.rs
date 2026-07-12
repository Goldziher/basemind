//! Machine-registry CLI verbs (`basemind registry <verb>`).
//!
//! The registry lives in the always-on comms broker daemon (its sole writer). Like the
//! [`comms`](super::comms) agent verbs — and unlike the code-map / memory CLI groups — these verbs
//! connect to the daemon DIRECTLY via [`CommsClient::ensure_and_connect`] rather than building a
//! full [`BasemindServer`](crate::mcp::BasemindServer), so they take no repo index lock and cannot
//! clash with a running `basemind serve`.
//!
//! This is the human-admin + parity path for the machine registry MCP tools (`workspaces` /
//! `worktrees` / `branches` / `worktree_claim` / `worktree_release`). `--json` emits the structured
//! response for every verb. Worktree claims are ADVISORY: they record intent in the registry but
//! enforce nothing.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::json;

use crate::comms::client::{CommsClient, scope_context_for};
use crate::comms::ids::AgentId;

/// Machine-registry verbs that talk to the broker daemon directly.
#[derive(Subcommand, Debug)]
pub enum RegistryCmd {
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

/// Resolve the CLI agent identity, tiered to MATCH the `serve` / `comms` resolver so CLI-driven
/// registry calls share the session's identity.
fn cli_agent_id(root: &Path) -> Result<AgentId> {
    if let Ok(raw) = std::env::var("BASEMIND_AGENT_ID")
        && let Ok(id) = AgentId::parse(raw)
    {
        return Ok(id);
    }
    if let Ok(existing) = std::fs::read_to_string(root.join(".basemind").join("agent-id"))
        && let Ok(id) = AgentId::parse(existing.trim())
    {
        return Ok(id);
    }
    AgentId::parse("basemind-cli").context("construct CLI agent id")
}

/// Connect a [`CommsClient`] to the broker as a resolved identity.
async fn connect_as(root: &Path, as_agent: Option<String>) -> Result<CommsClient> {
    let agent = match as_agent {
        Some(raw) => AgentId::parse(raw.clone()).with_context(|| format!("invalid --as-agent {raw:?}"))?,
        None => cli_agent_id(root)?,
    };
    let (remote, cwd) = scope_context_for(root);
    CommsClient::ensure_and_connect(agent, remote, cwd)
        .await
        .map_err(|e| anyhow::anyhow!("connect to comms daemon: {e}"))
}

/// Dispatch one registry verb. Builds a small current-thread runtime, then runs the verb.
pub fn run(root: &Path, json: bool, cmd: RegistryCmd) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(async move {
        let mut out = std::io::stdout().lock();
        dispatch(root, json, cmd, &mut out).await
    })
}

/// Run the verb: resolve the identity, connect, call the client method, and render to `out`.
async fn dispatch(root: &Path, json: bool, cmd: RegistryCmd, out: &mut impl Write) -> Result<()> {
    match cmd {
        RegistryCmd::Workspaces { as_agent } => {
            let mut client = connect_as(root, as_agent).await?;
            let workspaces = client
                .list_workspaces()
                .await
                .map_err(|e| anyhow::anyhow!("list workspaces: {e}"))?;
            if json {
                let rows: Vec<_> = workspaces
                    .iter()
                    .map(|w| {
                        json!({
                            "key": w.key,
                            "kind": if w.kind == crate::registry::WorkspaceKind::Git { "git" } else { "plain" },
                            "root": w.root,
                            "repo_id": w.repo_id,
                            "main_worktree": w.main_worktree,
                            "last_seen": w.last_seen,
                        })
                    })
                    .collect();
                writeln!(out, "{}", json!({ "total": rows.len(), "workspaces": rows }))?;
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
        RegistryCmd::Worktrees { repo_id, as_agent } => {
            let mut client = connect_as(root, as_agent).await?;
            let label = repo_id.clone();
            let worktrees = client
                .list_worktrees(repo_id)
                .await
                .map_err(|e| anyhow::anyhow!("list worktrees: {e}"))?;
            if json {
                let rows: Vec<_> = worktrees
                    .iter()
                    .map(|w| {
                        json!({
                            "repo_id": w.repo_id,
                            "name": w.name,
                            "path": w.path,
                            "head_sha": w.head_sha,
                            "branch": w.branch,
                            "detached": w.detached,
                            "claimed_by": w.claimed_by,
                            "last_seen": w.last_seen,
                        })
                    })
                    .collect();
                writeln!(
                    out,
                    "{}",
                    json!({ "repo_id": label, "total": rows.len(), "worktrees": rows })
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
        RegistryCmd::Branches { repo_id, as_agent } => {
            let mut client = connect_as(root, as_agent).await?;
            let label = repo_id.clone();
            let branches = client
                .list_branches(repo_id)
                .await
                .map_err(|e| anyhow::anyhow!("list branches: {e}"))?;
            if json {
                let rows: Vec<_> = branches
                    .iter()
                    .map(|b| {
                        json!({
                            "repo_id": b.repo_id,
                            "name": b.name,
                            "head_sha": b.head_sha,
                            "last_seen": b.last_seen,
                        })
                    })
                    .collect();
                writeln!(
                    out,
                    "{}",
                    json!({ "repo_id": label, "total": rows.len(), "branches": rows })
                )?;
            } else if branches.is_empty() {
                writeln!(out, "no branches")?;
            } else {
                for b in &branches {
                    writeln!(out, "{}\t{}", b.name, b.head_sha)?;
                }
            }
        }
        RegistryCmd::Claim {
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
        RegistryCmd::Release {
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

/// Render a claim / release outcome, honoring `--json`. `held` reflects whether the claim is now
/// held (claim) or was cleared (release) by `claimant`; `verb` is the human label.
// Flat render helper: json + out + the four outcome fields + verb. Bundling these into a struct
// would add ceremony without clarifying anything, so the arg count is acceptable here.
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
        writeln!(
            out,
            "{}",
            json!({ "repo_id": repo_id, "name": name, "claimant": claimant, "held": held })
        )?;
    } else if held {
        writeln!(out, "{verb} {name} in {repo_id} (as {claimant})")?;
    } else {
        writeln!(out, "not {verb}: {name} in {repo_id} (held by another or unknown)")?;
    }
    Ok(())
}
