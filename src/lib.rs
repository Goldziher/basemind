#![allow(missing_docs)]

pub mod alloc_tag;
pub mod backpressure;
mod boot_identity;
#[cfg(feature = "code-search")]
pub mod chunk;
/// The scan pipeline's one chunked-drive primitive: a serial loop over weight-bounded chunks with
/// a parallel interior, so no phase holds an O(corpus) set of per-file results live.
pub(crate) mod chunk_drive;
pub mod cli;
pub mod comms;
pub mod config;
/// Single-owner daemon lock + pidfile + the machine-wide, kind-tagged live-daemon registry and
/// ceiling, shared by every daemon family.
pub mod daemon_lock;
pub mod disk_space;
#[cfg(feature = "intelligence")]
pub mod embeddings;
pub mod eval;
pub mod extract;
pub mod git;
pub mod git_cache;
pub mod git_history;
pub mod hashing;
pub mod index;
/// Code-intelligence tier: scope/import-resolved navigation. Gated per-language on the
/// engine that backs it (`code-intel-js` = oxc). See `src/extract/locals.rs` for the
/// grammar-native intra-file layer that needs no feature flag.
pub mod intel;
#[cfg(feature = "intelligence")]
pub mod lance;
pub mod lang;
pub mod lang_rules;
pub mod mcp;
pub mod path;
pub mod process_start;
pub mod query;
pub mod registry;
pub mod render;
pub(crate) mod retrieval_wiring;
pub mod scan_evidence;
mod scan_evidence_hosts;
pub mod scan_phase_peaks;
pub(crate) mod scan_retrieval;
pub mod scanner;
pub(crate) mod scanner_candidates;
#[cfg(feature = "code-search")]
pub mod scanner_code;
#[cfg(feature = "documents")]
pub mod scanner_docs;
#[cfg(feature = "documents")]
pub(crate) mod scanner_docs_lane;
pub(crate) mod scanner_drive;
pub mod scanner_file;
pub(crate) mod scanner_filter;
pub(crate) mod scanner_flush;
pub(crate) mod scanner_index_batch;
pub(crate) mod scanner_integrity;
pub mod scanner_lanes;
pub(crate) mod scanner_policy;
pub(crate) mod scanner_vectors;
pub mod search;
pub mod store;
pub mod store_base;
mod store_base_retention;
pub mod store_blob;
mod store_blob_codec;
pub mod store_cache_admin;
pub mod store_doc_tombstone;
pub mod store_gc;
pub mod store_gc_budget;
pub(crate) mod store_gc_live;
mod store_gc_namespaces;
pub mod store_gc_protect;
pub mod store_gc_workspace;
mod store_group_live;
mod store_hash_refs;
pub mod store_layout;
mod store_lock;
mod store_migration;
pub mod store_pending;
mod store_seed;
pub mod sysres;
pub mod textmatch;
pub mod version;
pub mod watcher;

pub use config::Config;

/// Test-only helpers exposed from the library so integration tests can mint cursors
/// without re-implementing the base64url + msgpack encoding. Not part of the stable API.
#[doc(hidden)]
pub mod testing {
    /// Build an in-memory cursor with the given `(offset, snapshot_id)`, returning the
    /// opaque base64url string an MCP client would receive in `next_cursor`. Used by
    /// the smoke tests to forge stale cursors and verify `cursor_invalidated` plumbing.
    pub fn encode_in_memory_cursor(offset: u64, snapshot_id: u32) -> String {
        crate::mcp::cursor::Cursor::encode_in_memory(offset, snapshot_id).0
    }
}
