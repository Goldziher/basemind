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

/// Bundled scan parameters for [`audit_scan_core`] (and the serve-side `scan_audit_keyspace`
/// forwarder), keeping both call sites under clippy's argument-count limit. Borrowed so it costs
/// nothing to thread through.
pub(crate) struct AuditScanArgs<'a> {
    /// Visibility byte (group / individual).
    pub vis_byte: u8,
    /// Owner segment (`""` for group).
    pub owner: &'a str,
    /// A specific key to fetch, or `None` to prefix-scan the whole `(scope, vis, owner)` range.
    pub key: Option<&'a str>,
    /// Read from `memory_archive` when true, else `memory_by_key`.
    pub from_archive: bool,
    /// Max records to return.
    pub limit: usize,
    /// Scan cap bounding work on a large keyspace.
    pub scan_cap: usize,
}

/// Scan core for `memory_audit`: read one memory keyspace (live or archive) and return the raw
/// `(key, msgpack-value)` records so serve can run the cache + store-backed audit verdict locally.
/// A single-keyspace scan — the live-then-archive orchestration and the shared record limit stay
/// serve-side. `key = Some` fetches one record; `key = None` prefix-scans the `(scope, vis, owner)`
/// range up to `limit` / `scan_cap`. Mirrors the fjall reads in `run_memory_audit`.
pub(crate) fn audit_scan_core(
    idx: &IndexDb,
    scope: &str,
    args: &AuditScanArgs<'_>,
) -> Result<Vec<(String, Vec<u8>)>, MemoryOpError> {
    use crate::index::keys::{memory_by_key, memory_by_key_ns_prefix, parse_memory_key_only};

    let keyspace = if args.from_archive {
        &idx.memory_archive
    } else {
        &idx.memory_by_key
    };

    if let Some(single_key) = args.key {
        let raw_key = memory_by_key(scope, args.vis_byte, args.owner, single_key);
        let value = keyspace
            .get(raw_key)
            .map_err(|source| MemoryOpError::Fjall { op: "get", source })?;
        return Ok(value
            .map(|v| vec![(single_key.to_string(), v.to_vec())])
            .unwrap_or_default());
    }

    let ns_prefix = memory_by_key_ns_prefix(scope, args.vis_byte, args.owner);
    let mut items: Vec<(String, Vec<u8>)> = Vec::new();
    for (scanned, guard) in keyspace.prefix(&ns_prefix).enumerate() {
        if items.len() >= args.limit || scanned >= args.scan_cap {
            break;
        }
        let (raw_key_bytes, raw_val) = guard
            .into_inner()
            .map_err(|source| MemoryOpError::Fjall { op: "iter", source })?;
        let Some(parsed_key) = parse_memory_key_only(&raw_key_bytes) else {
            continue;
        };
        items.push((parsed_key.to_string(), raw_val.to_vec()));
    }
    Ok(items)
}

/// Persist core for `memory_audit`: apply serve-computed verdicts. Each mutation either rewrites the
/// live record (`archive = false`) or moves it to `memory_archive` (write archive + delete live).
/// Inlines the same fjall writes as `helpers_governance::{write_live, write_archive, delete_live}`
/// (in `MemoryOpError` terms), matching the `promote_core` precedent of not crossing the McpError
/// boundary from the daemon path.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) fn audit_persist_core(
    idx: &IndexDb,
    scope: &str,
    mutations: &[crate::comms::proposals_proto::AuditMutation],
) -> Result<(), MemoryOpError> {
    use crate::index::keys::memory_by_key;

    for mutation in mutations {
        let raw_key = memory_by_key(scope, mutation.vis_byte, &mutation.owner, &mutation.key);
        let bytes = rmp_serde::to_vec_named(&mutation.record).map_err(MemoryOpError::Serialize)?;
        if mutation.archive {
            idx.memory_archive
                .insert(&raw_key, bytes)
                .map_err(|source| MemoryOpError::Fjall { op: "insert", source })?;
            idx.memory_by_key
                .remove(&raw_key)
                .map_err(|source| MemoryOpError::Fjall { op: "remove", source })?;
        } else {
            idx.memory_by_key
                .insert(&raw_key, bytes)
                .map_err(|source| MemoryOpError::Fjall { op: "insert", source })?;
        }
    }
    Ok(())
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
        GovernanceOp::AuditScan {
            vis_byte,
            owner,
            key,
            from_archive,
            limit,
            scan_cap,
        } => {
            let args = AuditScanArgs {
                vis_byte: *vis_byte,
                owner,
                key: key.as_deref(),
                from_archive: *from_archive,
                limit: *limit as usize,
                scan_cap: *scan_cap as usize,
            };
            let items = audit_scan_core(idx, scope, &args)?;
            Ok(GovernanceOutcome::AuditScanned { items })
        }
        GovernanceOp::AuditPersist { mutations } => {
            audit_persist_core(idx, scope, mutations)?;
            Ok(GovernanceOutcome::AuditPersisted)
        }
    }
}
