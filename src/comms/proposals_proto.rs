//! Wire types for forwarding the PROPOSAL governance operations (`proposals_list` /
//! `proposal_reject` / `proposals_mine` / `proposal_accept`) from a `daemon_writer` serve to the
//! machine daemon.
//!
//! Under Seam B a comms-build `serve` opens its store read-only (no fjall index), so the daemon —
//! the machine's sole fjall writer — owns the `proposals` keyspace. The rule is the same as the
//! CORE memory ops in [`super::memory_proto`]: **daemon = fjall only; compute stays serve-side.**
//! Serve keeps its read-only store, the blobs-backed `MapCache`, git access, and LanceDB, so it does
//! the git-log co-change MINING (`proposals_mine`) and the `audit_one_record` verdict + LanceDB embed
//! (`proposal_accept`) locally, and forwards only the fjall reads/writes over the socket.
//!
//! [`ProposalRecord`] and [`MemoryRecord`] are reused verbatim on the wire (rather than parallel
//! structs). Both carry git-derived `f32` fields (`confidence` / `importance`), so — unlike the
//! memory proto — these enums are NOT `Eq`, only `PartialEq`. That is why [`super::protocol`]'s
//! `CommsRequest` / `CommsResponse` drop their `Eq` bound.

#![cfg(all(feature = "comms", feature = "memory"))]

use serde::{Deserialize, Serialize};

use crate::mcp::types_governance::ProposalRecord;
use crate::mcp::types_memory::MemoryRecord;

/// A PROPOSAL governance operation forwarded to the daemon. The scope is resolved serve-side; the
/// daemon runs the op against the workspace's read-write `proposals` (and, for a promote, its
/// `memory_by_key`) index.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum GovernanceOp {
    /// Range-scan pending proposals for the resolved kind byte(s), paginated. Returns
    /// [`GovernanceOutcome::ProposalsListed`].
    ProposalsList {
        /// The proposal kind byte(s) to scan (skill / memory), resolved serve-side.
        kind_bytes: Vec<u8>,
        /// Page size.
        limit: u32,
        /// Per-kind scan cap bounding work on a large keyspace.
        scan_cap: u32,
        /// Raw Fjall resume-key bytes from a previous page's `next_cursor`.
        cursor: Option<Vec<u8>>,
    },
    /// Delete a proposal by id and write a tombstone so re-mining will not resurface it. Returns
    /// [`GovernanceOutcome::Rejected`].
    ProposalReject {
        /// The proposal id (hex blake3 of the sorted file-set).
        id: String,
    },
    /// Apply a batch of freshly-mined candidates: for each, insert the proposal UNLESS a tombstone
    /// exists. The tombstone check + insert are done daemon-side (the sole fjall writer) so the
    /// filter and write are one consistent view. Returns [`GovernanceOutcome::Mined`].
    ProposalsMineApply {
        /// `(id, record)` pairs — every mined candidate; the daemon filters tombstoned ids.
        candidates: Vec<(String, ProposalRecord)>,
    },
    /// Read one proposal by id. Returns [`GovernanceOutcome::Proposal`].
    ProposalGet {
        /// The proposal id.
        id: String,
    },
    /// Promote an accepted proposal: write the (serve-audited) memory record into the live
    /// `memory_by_key` keyspace and remove the proposal. Returns [`GovernanceOutcome::Promoted`].
    ProposalPromote {
        /// The proposal id to remove after promotion.
        proposal_id: String,
        /// The live memory key to write the promoted record under.
        memory_key: String,
        /// The fully-stamped memory record (verdict + timestamps applied serve-side).
        record: MemoryRecord,
    },
    /// Scan one memory keyspace (live or archive) for `memory_audit`, returning the raw records so
    /// serve can run the (cache + store-backed) audit verdict locally. The daemon does no compute —
    /// it only reads fjall. Returns [`GovernanceOutcome::AuditScanned`]. A single-keyspace scan; the
    /// live-then-archive orchestration (and the shared `limit`) lives serve-side.
    AuditScan {
        /// Visibility byte (group / individual), resolved serve-side.
        vis_byte: u8,
        /// Owner segment (`""` for group, the agent id for individual).
        owner: String,
        /// A specific key to fetch, or `None` to prefix-scan the whole `(scope, vis, owner)` range.
        key: Option<String>,
        /// Read from `memory_archive` when true, else `memory_by_key`.
        from_archive: bool,
        /// Max records to return.
        limit: u32,
        /// Scan cap bounding work on a large keyspace.
        scan_cap: u32,
    },
    /// Persist a batch of serve-computed audit verdicts. Each mutation either rewrites the live
    /// record or archives it (write archive + delete live). Returns [`GovernanceOutcome::AuditPersisted`].
    AuditPersist {
        /// The verdict-driven writes to apply, in order.
        mutations: Vec<AuditMutation>,
    },
}

/// One serve-computed audit verdict to persist daemon-side. Float-bearing (`MemoryRecord` carries an
/// `f32` importance), which is why the governance wire enums are `PartialEq` but not `Eq`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditMutation {
    /// Visibility byte the record lives under.
    pub vis_byte: u8,
    /// Owner segment.
    pub owner: String,
    /// The memory key.
    pub key: String,
    /// The mutated record (verdict + timestamps + decay applied serve-side).
    pub record: MemoryRecord,
    /// When true, move the record to `memory_archive` (write archive + delete live) instead of
    /// rewriting it live.
    pub archive: bool,
}

/// The daemon's reply to a [`GovernanceOp`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum GovernanceOutcome {
    /// Reply to [`GovernanceOp::ProposalsList`]: a page of `(id, record)` plus pagination metadata.
    ProposalsListed {
        /// The page of proposals, each with its id.
        items: Vec<(String, ProposalRecord)>,
        /// Whether the scan hit the limit / scan cap (more proposals remain).
        truncated: bool,
        /// Raw Fjall resume-key bytes for the next page, when more remain.
        next_cursor: Option<Vec<u8>>,
    },
    /// Reply to [`GovernanceOp::ProposalReject`]: the proposal was removed + tombstoned.
    Rejected,
    /// Reply to [`GovernanceOp::ProposalsMineApply`]: the number of candidates actually written
    /// (tombstoned ids are skipped, so this may be less than the number sent).
    Mined {
        /// Count of proposals written this apply.
        count: u32,
    },
    /// Reply to [`GovernanceOp::ProposalGet`]: the proposal, or `None` when absent.
    Proposal(Option<ProposalRecord>),
    /// Reply to [`GovernanceOp::ProposalPromote`]: the memory was written + the proposal removed.
    Promoted,
    /// Reply to [`GovernanceOp::AuditScan`]: the raw `(key, msgpack-bytes)` records to audit. Bytes
    /// (not decoded records) so serve's `evaluate_one` stays the single decode + verdict site.
    AuditScanned {
        /// One `(key, raw msgpack value)` per scanned record.
        items: Vec<(String, Vec<u8>)>,
    },
    /// Reply to [`GovernanceOp::AuditPersist`]: the verdict mutations were applied.
    AuditPersisted,
}
