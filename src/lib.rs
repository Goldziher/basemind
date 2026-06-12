// Prototype: rustdoc lives in the code itself and in README.md rather than per-symbol docstrings.
// Flip this off once the public API is frozen.
#![allow(missing_docs)]

pub mod config;
#[cfg(feature = "intelligence")]
pub mod embeddings;
pub mod extract;
pub mod git;
pub mod git_cache;
pub mod hashing;
pub mod index;
#[cfg(feature = "intelligence")]
pub mod lance;
pub mod lang;
pub mod mcp;
pub mod path;
pub mod query;
pub mod render;
pub mod scanner;
#[cfg(feature = "documents")]
pub mod scanner_docs;
pub mod store;
#[cfg(feature = "crawl")]
pub mod url;
pub mod version;
pub mod watcher;
#[cfg(feature = "crawl")]
pub mod web;

pub use config::Config;
