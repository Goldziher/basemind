//! Fjall-side cores for the PROPOSAL governance operations, shared by the local serve path and the
//! daemon dispatch path (DRY).
//!
//! These functions own the `proposals` keyspace reads/writes and nothing else: no git-log mining, no
//! `audit_one_record` verdict, no LanceDB embed, no MCP response shaping. Those compute halves stay
//! serve-side (serve keeps its read-only store, blobs-backed `MapCache`, git, and LanceDB); only the
//! fjall reads/writes are forwarded to the daemon under `daemon_writer`. Both callers — the local
//! `run_proposal_*` helpers in [`super::helpers_proposals`] and the daemon's `on_governance` dispatch
//! — funnel through the same cores so the fjall behavior is identical in-process or in the daemon.
//!
//! Errors reuse [`MemoryOpError`](super::memory_ops::MemoryOpError) so they map cleanly onto both an
//! [`McpError`](rmcp::ErrorData) locally and a `CommsResponse::Error` in the daemon.

#![cfg(feature = "memory")]

use crate::index::IndexDb;
use crate::index::keys::{PROPOSAL_KIND_SKILL, PROPOSAL_KIND_TOMBSTONE, proposal_by_id};

use super::memory_ops::MemoryOpError;
use super::types_governance::ProposalRecord;
use super::types_memory::MemoryRecord;

/// One `(id, record)` pair as the list core yields it (and as it crosses the daemon wire).
pub(crate) type ProposalItem = (String, ProposalRecord);

/// The result of a `list_core` range scan.
pub(crate) struct ListResult {
    /// The page of `(id, record)` pairs.
    pub items: Vec<ProposalItem>,
    /// Whether the scan hit the limit / scan cap (more proposals remain).
    pub truncated: bool,
    /// Raw Fjall resume-key bytes for the next page, when more remain.
    pub next_cursor: Option<Vec<u8>>,
}

/// Range-scan core for `proposals_list`: iterate every requested kind namespace, decode each
/// proposal, and compute pagination. `cursor` is the raw last-seen key bytes from a previous page;
/// `next_cursor` is the raw last-emitted key bytes for the next page. Mirrors the loop that lived in
/// `run_proposals_list`.
pub(crate) fn list_core(
    idx: &IndexDb,
    scope: &str,
    kind_bytes: &[u8],
    limit: usize,
    scan_cap: usize,
    cursor: Option<&[u8]>,
) -> Result<ListResult, MemoryOpError> {
    use std::ops::Bound;

    use super::cursor::prefix_upper_bound;

    let mut items: Vec<ProposalItem> = Vec::new();
    let mut truncated = false;
    let mut last_key_bytes: Option<Vec<u8>> = None;
    let resume_key: Option<Vec<u8>> = cursor.map(<[u8]>::to_vec);

    'outer: for &kind_byte in kind_bytes {
        let prefix = crate::index::keys::proposal_ns_prefix(scope, kind_byte);
        let upper = prefix_upper_bound(&prefix);

        let lower_bound: Bound<Vec<u8>> = match &resume_key {
            Some(key) if key.starts_with(&prefix) => Bound::Excluded(key.clone()),
            _ => Bound::Included(prefix.clone()),
        };
        let upper_bound: Bound<Vec<u8>> = match upper {
            Some(u) => Bound::Excluded(u),
            None => Bound::Unbounded,
        };

        let iter = idx.proposals.range::<Vec<u8>, _>((lower_bound, upper_bound));
        for (scanned, guard) in iter.enumerate() {
            if scanned >= scan_cap {
                truncated = true;
                break 'outer;
            }
            if items.len() >= limit {
                truncated = true;
                break 'outer;
            }
            let (raw_key, raw_val) = guard
                .into_inner()
                .map_err(|source| MemoryOpError::Fjall { op: "iter", source })?;
            let Some((_, _, id)) = crate::index::keys::parse_proposal_by_id(&raw_key) else {
                continue;
            };
            let Ok(record) = rmp_serde::from_slice::<ProposalRecord>(&raw_val) else {
                continue;
            };
            last_key_bytes = Some(raw_key.to_vec());
            items.push((id, record));
        }
    }

    let next_cursor = if truncated { last_key_bytes } else { None };
    Ok(ListResult {
        items,
        truncated,
        next_cursor,
    })
}

/// Reject core for `proposal_reject`: remove the skill proposal and write a tombstone under
/// `PROPOSAL_KIND_TOMBSTONE` so re-mining will not resurface the same candidate.
pub(crate) fn reject_core(idx: &IndexDb, scope: &str, id: &str) -> Result<(), MemoryOpError> {
    let proposal_key = proposal_by_id(scope, PROPOSAL_KIND_SKILL, id);
    let tombstone_key = proposal_by_id(scope, PROPOSAL_KIND_TOMBSTONE, id);
    idx.proposals
        .remove(proposal_key)
        .map_err(|source| MemoryOpError::Fjall { op: "remove", source })?;
    idx.proposals
        .insert(tombstone_key, b"")
        .map_err(|source| MemoryOpError::Fjall { op: "insert", source })
}

