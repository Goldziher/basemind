//! Fjall-side cores for the CORE memory operations, shared by the local serve path and the daemon
//! dispatch path (DRY).
//!
//! These functions own the `memory_by_key` read-modify-write / range-scan logic and nothing else:
//! no LanceDB, no embedder, no MCP response shaping. Both callers — the local `run_memory_*` helpers
//! in [`super::memory`] and the daemon's `on_memory` dispatch — feed the same [`MemoryOp`] through
//! [`run_memory_op`] so the fjall behavior is identical whether the write happens in-process or in
//! the machine daemon.
//!
//! The put core is a bare read-then-write: it does NOT take a per-key lock. Serialization is the
//! caller's job — the local path holds `memory_put_lock`, and the daemon path is serialized by the
//! workspace pool's per-workspace store `Mutex` (one writer per workspace), which makes the RMW
//! atomic. That is why this module carries no synchronization of its own.

#![cfg(feature = "memory")]

use serde::{Deserialize, Serialize};

use crate::index::IndexDb;

use super::types_memory::{MemoryRecord, Provenance, VerifyState};

/// One memory entry as the cores yield it (and as it crosses the daemon wire): the durable record
/// fields, with `value` already preview-truncated for `List` results (never truncated for `Get`).
///
/// Defined here — rather than in `comms::memory_proto` — so the cores compile on a `memory`-only
/// build (no `comms`). `comms::memory_proto` re-exports this type so the wire protocol reuses the
/// exact same shape. Float-free, so `Eq` is derivable and the protocol enums keep their `Eq` bound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireMemoryEntry {
    /// The memory key.
    pub key: String,
    /// The value (preview-truncated for `List`).
    pub value: String,
    /// The record's tags.
    pub tags: Vec<String>,
    /// Micros when the record was first created.
    pub created_at: i64,
    /// Micros of the last write.
    pub updated_at: i64,
}

/// Maximum characters of a value surfaced in a `List` entry preview before it is truncated with an
/// ellipsis. Mirrors the constant the MCP `memory_list` tool used before the extraction.
pub(crate) const MEMORY_PREVIEW_CHARS: usize = 200;

/// A fjall-side memory operation failed. Kept small and stringly so it maps cleanly to both an
/// [`McpError`](rmcp::ErrorData) on the local path and a `CommsResponse::Error` on the daemon path.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MemoryOpError {
    /// The read-write `memory_by_key` index was not available (should never happen daemon-side).
    #[error("memory_by_key index not available")]
    IndexUnavailable,
    /// A fjall get / insert / remove failed.
    #[error("fjall {op}: {source}")]
    Fjall {
        /// The fjall verb that failed (`get` / `insert` / `remove` / `iter`).
        op: &'static str,
        /// The underlying fjall error.
        source: fjall::Error,
    },
    /// Serializing a [`MemoryRecord`] to msgpack failed.
    #[error("serialize memory record: {0}")]
    Serialize(rmp_serde::encode::Error),
}

impl From<MemoryOpError> for rmcp::ErrorData {
    fn from(error: MemoryOpError) -> Self {
        rmcp::ErrorData::internal_error(error.to_string(), None)
    }
}

/// Read the raw record for `(scope, vis_byte, owner, key)`, or `None` when absent / undecodable.
fn read_record(
    idx: &IndexDb,
    scope: &str,
    vis_byte: u8,
    owner: &str,
    key: &str,
) -> Result<Option<MemoryRecord>, MemoryOpError> {
    let raw_key = crate::index::keys::memory_by_key(scope, vis_byte, owner, key);
    let bytes = idx
        .memory_by_key
        .get(raw_key)
        .map_err(|source| MemoryOpError::Fjall { op: "get", source })?;
    Ok(bytes.and_then(|b| rmp_serde::from_slice(&b).ok()))
}

/// Serialize + insert `record` at `(scope, vis_byte, owner, key)`.
fn write_record(
    idx: &IndexDb,
    scope: &str,
    vis_byte: u8,
    owner: &str,
    key: &str,
    record: &MemoryRecord,
) -> Result<(), MemoryOpError> {
    let raw_key = crate::index::keys::memory_by_key(scope, vis_byte, owner, key);
    let bytes = rmp_serde::to_vec_named(record).map_err(MemoryOpError::Serialize)?;
    idx.memory_by_key
        .insert(raw_key, bytes)
        .map_err(|source| MemoryOpError::Fjall { op: "insert", source })
}

