//! The synchronous engine: one `Workspace` per served `(root, view)`, the caches it reads
//! through, and its operations, each failing with an [`EngineError`]. Nothing here names rmcp, clap
//! or tokio types (ADR-0039); the MCP adapter and the CLI reach it from the `basemind` crate.

pub mod budget;
pub mod by_path;
pub mod changed;
mod code_search;
pub mod cursor;
pub mod definition;
pub mod error;
pub mod error_kind;
pub mod files;
pub mod filters;
pub mod freshness;
pub mod git;
pub mod grep_text;
mod handle;
#[cfg_attr(
    not(all(test, any(feature = "code-intel-js", feature = "code-intel-stack"))),
    allow(dead_code, reason = "the import graph's commands and arguments land in later waves")
)]
pub mod import_graph;
#[cfg(test)]
mod index_view_tests;
pub mod l1_cache;
pub mod l1_shared;
#[cfg(any(feature = "memory", feature = "code-search"))]
pub mod lance_cell;
pub mod manifests;
pub mod map_cache;
#[cfg(feature = "memory")]
pub mod memory;
pub mod migration_history;
pub mod migration_rules;
pub mod ops;
pub mod outline_cache;
pub mod paged_map;
pub mod paging;
pub mod pick;
pub mod ranges;
pub mod ranked;
pub mod ranked_cache;
pub mod redaction;
pub mod refs;
mod retrieval;
pub mod savings;
pub mod scan;
pub mod scan_queue;
pub mod scope;
pub mod stale;
pub mod telemetry;
pub mod term_index;
pub mod timing;
pub mod tokens;
pub mod tool_names;
pub mod types;
pub mod workspace;

pub use crate::config::engine::EngineConfig;
pub use crate::data_root::DataRoot;
pub use error::{BUSY_DATA, EngineError};
pub use handle::Engine;
pub use map_cache::MapCache;
pub use workspace::{Lifecycle, Workspace};
