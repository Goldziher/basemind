//! The synchronous engine: one [`Workspace`] per served `(root, view)` and the caches it reads
//! through. Nothing here names rmcp, clap or tokio types (ADR-0039); the MCP adapter and the CLI
//! reach it from `src/mcp`.

pub(crate) mod l1_cache;
pub(crate) mod map_cache;
pub(crate) mod outline_cache;
pub(crate) mod ranked_cache;
pub(crate) mod term_index;
pub(crate) mod workspace;

pub(crate) use map_cache::MapCache;
pub(crate) use workspace::{Lifecycle, SharedReadStack};