/// Remove the record at `(scope, vis_byte, owner, key)`; returns whether it existed.
fn remove_record(idx: &IndexDb, scope: &str, vis_byte: u8, owner: &str, key: &str) -> Result<bool, MemoryOpError> {
    let raw_key = crate::index::keys::memory_by_key(scope, vis_byte, owner, key);
    let existed = idx
        .memory_by_key
        .get(raw_key.as_slice())
        .map_err(|source| MemoryOpError::Fjall { op: "get", source })?
        .is_some();
    if existed {
        idx.memory_by_key
            .remove(raw_key)
            .map_err(|source| MemoryOpError::Fjall { op: "remove", source })?;
    }
    Ok(existed)
}

/// Micro-timestamp source shared with the LanceDB row stamping so both stores agree.
fn now_micros() -> i64 {
    crate::lance::now_micros()
}

/// Read-modify-write core for `memory_put`: preserve any prior `created_at`, stamp a fresh
/// `updated_at`, and write. No lock — the caller serializes same-key writes.
pub(crate) fn put_core(
    idx: &IndexDb,
    scope: &str,
    vis_byte: u8,
    owner: &str,
    key: &str,
    value: &str,
    tags: &[String],
) -> Result<(i64, i64), MemoryOpError> {
    let now = now_micros();
    let existing = read_record(idx, scope, vis_byte, owner, key)?;
    let created_at = existing.map(|r| r.created_at).unwrap_or(now);
    let record = MemoryRecord {
        value: value.to_string(),
        tags: tags.to_vec(),
        created_at,
        updated_at: now,
        provenance: Provenance::default(),
        verified: VerifyState::Unverified,
        last_verified: 0,
        importance: 0.0,
    };
    write_record(idx, scope, vis_byte, owner, key, &record)?;
    Ok((created_at, now))
}

/// Read core for `memory_get`: the full (untruncated) record as a wire entry, or `None`.
pub(crate) fn get_core(
    idx: &IndexDb,
    scope: &str,
    vis_byte: u8,
    owner: &str,
    key: &str,
) -> Result<Option<WireMemoryEntry>, MemoryOpError> {
    Ok(read_record(idx, scope, vis_byte, owner, key)?.map(|r| WireMemoryEntry {
        key: key.to_string(),
        value: r.value,
        tags: r.tags,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }))
}

/// Delete core for `memory_delete`: remove the record, reporting whether it existed.
pub(crate) fn delete_core(
    idx: &IndexDb,
    scope: &str,
    vis_byte: u8,
    owner: &str,
    key: &str,
) -> Result<bool, MemoryOpError> {
    remove_record(idx, scope, vis_byte, owner, key)
}

/// Truncate a value to [`MEMORY_PREVIEW_CHARS`] characters (never bytes), appending an ellipsis when
/// clipped. Char-boundary safe.
fn preview(value: &str) -> String {
    if value.len() > MEMORY_PREVIEW_CHARS {
        format!(
            "{}…",
            value
                .char_indices()
                .nth(MEMORY_PREVIEW_CHARS)
                .map(|(i, _)| &value[..i])
                .unwrap_or(value)
        )
    } else {
        value.to_string()
    }
}

/// Range-scan parameters for [`list_core`]. Bundled into one struct so the core stays under the
/// clippy argument-count cap and the two callers (local serve + daemon dispatch) construct it
/// identically.
pub(crate) struct ListQuery<'a> {
    /// Visibility byte resolved by the caller.
    pub vis_byte: u8,
    /// Owner segment (`""` for group).
    pub owner: &'a str,
    /// Key-prefix filter (`""` = no filter).
    pub key_prefix: &'a str,
    /// Optional exact-tag filter.
    pub tag: Option<&'a str>,
    /// Page size.
    pub limit: usize,
    /// Raw Fjall resume-key bytes from a previous page.
    pub cursor: Option<&'a [u8]>,
}

/// The result of a `List` core scan, mirroring the MCP `memory_list` response fields.
pub(crate) struct ListResult {
    /// The page of preview-truncated entries.
    pub entries: Vec<WireMemoryEntry>,
    /// Total matching records seen this scan.
    pub total: u32,
    /// Whether `total` exceeded `limit`.
    pub truncated: bool,
    /// Raw Fjall resume-key bytes for the next page, when more remain.
    pub next_cursor: Option<Vec<u8>>,
}

