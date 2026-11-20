//! The memory and document verb tools of `BasemindServer`: `memory_save`, `memory_search` and
//! `docs_search` (ADR-0018).
//!
//! All three are registered in every build; the `memory` and `documents` feature gates are
//! enforced in the engine ops (`engine/ops/{memory,docs_search}.rs`), so a binary without them answers "rebuild with
//! `--features …`" instead of hiding the tool. See `tools.rs` for the shim contract.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;

use super::BasemindServer;
use super::strict::Strict;
use super::types_tools::{DocsSearchParams, MemorySaveParams, MemorySearchToolParams};
use crate::engine::Workspace;
use crate::engine::types::memory::MemoryDeleteParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_memory")]
impl BasemindServer {
    #[tool(
        title = "Search documents",
        description = "Use to find where a topic is discussed in the repository's documents \
        (markdown, PDFs, Office, HTML, email, notebooks) instead of opening them. Ranks passages by \
        meaning and words, filters first, and cites each hit (path, pages, section, lines). Code: \
        `code_grep`/`code_search`. \
        Example: {\"query\":\"how releases are versioned\"}",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn docs_search(
        &self,
        Parameters(args): Parameters<Strict<DocsSearchParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.docs_search_call(p)).await
    }

    #[tool(
        title = "Save a note",
        description = "Use to remember a decision, convention or finding for later sessions and \
        other agents working in this repository. Saving an existing `key` replaces it. \
        Example: {\"key\":\"decision/auth-tokens\",\"value\":\"Tokens rotate every 24h.\",\"tags\":[\"auth\"]}",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn memory_save(
        &self,
        Parameters(args): Parameters<Strict<MemorySaveParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.memory_save_call(p)).await
    }

    #[tool(
        title = "Recall notes",
        description = "Use to recall what was saved with `memory_save`: `query` searches notes by \
        meaning, `key` reads one, `prefix` lists by key prefix, and no arguments lists everything. \
        Lists show 200 characters of a value; `key` reads it whole. \
        Example: {\"query\":\"token rotation\"}",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn memory_search(
        &self,
        Parameters(args): Parameters<Strict<MemorySearchToolParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.memory_search_call(p)).await
    }
}

impl BasemindServer {
    /// Body of the `docs_search` tool, shared with `basemind docs search`.
    pub(crate) async fn docs_search_call(&self, p: DocsSearchParams) -> Result<CallToolResult, McpError> {
        self.op(p, Workspace::docs_search).await
    }

    /// Body of the `memory_save` tool, shared with `basemind memory put`.
    pub(crate) async fn memory_save_call(&self, p: MemorySaveParams) -> Result<CallToolResult, McpError> {
        self.op_unpaged(p, Workspace::memory_save).await
    }

    /// Body of the `memory_search` tool, shared with `basemind memory get|list|search`.
    pub(crate) async fn memory_search_call(&self, p: MemorySearchToolParams) -> Result<CallToolResult, McpError> {
        self.op(p, Workspace::memory_search).await
    }

    /// Delete one note. No MCP tool: `basemind memory delete`.
    pub(crate) async fn memory_delete_call(&self, p: MemoryDeleteParams) -> Result<CallToolResult, McpError> {
        self.cli_op(p, Workspace::memory_delete).await
    }
}