/// Apply core for `proposals_mine`: for each freshly-mined `(id, record)` candidate, skip it when a
/// tombstone exists (rejected by a prior `proposal_reject`), else insert the proposal. Returns the
/// number actually written. The tombstone check + insert live here — not inline in the mining loop —
/// so the local serve path and the daemon path filter tombstones over one consistent fjall view.
pub(crate) fn apply_mine_core(idx: &IndexDb, scope: &str, candidates: &[ProposalItem]) -> Result<u32, MemoryOpError> {
    let mut mined: u32 = 0;
    for (id, record) in candidates {
        let tombstone_key = proposal_by_id(scope, PROPOSAL_KIND_TOMBSTONE, id);
        let has_tombstone = idx
            .proposals
            .get(&tombstone_key)
            .map_err(|source| MemoryOpError::Fjall { op: "get", source })?
            .is_some();
        if has_tombstone {
            continue;
        }
        let raw_key = proposal_by_id(scope, PROPOSAL_KIND_SKILL, id);
        let bytes = rmp_serde::to_vec_named(record).map_err(MemoryOpError::Serialize)?;
        idx.proposals
            .insert(raw_key, bytes)
            .map_err(|source| MemoryOpError::Fjall { op: "insert", source })?;
        mined += 1;
    }
    Ok(mined)
}

/// Read core for `proposal_accept`'s first step: fetch + decode the skill proposal by id, or `None`.
pub(crate) fn get_core(idx: &IndexDb, scope: &str, id: &str) -> Result<Option<ProposalRecord>, MemoryOpError> {
    let raw_key = proposal_by_id(scope, PROPOSAL_KIND_SKILL, id);
    let bytes = idx
        .proposals
        .get(raw_key)
        .map_err(|source| MemoryOpError::Fjall { op: "get", source })?;
    Ok(bytes.and_then(|b| rmp_serde::from_slice(&b).ok()))
}

/// Promote core for `proposal_accept`: write the (serve-audited) `record` into the live
/// `memory_by_key` keyspace, then remove the accepted proposal. The verdict + timestamps are stamped
/// serve-side before the record reaches here.
pub(crate) fn promote_core(
    idx: &IndexDb,
    scope: &str,
    memory_key: &str,
    record: &MemoryRecord,
    proposal_id: &str,
) -> Result<(), MemoryOpError> {
    let mem_key = crate::index::keys::memory_by_key(scope, crate::index::keys::MEMORY_VIS_GROUP, "", memory_key);
    let bytes = rmp_serde::to_vec_named(record).map_err(MemoryOpError::Serialize)?;
    idx.memory_by_key
        .insert(mem_key, bytes)
        .map_err(|source| MemoryOpError::Fjall { op: "insert", source })?;
    let raw_key = proposal_by_id(scope, PROPOSAL_KIND_SKILL, proposal_id);
    idx.proposals
        .remove(raw_key)
        .map_err(|source| MemoryOpError::Fjall { op: "remove", source })
}

/// Dispatch a wire [`GovernanceOp`](crate::comms::proposals_proto::GovernanceOp) against a
/// workspace's read-write index, returning the wire outcome. This is the entry point the daemon
/// calls; the local serve path calls the per-op cores directly so it can interleave the git + audit +
/// LanceDB halves without a second match. Gated on `comms` because the wire enums live in
/// `comms::proposals_proto`.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) fn run_governance_op(
    idx: &IndexDb,
    scope: &str,
    op: &crate::comms::proposals_proto::GovernanceOp,
) -> Result<crate::comms::proposals_proto::GovernanceOutcome, MemoryOpError> {
    use crate::comms::proposals_proto::{GovernanceOp, GovernanceOutcome};

    match op {
        GovernanceOp::ProposalsList {
            kind_bytes,
            limit,
            scan_cap,
            cursor,
        } => {
            let result = list_core(
                idx,
                scope,
                kind_bytes,
                *limit as usize,
                *scan_cap as usize,
                cursor.as_deref(),
            )?;
            Ok(GovernanceOutcome::ProposalsListed {
                items: result.items,
                truncated: result.truncated,
                next_cursor: result.next_cursor,
            })
        }
        GovernanceOp::ProposalReject { id } => {
            reject_core(idx, scope, id)?;
            Ok(GovernanceOutcome::Rejected)
        }
        GovernanceOp::ProposalsMineApply { candidates } => {
            let count = apply_mine_core(idx, scope, candidates)?;
            Ok(GovernanceOutcome::Mined { count })
        }
        GovernanceOp::ProposalGet { id } => Ok(GovernanceOutcome::Proposal(get_core(idx, scope, id)?)),
        GovernanceOp::ProposalPromote {
            proposal_id,
            memory_key,
            record,
        } => {
            promote_core(idx, scope, memory_key, record, proposal_id)?;
            Ok(GovernanceOutcome::Promoted)
        }
    }
}
