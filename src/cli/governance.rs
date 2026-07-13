//! Governance subcommands: `mine`, `proposals`, `accept`, `reject`.
//!
//! These dispatch through the in-process MCP server, same pattern as `cli/memory.rs`.
//! The tool shims compile regardless of the `memory` feature (returning a "feature not
//! enabled" MCP error); this CLI module therefore always compiles too.

use std::io::Write;

use anyhow::Result;
use clap::Subcommand;

use crate::mcp::BasemindServer;
use crate::mcp::params::*;

use super::render::{Emit, emit};
use super::run_tool;

#[derive(Subcommand, Debug)]
pub enum GovernanceCmd {
    /// Mine co-change skill proposals from recent git history.
    Mine {
        /// Number of recent commits to inspect (default 200, max 2000).
        #[arg(long)]
        window: Option<u32>,
        /// Minimum co-change count for a pair to be emitted (default 5).
        #[arg(long)]
        min_support: Option<u32>,
        /// Minimum confidence (support / anchor_freq) for a pair (default 0.6).
        #[arg(long)]
        min_confidence: Option<f32>,
        /// Skip commits touching more than N files (default 25).
        #[arg(long)]
        max_files_per_commit: Option<u32>,
    },
    /// List pending governance proposals.
    Proposals {
        /// Filter by kind: `skill` or `memory` (default: all).
        #[arg(long)]
        kind: Option<String>,
        /// Maximum results to return (default 100).
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Accept a proposal and promote it to a searchable skill memory.
    Accept {
        /// Proposal id (as returned by `proposals`).
        id: String,
        /// Override the auto-derived memory key.
        #[arg(long)]
        key: Option<String>,
    },
    /// Reject a proposal and suppress it from future mining runs.
    Reject {
        /// Proposal id (as returned by `proposals`).
        id: String,
        /// Optional human-readable reason (logged only, not persisted).
        #[arg(long)]
        reason: Option<String>,
    },
    /// Audit shared/individual memory records: recompute importance, archive stale entries,
    /// refresh `verified` verdicts (needs `--features memory`).
    Audit {
        /// Audit exactly this one key instead of the whole scope.
        #[arg(long)]
        key: Option<String>,
        /// Memory tier: `individual` (per-agent) instead of the default shared `group`.
        #[arg(long)]
        individual: bool,
        /// Compute verdicts but persist no mutations.
        #[arg(long)]
        dry_run: bool,
        /// Maximum records to audit (default 100, max 1000).
        #[arg(long)]
        limit: Option<u32>,
        /// Also scan the archived/stale `memory_archive` keyspace.
        #[arg(long)]
        include_archived: bool,
    },
}

pub async fn run(server: &BasemindServer, cmd: GovernanceCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    match cmd {
        GovernanceCmd::Mine {
            window,
            min_support,
            min_confidence,
            max_files_per_commit,
        } => {
            let p = ProposalsMineParams {
                window,
                min_support,
                min_confidence,
                max_files_per_commit,
            };
            let r = run_tool("proposals_mine", server.proposals_mine(Parameters(p)).await)?;
            emit("proposals_mine", &r, opts, out)
        }
        GovernanceCmd::Proposals { kind, limit } => {
            let p = ProposalsListParams {
                kind,
                limit,
                cursor: None,
            };
            let r = run_tool("proposals_list", server.proposals_list(Parameters(p)).await)?;
            emit("proposals_list", &r, opts, out)
        }
        GovernanceCmd::Accept { id, key } => {
            let p = ProposalAcceptParams { id, key };
            let r = run_tool("proposal_accept", server.proposal_accept(Parameters(p)).await)?;
            emit("proposal_accept", &r, opts, out)
        }
        GovernanceCmd::Reject { id, reason } => {
            let p = ProposalRejectParams { id, reason };
            let r = run_tool("proposal_reject", server.proposal_reject(Parameters(p)).await)?;
            emit("proposal_reject", &r, opts, out)
        }
        GovernanceCmd::Audit {
            key,
            individual,
            dry_run,
            limit,
            include_archived,
        } => {
            let p = MemoryAuditParams {
                key,
                visibility: if individual {
                    Visibility::Individual
                } else {
                    Visibility::Group
                },
                dry_run,
                limit,
                include_archived,
            };
            let r = run_tool("memory_audit", server.memory_audit(Parameters(p)).await)?;
            emit("memory_audit", &r, opts, out)
        }
    }
}