/// Range-scan core for `memory_list`: iterate the namespace, applying the key-prefix + tag filters,
/// truncating each surfaced value, and computing pagination. `query.cursor` is the raw last-seen key
/// bytes from a previous page; `next_cursor` is the raw last-emitted key bytes for the next page.
pub(crate) fn list_core(idx: &IndexDb, scope: &str, query: &ListQuery<'_>) -> Result<ListResult, MemoryOpError> {
    use std::ops::Bound;

    use super::cursor::prefix_upper_bound;

    let limit = query.limit;
    let ns_prefix = crate::index::keys::memory_by_key_ns_prefix(scope, query.vis_byte, query.owner);
    let upper = prefix_upper_bound(&ns_prefix);
    let lower: Bound<Vec<u8>> = match query.cursor {
        Some(k) => Bound::Excluded(k.to_vec()),
        None => Bound::Included(ns_prefix.clone()),
    };
    let upper_bound: Bound<Vec<u8>> = match upper {
        Some(b) => Bound::Excluded(b),
        None => Bound::Unbounded,
    };
    let scan_cap = limit.saturating_mul(8).max(2_000);
    let mut entries: Vec<WireMemoryEntry> = Vec::with_capacity(limit.min(64));
    let mut total: usize = 0;
    let mut last_emitted_key: Option<Vec<u8>> = None;
    let mut has_more = false;
    for guard in idx.memory_by_key.range::<Vec<u8>, _>((lower, upper_bound)) {
        let (raw_key, raw_val) = guard
            .into_inner()
            .map_err(|source| MemoryOpError::Fjall { op: "iter", source })?;
        let Some(key) = crate::index::keys::parse_memory_key_only(&raw_key) else {
            continue;
        };
        if !key.starts_with(query.key_prefix) {
            continue;
        }
        let Ok(record): Result<MemoryRecord, _> = rmp_serde::from_slice(&raw_val) else {
            continue;
        };
        if let Some(tag) = query.tag
            && !record.tags.iter().any(|t| t == tag)
        {
            continue;
        }
        total += 1;
        if entries.len() < limit {
            entries.push(WireMemoryEntry {
                key: key.to_string(),
                value: preview(&record.value),
                tags: record.tags,
                created_at: record.created_at,
                updated_at: record.updated_at,
            });
            last_emitted_key = Some(raw_key.to_vec());
        } else {
            has_more = true;
            if total > scan_cap {
                break;
            }
        }
    }
    let next_cursor = if has_more { last_emitted_key } else { None };
    Ok(ListResult {
        entries,
        total: total as u32,
        truncated: total > limit,
        next_cursor,
    })
}

/// Dispatch a wire [`MemoryOp`](crate::comms::memory_proto::MemoryOp) against a workspace's
/// read-write index, returning the wire outcome. This is the entry point the daemon calls; the local
/// serve path calls the per-op cores directly so it can interleave the LanceDB half without a second
/// match. Gated on `comms` because the wire enums live in `comms::memory_proto`.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) fn run_memory_op(
    idx: &IndexDb,
    scope: &str,
    op: &crate::comms::memory_proto::MemoryOp,
) -> Result<crate::comms::memory_proto::MemoryOutcome, MemoryOpError> {
    use crate::comms::memory_proto::{MemoryOp, MemoryOutcome};

    match op {
        MemoryOp::Get { vis_byte, owner, key } => Ok(MemoryOutcome::Got(get_core(idx, scope, *vis_byte, owner, key)?)),
        MemoryOp::Put {
            vis_byte,
            owner,
            key,
            value,
            tags,
        } => {
            let (created_at, updated_at) = put_core(idx, scope, *vis_byte, owner, key, value, tags)?;
            Ok(MemoryOutcome::Put { created_at, updated_at })
        }
        MemoryOp::List {
            vis_byte,
            owner,
            prefix,
            tag,
            limit,
            cursor,
        } => {
            let result = list_core(
                idx,
                scope,
                &ListQuery {
                    vis_byte: *vis_byte,
                    owner,
                    key_prefix: prefix,
                    tag: tag.as_deref(),
                    limit: *limit as usize,
                    cursor: cursor.as_deref(),
                },
            )?;
            Ok(MemoryOutcome::Listed {
                entries: result.entries,
                total: result.total,
                truncated: result.truncated,
                next_cursor: result.next_cursor,
            })
        }
        MemoryOp::Delete { vis_byte, owner, key } => Ok(MemoryOutcome::Deleted {
            deleted: delete_core(idx, scope, *vis_byte, owner, key)?,
        }),
    }
}
