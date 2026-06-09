//! Admin / housekeeping tool shims for `BasemindServer`.
//!
//! These are operations that mutate basemind's own on-disk state (index,
//! caches) rather than just querying it. Kept in a separate file so
//! `tools.rs` stays under the 1000-line cap.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;

use super::BasemindServer;
use super::types::RescanParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_admin")]
impl BasemindServer {
    #[tool(
        description = "Refresh basemind's index by running the scanner in-process. \
            Walks the working tree (or only the supplied `paths`), re-parses changed files, \
            updates the Fjall index, and rebuilds the in-RAM map cache. \
            Holds an exclusive lock for the duration of the scan — other MCP queries block \
            until it returns. Cheap on small repos (<1s for ~100 files). Use after editing \
            code when you need new symbols / calls / outlines to show up without restarting \
            the MCP server. Returns scanned / updated / removed counts and elapsed time."
    )]
    async fn rescan(
        &self,
        Parameters(p): Parameters<RescanParams>,
    ) -> Result<CallToolResult, McpError> {
        super::helpers::run_rescan(&self.state, p).await
    }
}
