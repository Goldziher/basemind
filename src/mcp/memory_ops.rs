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

/// The most keys one batched read may name. The serving side refuses a larger request; a caller
/// with more keys splits them (`memory_get_many_wire`).
pub(crate) const GET_MANY_MAX_KEYS: usize = 256;

/// The most bytes of notes one batched read answers with, well inside the daemon's 16 MiB frame.
/// The reply carries every note whole, so a few long notes fill it before the key cap does: the
/// serving side stops early and the caller asks again from the first key left unanswered.
pub(crate) const GET_MANY_MAX_BYTES: usize = 8 * 1024 * 1024;

/// The most keys one read of stamps may name. A stamp is one number, so the reply stays small.
pub(crate) const STAMPS_MAX_KEYS: usize = 4096;

/// The longest key a note saved now may have, in bytes.
pub(crate) const KEY_MAX_BYTES: usize = 1024;

/// The most bytes of text and tags a note saved now may have. A note crosses the daemon's 16 MiB
/// frame whole, in the save and in every read of it.
pub(crate) const NOTE_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Refuse a note too long to save: its key past [`KEY_MAX_BYTES`], or its text and tags past
/// [`NOTE_MAX_BYTES`].
pub(crate) fn check_note(key: &str, value: &str, tags: &[String]) -> Result<(), MemoryOpError> {
    if key.len() > KEY_MAX_BYTES {
        return Err(MemoryOpError::TooLong {
            what: "key",
            bytes: key.len(),
            most: KEY_MAX_BYTES,
        });
    }
    let note = value.len() + tags.iter().map(String::len).sum::<usize>();
    if note > NOTE_MAX_BYTES {
        return Err(MemoryOpError::TooLong {
            what: "value with its tags",
            bytes: note,
            most: NOTE_MAX_BYTES,
        });
    }
    Ok(())
}

/// A fjall-side memory operation failed. Kept small and stringly so it maps cleanly to both an
/// [`McpError`](rmcp::ErrorData) on the local path and a `CommsResponse::Error` on the daemon path.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MemoryOpError {
    /// The read-write `memory_by_key` index was not available (should never happen daemon-side).
    /// Constructed only when the daemon resolves a workspace whose store has no index, so it is
    /// gated on the daemon build — under a `memory`-only (no-`comms`) build there is no daemon path
    /// and the variant would be dead.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    #[error("memory_by_key index not available")]
    IndexUnavailable,
    /// The group memory store is owned by another process (the daemon), so this handle holds no
    /// memory; the operation must go through that process.
    #[error("memory store is held by another process; use the basemind daemon for memory operations")]
    MemoryDetached,
    /// A newer basemind wrote the group memory store in a layout this binary does not know.
    #[error(
        "memory store written by a newer basemind (version {stored}); this binary understands {current} \
         — upgrade basemind to read or write memory notes here"
    )]
    NewerStore {
        /// The version the store carries.
        stored: u32,
        /// The version this binary writes.
        current: u32,
    },
    /// One batched read named more keys than a request may carry.
    #[error(
        "a batched memory read carries at most {most} keys, and this one has {asked}; split it into several \
         requests"
    )]
    TooManyKeys {
        /// How many keys the request named.
        asked: usize,
        /// How many it may name.
        most: usize,
    },
    /// A key or a note longer than a save or a request may carry.
    #[error("a memory {what} carries at most {most} bytes, and this one has {bytes}")]
    TooLong {
        /// What was too long.
        what: &'static str,
        /// How long it was.
        bytes: usize,
        /// How long it may be.
        most: usize,
    },
    /// A fjall get / insert / remove failed.
    #[error("fjall {op}: {source}")]
    Fjall {
        /// The fjall verb that failed (`get` / `insert` / `remove` / `iter`).
        op: &'static str,
        /// The underlying fjall error.
        source: fjall::Error,
    },
    /// The fsync that makes an acknowledged write durable failed.
    #[error("fsync memory store: {0}")]
    Sync(String),
    /// Opening or creating the group store on its first use failed.
    #[error("open memory store: {0}")]
    Open(String),
    /// Serializing a [`MemoryRecord`] to msgpack failed.
    #[error("serialize memory record: {0}")]
    Serialize(rmp_serde::encode::Error),
}

