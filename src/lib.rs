#![allow(missing_docs)]

#[cfg(feature = "code-search")]
pub mod chunk;
pub mod cli;
pub mod comms;
pub mod config;
#[cfg(feature = "intelligence")]
pub mod embeddings;
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
pub mod mcp;
pub mod path;
pub mod query;
pub mod render;
pub mod scanner;
#[cfg(feature = "code-search")]
pub mod scanner_code;
#[cfg(feature = "documents")]
pub mod scanner_docs;
pub(crate) mod scanner_filter;
pub mod search;
#[cfg(all(feature = "shells", any(unix, windows)))]
pub mod shells;
pub mod store;
pub mod store_blob;
pub mod store_gc;
mod store_lock;
pub mod sysres;
pub mod textcompress;
#[cfg(feature = "crawl")]
pub mod url;
pub mod version;
pub mod watcher;
#[cfg(feature = "crawl")]
pub mod web;

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
