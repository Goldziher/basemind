//! The one Lance session every [`super::LanceStore`] of the process connects through.
//!
//! A `lancedb::connect` without a session builds its own with Lance's defaults: a 6 GiB index
//! cache and a 1 GiB metadata cache. Every commit puts the new version's manifest, transaction and
//! data-file metadata into the metadata cache, and versions a prune has removed from disk stay
//! there until the budget evicts them, so a long-lived store under steady writes climbs toward
//! 1 GiB. The daemon holds several stores at once (the memory vectors, the workspace vector store
//! and the document store, per workspace), each with its own budget. A soak of the daemon grew its
//! heap by about 4 MiB/min in Lance cache entries alone, written from `record_successful_commit`
//! and the manifest and file-metadata loads after it.
//!
//! One session with small budgets bounds all of it together. The metadata a query needs is the
//! current version's manifest and the metadata of its live data files and indices; the index
//! cache holds scalar and vector index pages, which a miss reads back from local disk.

use std::sync::{Arc, OnceLock};

use lancedb::{ObjectStoreRegistry, Session};

/// Budget of the index cache shared by every store of the process.
pub const INDEX_CACHE_BYTES: usize = 64 << 20;

/// Budget of the metadata cache shared by every store of the process.
pub const METADATA_CACHE_BYTES: usize = 32 << 20;

/// The process's session, built on first use.
pub fn shared() -> Arc<Session> {
    static SESSION: OnceLock<Arc<Session>> = OnceLock::new();
    Arc::clone(SESSION.get_or_init(|| bounded(INDEX_CACHE_BYTES, METADATA_CACHE_BYTES)))
}

/// A session whose caches hold at most `index_bytes` and `metadata_bytes`.
pub fn bounded(index_bytes: usize, metadata_bytes: usize) -> Arc<Session> {
    Arc::new(Session::new(
        index_bytes,
        metadata_bytes,
        Arc::new(ObjectStoreRegistry::default()),
    ))
}
