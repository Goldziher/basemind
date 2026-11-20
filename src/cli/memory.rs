//! `basemind memory` — the CLI half of `memory_save`, `memory_search` and `docs_search`.
//!
//! `put` runs the body of `memory_save`, `search` / `get` / `list` the body of `memory_search`, and
//! `documents` the body of `docs_search`, which is what `tests/cli_parity/` asserts. `delete` has
//! no MCP tool.
//!
//! The tools are registered in every build and gate in the body, so these handlers always compile
//! and a command whose feature is missing surfaces the tool's own "rebuild with --features …"
//! error.

use std::io::Write;

use anyhow::Result;
use clap::Subcommand;

use super::context::Tools;
use crate::mcp::cursor::Cursor;
use crate::mcp::params::*;

use super::render::{Emit, emit};
use super::run_tool;

/// The memory tier a `--individual` flag selects (`None` → the default group tier).
fn visibility(individual: bool) -> Option<Visibility> {
    individual.then_some(Visibility::Individual)
}

#[derive(Subcommand, Debug)]
pub enum MemoryCmd {
    /// Store a key/value memory entry.
    Put {
        key: String,
        value: String,
        /// Tag for filtered recall (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Store in the per-agent individual tier instead of the shared group tier.
        #[arg(long)]
        individual: bool,
    },
    /// Semantic search over memory.
    Search {
        query: String,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        individual: bool,
    },
    /// Read a single memory entry by key. Shorthand for one shape of `memory_search`.
    Get {
        key: String,
        #[arg(long)]
        individual: bool,
    },
    /// List memory entries by key prefix and/or tag. Shorthand for one shape of `memory_search`.
    List {
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        individual: bool,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Delete a memory entry by key (CLI only).
    Delete {
        key: String,
        #[arg(long)]
        individual: bool,
    },
    /// Semantic search over indexed documents (needs `--features documents`).
    Documents {
        query: String,
        /// Exact MIME filter, e.g. `application/pdf`.
        #[arg(long)]
        mime_type: Option<String>,
        /// Format family: pdf, office, html, markdown, email, notebook, text, other.
        #[arg(long)]
        kind: Option<String>,
        /// Repository-relative prefix, or a glob when it holds `*` (`docs/`, `docs/**/*.md`).
        #[arg(long)]
        path: Option<String>,
        /// Keep documents with any of these whole keywords (comma-separated).
        #[arg(long)]
        keywords: Option<String>,
        /// Keep chunks naming an entity of this category.
        #[arg(long)]
        entity: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
}

/// A `memory_search` call with no selector set.
fn recall(individual: bool) -> MemorySearchToolParams {
    MemorySearchToolParams {
        query: None,
        key: None,
        prefix: None,
        tag: None,
        visibility: visibility(individual),
        limit: None,
        cursor: None,
    }
}

/// Dispatch a `memory` subcommand through the in-process server.
pub async fn run(tools: &Tools, cmd: MemoryCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let (tool, result) = match cmd {
        MemoryCmd::Put {
            key,
            value,
            tags,
            individual,
        } => {
            let p = MemorySaveParams {
                key,
                value,
                tags: (!tags.is_empty()).then_some(tags),
                visibility: visibility(individual),
            };
            (
                "memory_save",
                tools
                    .call("memory_save", p, async |s, p| s.memory_save_call(p).await)
                    .await,
            )
        }
        MemoryCmd::Search {
            query,
            limit,
            tag,
            individual,
        } => {
            let p = MemorySearchToolParams {
                query: Some(query),
                limit,
                tag,
                ..recall(individual)
            };
            (
                "memory_search",
                tools
                    .call("memory_search", p, async |s, p| s.memory_search_call(p).await)
                    .await,
            )
        }
        MemoryCmd::Get { key, individual } => {
            let p = MemorySearchToolParams {
                key: Some(key),
                ..recall(individual)
            };
            (
                "memory_search",
                tools
                    .call("memory_search", p, async |s, p| s.memory_search_call(p).await)
                    .await,
            )
        }
        MemoryCmd::List {
            prefix,
            tag,
            limit,
            individual,
            cursor,
        } => {
            let p = MemorySearchToolParams {
                prefix,
                tag,
                limit,
                cursor: cursor.map(Cursor),
                ..recall(individual)
            };
            (
                "memory_search",
                tools
                    .call("memory_search", p, async |s, p| s.memory_search_call(p).await)
                    .await,
            )
        }
        MemoryCmd::Delete { key, individual } => {
            let p = MemoryDeleteParams {
                key,
                visibility: visibility(individual).unwrap_or_default(),
            };
            (
                "memory_delete",
                tools
                    .call("memory_delete", p, async |s, p| s.memory_delete_call(p).await)
                    .await,
            )
        }
        MemoryCmd::Documents {
            query,
            mime_type,
            kind,
            path,
            keywords,
            entity,
            limit,
            cursor,
            max_tokens,
        } => {
            let p = DocsSearchParams {
                query,
                mime_type,
                kind,
                path,
                keywords,
                entity,
                limit,
                cursor: cursor.map(Cursor),
                max_tokens,
            };
            (
                "docs_search",
                tools
                    .call("docs_search", p, async |s, p| s.docs_search_call(p).await)
                    .await,
            )
        }
    };

    let r = run_tool(tool, result)?;
    emit(tool, &r, opts, out)
}