impl From<MemoryOpError> for rmcp::ErrorData {
    fn from(error: MemoryOpError) -> Self {
        match error {
            MemoryOpError::TooLong { .. } => rmcp::ErrorData::invalid_params(error.to_string(), None),
            _ => rmcp::ErrorData::internal_error(error.to_string(), None),
        }
    }
}

/// The notes store behind `idx`, or the refusal for a handle that holds none: the store is owned
/// by another process, or a newer basemind wrote it.
pub(crate) fn require_memory(idx: &IndexDb) -> Result<&crate::index::memory_store::MemoryDb, MemoryOpError> {
    use crate::index::MemoryUnavailable;

    idx.memory().map_err(|why| match why {
        MemoryUnavailable::HeldElsewhere => MemoryOpError::MemoryDetached,
        MemoryUnavailable::NewerStore { stored, current } => MemoryOpError::NewerStore { stored, current },
    })
}

/// The notes keyspace for a read, or `None` while the group store does not exist (nothing was ever
/// saved). A read never creates the store.
fn notes_for_read(idx: &IndexDb) -> Result<Option<&fjall::Keyspace>, MemoryOpError> {
    require_memory(idx)?.notes().map_err(store_error)
}

/// The notes keyspace for a write, creating the group store on the first one.
fn notes_for_write(idx: &IndexDb) -> Result<&fjall::Keyspace, MemoryOpError> {
    require_memory(idx)?.notes_for_write().map_err(store_error)
}

