//! The memory and document verb tools of `BasemindServer`: `memory_save`, `memory_search` and
//! `docs_search` (ADR-0018).
//!
//! All three are registered in every build; the `memory` and `documents` feature gates are
//! enforced in the bodies (`helpers_memory.rs`), so a binary without them answers "rebuild with
//! `--features …`" instead of hiding the tool. See `tools.rs` for the shim contract.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;

use super::BasemindServer;
use super::helpers_memory;
use super::strict::Strict;
use super::types_memory::MemoryDeleteParams;
use super::types_tools::{DocsSearchParams, MemorySaveParams, MemorySearchToolParams};

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_memory")]
impl BasemindServer {
    #[tool(
        title = "Search documents",
        description = "Use to find where a topic is discussed in the repository's documents \
        (markdown, PDFs, Office, HTML, config and data files) instead of opening them. Returns the \
        best-matching passages with their path. Code is searched with `code_grep`/`code_search`. \
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
    /// Body of `docs_search`, shared with `basemind memory documents`.
    pub(crate) async fn docs_search_call(&self, p: DocsSearchParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "docs_search",
            p,
            helpers_memory::docs_search(&self.state, p).await
        )
    }

    /// Body of `memory_save`, shared with `basemind memory put`.
    pub(crate) async fn memory_save_call(&self, p: MemorySaveParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "memory_save",
            p,
            helpers_memory::memory_save(&self.state, p).await
        )
    }

    /// Body of `memory_search`, shared with `basemind memory get|list|search`.
    pub(crate) async fn memory_search_call(&self, p: MemorySearchToolParams) -> Result<CallToolResult, McpError> {
        crate::instrument_tool!(
            &self.state,
            "memory_search",
            p,
            helpers_memory::memory_search(&self.state, p).await
        )
    }

    /// Delete one note. No MCP tool: `basemind memory delete`.
    pub(crate) async fn memory_delete_call(&self, p: MemoryDeleteParams) -> Result<CallToolResult, McpError> {
        helpers_memory::memory_delete(&self.state, p).await
    }
}
