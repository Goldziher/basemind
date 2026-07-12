//! Wire types for forwarding the CORE memory operations (`memory_get` / `memory_put` /
//! `memory_list` / `memory_delete`) from a `daemon_writer` serve to the machine daemon.
//!
//! Under Seam B a comms-build `serve` opens its store read-only (no fjall index), so the daemon —
//! the machine's sole fjall writer — owns the `memory_by_key` keyspace. Serve resolves the
//! `(vis_byte, owner)` namespace and the scope on its side, ships a [`MemoryOp`] over the socket,
//! and the daemon runs it against the workspace's read-write index. The vector (LanceDB) half stays
//! on serve — the daemon never loads the ONNX embedder — so `memory_search` is untouched and
//! `memory_put` / `memory_delete` do their embed + upsert/delete locally AFTER the fjall RPC.
//!
//! Every type is float-free, so `Eq` is derivable and the protocol enums keep their `Eq` bound.

#![cfg(all(feature = "comms", feature = "memory"))]

use serde::{Deserialize, Serialize};

/// The wire entry shape is owned by [`crate::mcp::memory_ops`] (so its fjall cores compile on a
/// `memory`-only build without `comms`); the protocol reuses it verbatim.
pub use crate::mcp::memory_ops::WireMemoryEntry;

/// A CORE memory operation forwarded to the daemon. The namespace coordinates (`vis_byte`, `owner`)
/// are resolved serve-side via `namespace(state, visibility)`; the daemon does not re-derive scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryOp {
    /// Read one record by key. Returns [`MemoryOutcome::Got`].
    Get {
        /// Visibility byte (`group` / `individual`) resolved serve-side.
        vis_byte: u8,
        /// Owner segment (`""` for group, the agent id for individual).
        owner: String,
        /// The memory key.
        key: String,
    },
    /// Upsert a record (read-modify-write preserving `created_at`). Returns [`MemoryOutcome::Put`].
    Put {
        /// Visibility byte resolved serve-side.
        vis_byte: u8,
        /// Owner segment.
        owner: String,
        /// The memory key.
        key: String,
        /// The value to store.
        value: String,
        /// Free-form tags.
        tags: Vec<String>,
    },
    /// Range-scan a namespace, optionally filtered by key prefix + tag. Returns
    /// [`MemoryOutcome::Listed`]. Entries carry the same preview truncation the MCP tool applies.
    List {
        /// Visibility byte resolved serve-side.
        vis_byte: u8,
        /// Owner segment.
        owner: String,
        /// Key-prefix filter (`""` = no filter).
        prefix: String,
        /// Optional exact-tag filter.
        tag: Option<String>,
        /// Page size.
        limit: u32,
        /// Raw Fjall resume-key bytes from a previous page's `next_cursor`.
        cursor: Option<Vec<u8>>,
    },
    /// Delete a record by key. Returns [`MemoryOutcome::Deleted`].
    Delete {
        /// Visibility byte resolved serve-side.
        vis_byte: u8,
        /// Owner segment.
        owner: String,
        /// The memory key.
        key: String,
    },
}

/// The daemon's reply to a [`MemoryOp`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryOutcome {
    /// Reply to [`MemoryOp::Put`]: the resolved timestamps (`created_at` preserved across an RMW).
    Put {
        /// Micros the record was first created (preserved from any prior record).
        created_at: i64,
        /// Micros of this write.
        updated_at: i64,
    },
    /// Reply to [`MemoryOp::Get`]: the record, or `None` when the key is absent.
    Got(Option<WireMemoryEntry>),
    /// Reply to [`MemoryOp::List`]: a page of entries plus pagination metadata.
    Listed {
        /// The page of entries (preview-truncated values).
        entries: Vec<WireMemoryEntry>,
        /// Total matching records seen this scan (may exceed `entries.len()`).
        total: u32,
        /// Whether `total` exceeded `limit` (more records match than were returned).
        truncated: bool,
        /// Raw Fjall resume-key bytes for the next page, when more remain.
        next_cursor: Option<Vec<u8>>,
    },
    /// Reply to [`MemoryOp::Delete`]: whether a record existed and was removed.
    Deleted {
        /// `true` when a record existed and was deleted.
        deleted: bool,
    },
}