/// A failure opening the group store on first use, mapped like the same failure at handle open.
fn store_error(error: crate::index::IndexError) -> MemoryOpError {
    match error {
        crate::index::IndexError::Fjall(fjall::Error::Locked) => MemoryOpError::MemoryDetached,
        crate::index::IndexError::NewerMemoryStore { stored, current } => MemoryOpError::NewerStore { stored, current },
        crate::index::IndexError::Fjall(source) => MemoryOpError::Fjall { op: "open", source },
        other => MemoryOpError::Open(other.to_string()),
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
    let Some(notes) = notes_for_read(idx)? else {
        return Ok(None);
    };
    let bytes = notes
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
    notes_for_write(idx)?
        .insert(raw_key, bytes)
        .map_err(|source| MemoryOpError::Fjall { op: "insert", source })?;
    sync_memory(idx)
}

/// Make the write just issued durable before it is acknowledged (see
/// [`MemoryDb::sync`](crate::index::memory_store::MemoryDb::sync)).
fn sync_memory(idx: &IndexDb) -> Result<(), MemoryOpError> {
    require_memory(idx)?
        .sync()
        .map_err(|error| MemoryOpError::Sync(error.to_string()))
}

/// Remove the record at `(scope, vis_byte, owner, key)`; returns whether it existed.
fn remove_record(idx: &IndexDb, scope: &str, vis_byte: u8, owner: &str, key: &str) -> Result<bool, MemoryOpError> {
    let raw_key = crate::index::keys::memory_by_key(scope, vis_byte, owner, key);
    let Some(notes) = notes_for_read(idx)? else {
        return Ok(false);
    };
    let existed = notes
        .get(raw_key.as_slice())
        .map_err(|source| MemoryOpError::Fjall { op: "get", source })?
        .is_some();
    if existed {
        notes
            .remove(raw_key)
            .map_err(|source| MemoryOpError::Fjall { op: "remove", source })?;
        sync_memory(idx)?;
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
    check_note(key, value, tags)?;
    require_memory(idx)?;
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
    require_memory(idx)?;
    Ok(read_record(idx, scope, vis_byte, owner, key)?.map(|r| WireMemoryEntry {
        key: key.to_string(),
        value: r.value,
        tags: r.tags,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }))
}

/// [`get_core`] for several keys of one namespace: one slot per key, in order, for the keys
/// whose notes fit in [`GET_MANY_MAX_BYTES`]. Always the first; the caller asks again for the rest.
pub(crate) fn get_many_core(
    idx: &IndexDb,
    scope: &str,
    vis_byte: u8,
    owner: &str,
    keys: &[String],
) -> Result<Vec<Option<WireMemoryEntry>>, MemoryOpError> {
    get_many_within(idx, scope, vis_byte, owner, keys, GET_MANY_MAX_BYTES)
}

/// [`get_many_core`] answering with at most `max_bytes` of notes, or the first note alone.
fn get_many_within(
    idx: &IndexDb,
    scope: &str,
    vis_byte: u8,
    owner: &str,
    keys: &[String],
    max_bytes: usize,
) -> Result<Vec<Option<WireMemoryEntry>>, MemoryOpError> {
    if keys.len() > GET_MANY_MAX_KEYS {
        return Err(MemoryOpError::TooManyKeys {
            asked: keys.len(),
            most: GET_MANY_MAX_KEYS,
        });
    }
    let mut entries = Vec::with_capacity(keys.len());
    let mut bytes = 0_usize;
    for key in keys {
        let entry = get_core(idx, scope, vis_byte, owner, key)?;
        let size = key.len() + entry.as_ref().map_or(0, wire_bytes);
        if !entries.is_empty() && bytes.saturating_add(size) > max_bytes {
            break;
        }
        bytes = bytes.saturating_add(size);
        entries.push(entry);
    }
    Ok(entries)
}

/// What an entry costs a reply, near enough: its strings, and room for the numbers and framing.
fn wire_bytes(entry: &WireMemoryEntry) -> usize {
    const FIXED: usize = 64;
    entry.key.len() + entry.value.len() + entry.tags.iter().map(|tag| tag.len() + 8).sum::<usize>() + FIXED
}

/// The `updated_at` of each key's note, in order, `None` where no note exists: what decides
/// whether a vector row is current, without the notes' text.
pub(crate) fn stamps_core(
    idx: &IndexDb,
    scope: &str,
    vis_byte: u8,
    owner: &str,
    keys: &[String],
) -> Result<Vec<Option<i64>>, MemoryOpError> {
    if keys.len() > STAMPS_MAX_KEYS {
        return Err(MemoryOpError::TooManyKeys {
            asked: keys.len(),
            most: STAMPS_MAX_KEYS,
        });
    }
    require_memory(idx)?;
    keys.iter()
        .map(|key| Ok(read_record(idx, scope, vis_byte, owner, key)?.map(|record| record.updated_at)))
        .collect()
}

/// Delete core for `memory_delete`: remove the record, reporting whether it existed.
pub(crate) fn delete_core(
    idx: &IndexDb,
    scope: &str,
    vis_byte: u8,
    owner: &str,
    key: &str,
) -> Result<bool, MemoryOpError> {
    require_memory(idx)?;
    remove_record(idx, scope, vis_byte, owner, key)
}

/// Truncate a value to [`MEMORY_PREVIEW_CHARS`] characters (never bytes), appending an ellipsis when
/// clipped. Char-boundary safe.
pub(crate) fn preview(value: &str) -> String {
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
    /// Raw Fjall resume-key bytes for the next page, when more remain.
    pub next_cursor: Option<Vec<u8>>,
}

/// How many matching notes a listing counts before it stops: `total` is exact below this and a
/// lower bound at it. The listing's `truncated_reason: "scan_cap"` is derived from it.
pub(crate) fn list_scan_cap(limit: usize) -> usize {
    limit.saturating_mul(8).max(2_000)
}

/// Range-scan core for `memory_list`: iterate the namespace, applying the key-prefix + tag filters,
/// truncating each surfaced value, and computing pagination. `query.cursor` is the raw last-seen key
/// bytes from a previous page; `next_cursor` is the raw last-emitted key bytes for the next page.
pub(crate) fn list_core(idx: &IndexDb, scope: &str, query: &ListQuery<'_>) -> Result<ListResult, MemoryOpError> {
    require_memory(idx)?;
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
    let scan_cap = list_scan_cap(limit);
    let mut entries: Vec<WireMemoryEntry> = Vec::with_capacity(limit.min(64));
    let mut total: usize = 0;
    let mut last_emitted_key: Option<Vec<u8>> = None;
    let mut has_more = false;
    let notes = notes_for_read(idx)?;
    for guard in notes
        .map(|notes| notes.range::<Vec<u8>, _>((lower, upper_bound)))
        .into_iter()
        .flatten()
    {
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
        next_cursor,
    })
}

/// Room a daemon reply keeps beside the notes it carries, for its framing and the entries' numbers.
#[cfg(all(feature = "comms", any(unix, windows)))]
const REPLY_FRAME_SLACK: usize = 1024 * 1024;

/// `outcome` made fit to cross the daemon's socket, or why it cannot. A note saved before notes
/// were capped ([`NOTE_MAX_BYTES`]) can be too long for one frame; sent as it is, the reply would
/// break the link and say nothing of the note. Read by key it is refused by name. In a batched read
/// (a search's join, a backfill batch) its slot is emptied and the note named in the daemon's log:
/// the reader takes it for a note it cannot serve, and one such note costs only itself instead of
/// failing every search whose hits or batch include it.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) fn fit_for_a_frame(
    outcome: crate::comms::memory_proto::MemoryOutcome,
) -> Result<crate::comms::memory_proto::MemoryOutcome, String> {
    use crate::comms::memory_proto::MemoryOutcome;
    let most = crate::comms::transport::MAX_FRAME_BYTES - REPLY_FRAME_SLACK;
    let too_long = |entry: &WireMemoryEntry| {
        format!(
            "the memory note `{}` is {} bytes, more than a reply from the daemon carries ({most}); it was saved \
             before notes were capped at {NOTE_MAX_BYTES} bytes. Save it again under the same key with shorter \
             text to read it",
            entry.key,
            wire_bytes(entry)
        )
    };
    match outcome {
        MemoryOutcome::Got(Some(entry)) if wire_bytes(&entry) > most => Err(too_long(&entry)),
        MemoryOutcome::GotMany(mut entries) => {
            for slot in &mut entries {
                if let Some(entry) = slot.take_if(|entry| wire_bytes(entry) > most) {
                    tracing::warn!(reason = %too_long(&entry), "a batched memory read left a note out");
                }
            }
            Ok(MemoryOutcome::GotMany(entries))
        }
        other => Ok(other),
    }
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
        MemoryOp::GetMany { vis_byte, owner, keys } => Ok(MemoryOutcome::GotMany(get_many_core(
            idx, scope, *vis_byte, owner, keys,
        )?)),
        MemoryOp::Stamps { vis_byte, owner, keys } => {
            Ok(MemoryOutcome::Stamps(stamps_core(idx, scope, *vis_byte, owner, keys)?))
        }
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
                truncated: result.total as usize > *limit as usize,
                next_cursor: result.next_cursor,
            })
        }
        MemoryOp::Delete { vis_byte, owner, key } => Ok(MemoryOutcome::Deleted {
            deleted: delete_core(idx, scope, *vis_byte, owner, key)?,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    /// A note cannot be rebuilt from anything, so it is fsynced before the call returns.
    #[test]
    fn a_put_and_a_delete_are_fsynced_before_they_are_acknowledged() {
        let dir = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(dir.path()).unwrap();
        let syncs = || idx.memory().expect("notes").syncs.load(Ordering::Acquire);
        let before = syncs();

        put_core(&idx, "scope", 0, "", "k", "v", &[]).unwrap();
        assert_eq!(syncs(), before + 1, "a put must fsync the memory store");

        assert!(delete_core(&idx, "scope", 0, "", "k").unwrap());
        assert_eq!(syncs(), before + 2, "a delete must fsync the memory store");
    }

    /// One slot per requested key, in the order asked, `None` where no note exists.
    #[test]
    fn a_batched_read_answers_every_key_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(dir.path()).unwrap();
        put_core(&idx, "scope", 0, "", "a", "one", &[]).unwrap();
        put_core(&idx, "scope", 0, "", "b", "two", &[]).unwrap();
        let keys = ["b".to_string(), "missing".to_string(), "a".to_string()];

        let read = get_many_core(&idx, "scope", 0, "", &keys).unwrap();

        let values: Vec<Option<&str>> = read
            .iter()
            .map(|note| note.as_ref().map(|n| n.value.as_str()))
            .collect();
        assert_eq!(values, [Some("two"), None, Some("one")]);
    }

    /// The daemon answers the batched read through the same dispatch as every other memory op.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    #[test]
    fn the_daemon_dispatch_serves_a_batched_read() {
        use crate::comms::memory_proto::{MemoryOp, MemoryOutcome};

        let dir = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(dir.path()).unwrap();
        put_core(&idx, "scope", 0, "", "a", "one", &[]).unwrap();
        let op = MemoryOp::GetMany {
            vis_byte: 0,
            owner: String::new(),
            keys: vec!["a".into(), "missing".into()],
        };

        let MemoryOutcome::GotMany(read) = run_memory_op(&idx, "scope", &op).unwrap() else {
            panic!("a batched read answers with GotMany");
        };
        assert_eq!(read.len(), 2);
        assert!(read[0].as_ref().is_some_and(|note| note.value == "one") && read[1].is_none());
    }

    /// A batched read is bounded where it is served: the reply carries every note whole, and a
    /// frame to the daemon's client has a size limit.
    #[test]
    fn a_batched_read_over_the_cap_is_refused_and_one_at_the_cap_is_served() {
        let dir = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(dir.path()).unwrap();
        put_core(&idx, "scope", 0, "", "k", "v", &[]).unwrap();
        let keys = |count: usize| -> Vec<String> {
            std::iter::once("k".to_string())
                .chain((1..count).map(|index| format!("absent-{index}")))
                .collect()
        };

        let served = get_many_core(&idx, "scope", 0, "", &keys(GET_MANY_MAX_KEYS)).unwrap();
        assert_eq!(served.len(), GET_MANY_MAX_KEYS);
        assert_eq!(served[0].as_ref().map(|note| note.value.as_str()), Some("v"));

        let refused = get_many_core(&idx, "scope", 0, "", &keys(GET_MANY_MAX_KEYS + 1)).unwrap_err();
        let message = refused.to_string();
        assert!(
            message.contains("at most 256 keys") && message.contains("has 257"),
            "{message}"
        );
    }

    /// The daemon serves a forwarded batch through the same bound.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    #[test]
    fn the_daemon_refuses_a_forwarded_batch_over_the_cap() {
        use crate::comms::memory_proto::MemoryOp;

        let dir = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(dir.path()).unwrap();
        let op = MemoryOp::GetMany {
            vis_byte: 0,
            owner: String::new(),
            keys: (0..=GET_MANY_MAX_KEYS).map(|index| format!("k{index}")).collect(),
        };

        let refused = run_memory_op(&idx, "scope", &op).unwrap_err();
        assert!(matches!(refused, MemoryOpError::TooManyKeys { asked, .. } if asked == GET_MANY_MAX_KEYS + 1));
    }

    /// The notes of a reply fill it to its byte cap and no further: the first note always goes, and
    /// the caller asks again from the first key left out.
    #[test]
    fn a_batched_read_stops_before_its_byte_cap() {
        let dir = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(dir.path()).unwrap();
        let long = "x".repeat(1000);
        for key in ["a", "b", "c"] {
            put_core(&idx, "scope", 0, "", key, &long, &[]).unwrap();
        }
        let keys: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();

        let two = get_many_within(&idx, "scope", 0, "", &keys, 2500).unwrap();
        assert_eq!(two.len(), 2, "a third note would pass the cap");
        let one = get_many_within(&idx, "scope", 0, "", &keys, 10).unwrap();
        assert_eq!(one.len(), 1, "the first note goes whatever its size");
        assert!(one[0].as_ref().is_some_and(|note| note.value == long));
        assert_eq!(
            get_many_within(&idx, "scope", 0, "", &keys, usize::MAX).unwrap().len(),
            3
        );
    }

    /// Stamps answer whether a row is current without carrying the notes' text, and the daemon
    /// serves them through the same dispatch.
    #[test]
    fn stamps_are_read_without_the_notes() {
        let dir = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(dir.path()).unwrap();
        let (_, written) = put_core(&idx, "scope", 0, "", "a", "one", &[]).unwrap();
        let keys = ["missing".to_string(), "a".to_string()];

        assert_eq!(stamps_core(&idx, "scope", 0, "", &keys).unwrap(), [None, Some(written)]);
        let too_many: Vec<String> = (0..=STAMPS_MAX_KEYS).map(|index| format!("k{index}")).collect();
        assert!(matches!(
            stamps_core(&idx, "scope", 0, "", &too_many),
            Err(MemoryOpError::TooManyKeys {
                most: STAMPS_MAX_KEYS,
                ..
            })
        ));

        #[cfg(all(feature = "comms", any(unix, windows)))]
        {
            use crate::comms::memory_proto::{MemoryOp, MemoryOutcome};
            let op = MemoryOp::Stamps {
                vis_byte: 0,
                owner: String::new(),
                keys: keys.to_vec(),
            };
            assert_eq!(
                run_memory_op(&idx, "scope", &op).unwrap(),
                MemoryOutcome::Stamps(vec![None, Some(written)])
            );
        }
    }

    /// The notes of a newer basemind are refused by name, for a read and for a write.
    #[test]
    fn memory_operations_on_a_store_of_a_newer_version_are_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path().join("memory.fjall");
        drop(IndexDb::open_with_memory(dir.path(), &memory_dir).unwrap());
        crate::index::memory_store::stamp_newer_for_test(&memory_dir);
        let idx = IndexDb::open_with_memory(dir.path(), &memory_dir).expect("the code index opens");

        for refused in [
            get_core(&idx, "scope", 0, "", "k").map(|_| ()).unwrap_err(),
            put_core(&idx, "scope", 0, "", "k", "v", &[]).map(|_| ()).unwrap_err(),
            delete_core(&idx, "scope", 0, "", "k").map(|_| ()).unwrap_err(),
        ] {
            let message = refused.to_string();
            assert!(
                matches!(refused, MemoryOpError::NewerStore { .. })
                    && message.contains("written by a newer basemind")
                    && message.contains("upgrade basemind"),
                "{message}"
            );
        }
    }

    /// A note too long for the daemon's frame is refused at the save, wherever it is served, with
    /// what was too long; one at the limits is saved.
    #[test]
    fn a_note_past_the_limits_is_refused_and_one_at_them_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(dir.path()).unwrap();
        let key = "k".repeat(KEY_MAX_BYTES);
        let value = "v".repeat(NOTE_MAX_BYTES - 3);
        put_core(&idx, "scope", 0, "", &key, &value, &["tag".to_string()]).expect("at the limits");

        let long_key = put_core(&idx, "scope", 0, "", &format!("{key}k"), "v", &[]).unwrap_err();
        assert!(
            long_key.to_string().contains("key carries at most 1024 bytes"),
            "{long_key}"
        );
        let long_note = put_core(&idx, "scope", 0, "", "k", &value, &["tags".to_string()]).unwrap_err();
        assert!(
            matches!(
                long_note,
                MemoryOpError::TooLong {
                    most: NOTE_MAX_BYTES,
                    ..
                }
            ),
            "{long_note}"
        );
        assert_eq!(
            rmcp::ErrorData::from(long_note).code,
            rmcp::model::ErrorCode::INVALID_PARAMS
        );
    }

    /// A note saved before the cap and too long for a frame is refused by name when read by key,
    /// and left out of a batched read, which still answers the notes beside it; one that fits is
    /// sent.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    #[test]
    fn a_note_too_long_for_a_frame_is_refused_by_name() {
        use crate::comms::memory_proto::MemoryOutcome;
        let note = |value: String| WireMemoryEntry {
            key: "huge".into(),
            value,
            tags: Vec::new(),
            created_at: 1,
            updated_at: 1,
        };
        let long = note("x".repeat(crate::comms::transport::MAX_FRAME_BYTES));
        let fits = note("x".repeat(NOTE_MAX_BYTES * 2));
        let refused = fit_for_a_frame(MemoryOutcome::Got(Some(long.clone())));
        assert!(refused.is_err_and(|message| message.contains("`huge`")));
        let batch = MemoryOutcome::GotMany(vec![None, Some(long), Some(fits.clone())]);
        assert_eq!(
            fit_for_a_frame(batch),
            Ok(MemoryOutcome::GotMany(vec![None, None, Some(fits.clone())])),
            "only the note too long is left out"
        );
        let one = MemoryOutcome::Got(Some(fits));
        assert_eq!(fit_for_a_frame(one.clone()), Ok(one));
    }
}
