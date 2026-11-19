//! The engine's operations: one synchronous `impl Workspace` method per verb tool, each in its own
//! file. Their arguments and answers are the tool's own types ([`super::types`]).

/// Symbols one `code_symbols` page holds when the caller passes no `limit`.
pub(crate) const SEARCH_LIMIT_DEFAULT: u32 = 100;
/// Upper bound on a `limit` that pages a symbol list.
pub(crate) const SEARCH_LIMIT_MAX: u32 = 1000;

pub(crate) mod code_grep;
mod code_outline;
mod code_read;
mod code_refs;
mod code_search;
mod code_symbols;
mod cycles;
mod docs_search;
pub(crate) mod find_files;
mod graph_report;
mod impact;
pub(crate) mod index;
mod memory;
pub(crate) mod overview;
