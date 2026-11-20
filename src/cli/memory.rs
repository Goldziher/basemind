//! `basemind memory` — the CLI half of `memory_save` and `memory_search`.
//!
//! `put` runs the body of `memory_save` and `search` / `get` / `list` the body of `memory_search`,
//! which is what `tests/cli_parity/` asserts. `delete` and `clear` have no MCP tool. The hidden
//! `documents` is `basemind docs search` ([`super::docs`]) under its earlier name.
//!
//! The tools are registered in every build and gate in the body, so these handlers always compile
//! and a command whose feature is missing surfaces the tool's own "rebuild with --features …"
//! error.

use std::io::{IsTerminal, Write};
use std::path::Path;

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
        /// Keep notes carrying any of these tags (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long)]
        individual: bool,
    },
    /// Read a single memory entry by key. Shorthand for one shape of `memory_search`.
    Get {
        key: String,
        #[arg(long)]
        individual: bool,
    },
    /// List memory entries by key prefix and/or tags. Shorthand for one shape of `memory_search`.
    List {
        #[arg(long)]
        prefix: Option<String>,
        /// Keep notes carrying any of these tags (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
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
    /// Remove every memory note of this repository, of every tier and agent, and their vectors (CLI
    /// only). Asks for confirmation on a terminal and requires `--yes` otherwise.
    Clear {
        /// Skip the confirmation prompt (required when stdin is not a terminal).
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// `basemind docs search`, under the name it had before `docs` existed.
    #[command(hide = true)]
    Documents(super::docs::DocsSearchArgs),
}

/// A `memory_search` call with no selector set.
fn recall(individual: bool) -> MemorySearchToolParams {
    MemorySearchToolParams {
        query: None,
        key: None,
        prefix: None,
        tags: None,
        visibility: visibility(individual),
        limit: None,
        cursor: None,
    }
}

/// What `basemind memory clear` asks to confirm for the repository at `root`.
fn clear_prompt(root: &Path) -> String {
    format!(
        "remove every memory note of this repository, of every tier and agent, and their vectors ({})",
        crate::store_layout::repo_group_dir(root).display()
    )
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
            tags,
            individual,
        } => {
            let p = MemorySearchToolParams {
                query: Some(query),
                limit,
                tags: (!tags.is_empty()).then_some(tags),
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
            tags,
            limit,
            individual,
            cursor,
        } => {
            let p = MemorySearchToolParams {
                prefix,
                tags: (!tags.is_empty()).then_some(tags),
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
        MemoryCmd::Clear { yes } => {
            if !yes {
                super::admin::confirm(&clear_prompt(tools.root()), std::io::stdin().is_terminal())?;
            }
            (
                "memory_clear",
                tools
                    .call("memory_clear", MemoryClearParams {}, async |s, p| {
                        s.memory_clear_call(p).await
                    })
                    .await,
            )
        }
        MemoryCmd::Documents(args) => return super::docs::search(tools, args, opts, out).await,
    };

    let r = run_tool(tool, result)?;
    emit(tool, &r, opts, out)
}
