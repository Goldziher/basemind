//! Wire types for forwarding the fjall-backed reference reads from a `daemon_writer` serve to the
//! machine daemon.
//!
//! A `daemon_writer` serve opens its store without the fjall index (fjall's directory lock is
//! single-process and the daemon holds it), so `references` / `callers` / `implementations` — which
//! are range scans over `calls_by_callee`, `calls_by_path` and `implementations_by_trait` — have
//! nothing to scan locally. The previous fallback projected every call site into the session's
//! RAM under a byte budget, so on a large monorepo the answer was truncated. Forwarding the query
//! instead means the daemon runs the SAME scan a writer session runs, against the SAME index, and
//! ships back only the page: the session holds no projection and the answer is complete.
//!
//! Every reply is one page (bounded by the clamped `limit`) or a bounded per-file batch, never a
//! corpus-sized payload. All types are float-free, so `Eq` is derivable.

#![cfg(all(feature = "comms", any(unix, windows)))]

use serde::{Deserialize, Serialize};

use crate::path::RelPath;

/// Ceiling on the files one [`IndexReadQuery::CallsInFiles`] may name. Enforced daemon-side: the
/// list arrives off the wire and each file costs a `calls_by_path` prefix scan under the
/// workspace lock.
pub const MAX_CALLS_IN_FILES: usize = 512;

/// Ceiling on the files one [`IndexReadQuery::GrepBloom`] may name. Enforced daemon-side (the list
/// arrives off the wire); the client splits a larger window into requests of at most this many.
pub const MAX_GREP_BLOOM_PATHS: usize = 16_384;

/// A fjall-backed reference read forwarded to the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexReadQuery {
    /// The `calls_by_callee` name scan behind `references` and `callers`.
    CallScan {
        /// Case-sensitive substring of the callee identifier.
        name: String,
        /// Page size, already clamped by the caller; clamped again daemon-side.
        limit: u32,
        /// Raw key of the last hit of the previous page (exclusive lower bound).
        cursor: Option<Vec<u8>>,
    },
    /// Every call site of each named file (`calls_by_path`), for the `callers` resolved refinement.
    CallsInFiles {
        /// Files to read, at most [`MAX_CALLS_IN_FILES`].
        paths: Vec<RelPath>,
    },
    /// The per-file trigram bloom prefilter behind `code grep` (ADR-0012): which of `paths` provably
    /// cannot match `pattern`. The daemon compiles the pattern's required literals itself, so the
    /// client and the index can never disagree about them.
    GrepBloom {
        /// The grep regex source.
        pattern: String,
        /// Files to test, at most [`MAX_GREP_BLOOM_PATHS`].
        paths: Vec<RelPath>,
    },
    /// The `implementations_by_trait` scan behind `implementations`.
    ImplScan {
        /// Case-sensitive substring of the trait name.
        trait_name: String,
        /// Only implementations in files of this language.
        language: Option<String>,
        /// Page size, already clamped by the caller; clamped again daemon-side.
        limit: u32,
        /// Raw key of the last hit of the previous page (exclusive lower bound).
        cursor: Option<Vec<u8>>,
    },
}

/// The daemon's answer to an [`IndexReadQuery`]; the variant mirrors the query.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexReadResult {
    /// Reply to [`IndexReadQuery::CallScan`].
    CallScan(WireCallScan),
    /// Reply to [`IndexReadQuery::CallsInFiles`]: one entry per requested path, in request order.
    CallsInFiles(Vec<Vec<WireCall>>),
    /// Reply to [`IndexReadQuery::ImplScan`].
    ImplScan(WireImplScan),
    /// Reply to [`IndexReadQuery::GrepBloom`]: one flag per requested path, in request order; `true`
    /// means the file provably cannot match and may be skipped.
    GrepSkip(Vec<bool>),
}

/// One page of the call-site name scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCallScan {
    /// Matching call sites repo-wide (a lower bound when `total_is_partial`).
    pub total: u32,
    /// The scan hit its own `scan_cap` before exhausting the partition.
    pub total_is_partial: bool,
    /// Whether matches remain past this page. The cursor is the last hit's key.
    pub has_more: bool,
    /// The page's hits, in key order.
    pub hits: Vec<WireCallHit>,
}

/// A call-site hit with the index key it was read under (the cursor / budget re-anchor material).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCallHit {
    /// `calls_by_callee` key.
    pub key: Vec<u8>,
    /// File containing the call.
    pub path: RelPath,
    /// 1-based line.
    pub line: u32,
    /// 0-based byte column.
    pub column: u32,
    /// The callee identifier the index captured.
    pub callee: String,
    /// Call-site start byte.
    pub start_byte: u32,
}

/// A call site within one file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCall {
    /// The callee identifier.
    pub callee: String,
    /// Call-site start byte.
    pub start_byte: u32,
    /// 1-based line.
    pub line: u32,
    /// 0-based byte column.
    pub column: u32,
}

/// One page of the implementation scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireImplScan {
    /// Matching implementations repo-wide (a lower bound when `total_is_partial`).
    pub total: u64,
    /// The scan hit its own `scan_cap` before exhausting the partition.
    pub total_is_partial: bool,
    /// Whether matches remain past this page. The cursor is the last hit's key.
    pub has_more: bool,
    /// The page's hits, in key order.
    pub hits: Vec<WireImplHit>,
}

/// An implementation hit with its index key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireImplHit {
    /// `implementations_by_trait` key.
    pub key: Vec<u8>,
    /// File containing the implementation.
    pub path: RelPath,
    /// The trait / base type implemented.
    pub trait_name: String,
    /// The implementing type.
    pub impl_type: String,
    /// 1-based row.
    pub start_row: u32,
    /// 0-based byte column.
    pub start_col: u32,
}
