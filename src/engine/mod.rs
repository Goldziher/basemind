//! The synchronous engine: one `Workspace` per served `(root, view)`, the caches it reads
//! through, and its operations, each failing with an [`EngineError`]. Nothing here names rmcp, clap
//! or tokio types (ADR-0039); the MCP adapter and the CLI reach it from `src/mcp`.

pub(crate) mod budget;
pub(crate) mod by_path;
pub(crate) mod changed;
mod code_search;
pub(crate) mod content_pools;
pub(crate) mod cursor;
pub(crate) mod definition;
pub(crate) mod error;
pub(crate) mod error_kind;
pub(crate) mod files;
pub(crate) mod filters;
pub(crate) mod freshness;
pub(crate) mod git;
pub(crate) mod grep_text;
mod handle;
#[cfg_attr(
    not(all(test, any(feature = "code-intel-js", feature = "code-intel-stack"))),
    allow(dead_code, reason = "the import graph's commands and arguments land in later waves")
)]
pub(crate) mod import_graph;
pub(crate) mod l1_cache;
pub(crate) mod l1_shared;
#[cfg(any(feature = "memory", feature = "code-search"))]
pub(crate) mod lance_cell;
pub(crate) mod manifests;
pub(crate) mod map_cache;
#[cfg(feature = "memory")]
pub(crate) mod memory;
pub(crate) mod migration_history;
pub(crate) mod migration_rules;
pub(crate) mod ops;
pub(crate) mod outline_cache;
pub(crate) mod paged_map;
pub(crate) mod pick;
pub(crate) mod ranges;
pub(crate) mod ranked;
pub(crate) mod ranked_cache;
pub(crate) mod redaction;
pub(crate) mod refs;
mod retrieval;
pub(crate) mod scan;
pub(crate) mod scan_queue;
pub(crate) mod scope;
pub(crate) mod stale;
pub(crate) mod term_index;
pub(crate) mod timing;
pub(crate) mod tokens;
pub(crate) mod types;
pub(crate) mod workspace;

pub use crate::config::engine::EngineConfig;
pub use crate::data_root::DataRoot;
pub use error::EngineError;
pub use handle::Engine;
pub(crate) use map_cache::MapCache;
pub(crate) use workspace::{Lifecycle, Workspace};
