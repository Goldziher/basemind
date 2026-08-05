//! `basemind memory` — the CLI half of the `memory` domain.
//!
//! Real clap subcommands rather than a `--mode` flag, so each operation keeps its own `--help` and
//! its own argument validation; they map one-to-one onto the MCP `memory` tool's [`MemoryMode`]
//! values, which is what `tests/cli_parity.rs` asserts.
//!
//! The MCP tool is registered in every build and gates per mode, so these handlers always compile
//! and dispatch identically — a mode whose feature is missing surfaces the tool's own
//! "rebuild with --features …" error.

use std::io::Write;

use anyhow::Result;
use clap::Subcommand;

use crate::mcp::BasemindServer;
use crate::mcp::mode::MemoryMode;
use crate::mcp::params::{Lenient, Parameters};
use crate::mcp::types_memory::{MemoryParams, Visibility};

use super::render::{Emit, emit};
use super::run_tool;

/// Map the `--individual` flag onto a [`Visibility`]. Absent leaves the field unset, which the
/// tool reads as the shared `group` tier — passing it explicitly would trip the per-mode field
/// check on the modes that have no tier.
fn visibility(individual: bool) -> Option<Visibility> {
    individual.then_some(Visibility::Individual)
}

#[derive(Subcommand, Debug)]
pub enum MemoryCmd {
    /// Persist a key-value pair in scoped memory.
    Put {
        key: String,
        value: String,
        #[arg(long)]
        tag: Vec<String>,
        /// Disable embedding into LanceDB (skips `memory search` indexing).
        #[arg(long)]
        no_embed: bool,
        /// Use the per-agent (individual) memory tier instead of shared (group).
        #[arg(long)]
        individual: bool,
    },
    /// Exact-key lookup.
    Get {
        key: String,
        /// Look up in the per-agent (individual) tier instead of shared (group).
        #[arg(long)]
        individual: bool,
    },
    /// List scoped memory entries.
    List {
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        /// List the per-agent (individual) tier instead of shared (group).
        #[arg(long)]
        individual: bool,
    },
    /// Vector KNN search over stored memory.
    Search {
        query: String,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        tag: Option<String>,
        /// Search the per-agent (individual) tier instead of shared (group).
        #[arg(long)]
        individual: bool,
    },
    /// Delete a memory entry by exact key.
    Delete {
        key: String,
        /// Delete from the per-agent (individual) tier instead of shared (group).
        #[arg(long)]
        individual: bool,
    },
    /// Audit stored memories against the live index: refresh verdicts, decay importance,
    /// archive long-stale records.
    Audit {
        /// Audit exactly this one key instead of the whole scope.
        #[arg(long)]
        key: Option<String>,
        /// Audit the per-agent (individual) tier instead of shared (group).
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
    /// Semantic search over indexed document chunks (PDF / Office / HTML / email / OCR).
    Documents {
        query: String,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        mime_type: Option<String>,
        /// Ingestion scope to search. Defaults to this repo's. Scraped pages live under
        /// `web:<host>`.
        #[arg(long)]
        scope: Option<String>,
    },
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
        /// Proposal id (as returned by `memory proposals`).
        id: String,
        /// Override the auto-derived memory key.
        #[arg(long)]
        key: Option<String>,
    },
    /// Reject a proposal and suppress it from future mining runs.
    Reject {
        /// Proposal id (as returned by `memory proposals`).
        id: String,
        /// Optional human-readable reason (logged only, not persisted).
        #[arg(long)]
        reason: Option<String>,
    },
}

/// Every field the `memory` tool accepts, with the ones this mode does not use left `None` — the
/// helper rejects a field that belongs to another mode, so they must not be populated blindly.
fn params(mode: MemoryMode) -> MemoryParams {
    MemoryParams {
        mode,
        key: None,
        value: None,
        tags: None,
        embed: None,
        visibility: None,
        prefix: None,
        tag: None,
        limit: None,
        cursor: None,
        query: None,
        dry_run: None,
        include_archived: None,
        max_tokens: None,
        format: None,
        mime_type: None,
        scope: None,
        entity_category: None,
        keywords_contains: None,
        window: None,
        min_support: None,
        min_confidence: None,
        max_files_per_commit: None,
        kind: None,
        id: None,
        reason: None,
        overrides: Default::default(),
    }
}

pub async fn run(server: &BasemindServer, cmd: MemoryCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let p = match cmd {
        MemoryCmd::Put {
            key,
            value,
            tag,
            no_embed,
            individual,
        } => MemoryParams {
            key: Some(key),
            value: Some(value),
            tags: (!tag.is_empty()).then_some(tag),
            embed: no_embed.then_some(false),
            visibility: visibility(individual),
            ..params(MemoryMode::Put)
        },
        MemoryCmd::Get { key, individual } => MemoryParams {
            key: Some(key),
            visibility: visibility(individual),
            ..params(MemoryMode::Get)
        },
        MemoryCmd::List {
            prefix,
            tag,
            limit,
            individual,
        } => MemoryParams {
            prefix,
            tag,
            limit,
            visibility: visibility(individual),
            ..params(MemoryMode::List)
        },
        MemoryCmd::Search {
            query,
            limit,
            tag,
            individual,
        } => MemoryParams {
            query: Some(query),
            limit,
            tag,
            visibility: visibility(individual),
            ..params(MemoryMode::Search)
        },
        MemoryCmd::Delete { key, individual } => MemoryParams {
            key: Some(key),
            visibility: visibility(individual),
            ..params(MemoryMode::Delete)
        },
        MemoryCmd::Audit {
            key,
            individual,
            dry_run,
            limit,
            include_archived,
        } => MemoryParams {
            key,
            visibility: visibility(individual),
            dry_run: dry_run.then_some(true),
            limit,
            include_archived: include_archived.then_some(true),
            ..params(MemoryMode::Audit)
        },
        MemoryCmd::Documents {
            query,
            limit,
            mime_type,
            scope,
        } => MemoryParams {
            query: Some(query),
            limit,
            mime_type,
            scope,
            ..params(MemoryMode::Documents)
        },
        MemoryCmd::Mine {
            window,
            min_support,
            min_confidence,
            max_files_per_commit,
        } => MemoryParams {
            window,
            min_support,
            min_confidence,
            max_files_per_commit,
            ..params(MemoryMode::Mine)
        },
        MemoryCmd::Proposals { kind, limit } => MemoryParams {
            kind,
            limit,
            ..params(MemoryMode::Proposals)
        },
        MemoryCmd::Accept { id, key } => MemoryParams {
            id: Some(id),
            key,
            ..params(MemoryMode::Accept)
        },
        MemoryCmd::Reject { id, reason } => MemoryParams {
            id: Some(id),
            reason,
            ..params(MemoryMode::Reject)
        },
    };

    let key = p.mode.telemetry_key();
    let r = run_tool(key, server.memory(Parameters(Lenient(p))).await)?;
    emit(key, &r, opts, out)
}
