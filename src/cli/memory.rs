//! Memory + document-search subcommands.
//!
//! The MCP `memory_*` methods exist on the server regardless of the `memory`
//! feature (they return a "feature not enabled" error when the feature is off),
//! so these handlers always compile and dispatch identically. `search-documents`
//! likewise surfaces the documents-feature gate via the tool's own error.

use std::io::Write;

use anyhow::Result;
use clap::Subcommand;

use crate::mcp::BasemindServer;
use crate::mcp::params::*;

use super::render::emit;
use super::run_tool;

/// Map the `--individual` CLI flag onto a [`Visibility`]. Absent = group (shared) tier.
fn visibility(individual: bool) -> Visibility {
    if individual {
        Visibility::Individual
    } else {
        Visibility::Group
    }
}

#[derive(Subcommand, Debug)]
pub enum MemoryCmd {
    /// Persist a key-value pair in scoped memory.
    Put {
        key: String,
        value: String,
        #[arg(long)]
        tag: Vec<String>,
        /// Disable embedding into LanceDB (skips memory_search indexing).
        #[arg(long)]
        no_embed: bool,
        /// Use the per-agent (individual) memory tier instead of shared (group).
        #[arg(long)]
        individual: bool,
    },
    /// Exact-key lookup.
    Get {
        key: String,
        /// Look up in the per-agent (individual) tier instead of shared (group).
        #[arg(long)]
        individual: bool,
    },
    /// List scoped memory entries.
    List {
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        /// List the per-agent (individual) tier instead of shared (group).
        #[arg(long)]
        individual: bool,
    },
    /// Vector KNN search over stored memory.
    Search {
        query: String,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        tag: Option<String>,
        /// Search the per-agent (individual) tier instead of shared (group).
        #[arg(long)]
        individual: bool,
    },
    /// Delete a memory entry by exact key.
    Delete {
        key: String,
        /// Delete from the per-agent (individual) tier instead of shared (group).
        #[arg(long)]
        individual: bool,
    },
    /// Semantic search over indexed document chunks.
    SearchDocuments {
        query: String,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        mime_type: Option<String>,
    },
}

pub async fn run(server: &BasemindServer, cmd: MemoryCmd, json: bool, out: &mut impl Write) -> Result<()> {
    match cmd {
        MemoryCmd::Put {
            key,
            value,
            tag,
            no_embed,
            individual,
        } => {
            let p = MemoryPutParams {
                key,
                value,
                tags: if tag.is_empty() { None } else { Some(tag) },
                embed: !no_embed,
                visibility: visibility(individual),
            };
            let r = run_tool("memory_put", server.memory_put(Parameters(p)).await)?;
            emit("memory_put", &r, json, out)
        }
        MemoryCmd::Get { key, individual } => {
            let p = MemoryGetParams {
                key,
                visibility: visibility(individual),
            };
            let r = run_tool("memory_get", server.memory_get(Parameters(p)).await)?;
            emit("memory_get", &r, json, out)
        }
        MemoryCmd::List {
            prefix,
            tag,
            limit,
            individual,
        } => {
            let p = MemoryListParams {
                prefix,
                tag,
                limit,
                cursor: None,
                visibility: visibility(individual),
            };
            let r = run_tool("memory_list", server.memory_list(Parameters(p)).await)?;
            emit("memory_list", &r, json, out)
        }
        MemoryCmd::Search {
            query,
            limit,
            tag,
            individual,
        } => {
            let p = MemorySearchParams {
                query,
                limit,
                tag,
                visibility: visibility(individual),
            };
            let r = run_tool("memory_search", server.memory_search(Parameters(p)).await)?;
            emit("memory_search", &r, json, out)
        }
        MemoryCmd::Delete { key, individual } => {
            let p = MemoryDeleteParams {
                key,
                visibility: visibility(individual),
            };
            let r = run_tool("memory_delete", server.memory_delete(Parameters(p)).await)?;
            emit("memory_delete", &r, json, out)
        }
        MemoryCmd::SearchDocuments {
            query,
            limit,
            mime_type,
        } => {
            let p = SearchDocumentsParams {
                query,
                limit,
                max_tokens: None,
                format: None,
                mime_type,
                entity_category: None,
                keywords_contains: None,
                overrides: Default::default(),
            };
            let r = run_tool(
                "search_documents",
                server.search_documents(Parameters(Lenient(p))).await,
            )?;
            emit("search_documents", &r, json, out)
        }
    }
}
