// Prototype: rustdoc lives in the code itself and in README.md rather than per-symbol docstrings.
// Flip this off once the public API is frozen.
#![allow(missing_docs)]

pub mod config;
pub mod extract;
pub mod git;
pub mod git_cache;
pub mod hashing;
pub mod index;
pub mod lang;
pub mod mcp;
pub mod path;
pub mod query;
pub mod render;
pub mod scanner;
pub mod store;
pub mod watcher;

pub use config::Config;
