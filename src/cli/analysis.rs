//! `basemind code` analysis commands: reports with no verb tool, answered by the daemon by name
//! through `src/mcp/cli_only.rs` (ADR-0018, ADR-0039 rule 4). [`AnalysisCmd`] is flattened into
//! [`super::code::CodeCmd`], so each command is `basemind code <name>`; it has none yet.

use clap::Subcommand;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::context::Tools;

#[derive(Subcommand, Debug)]
pub enum AnalysisCmd {}

/// Run an analysis command, returning its tool name and result for `code::run` to render.
pub(super) async fn call(_tools: &Tools, cmd: AnalysisCmd) -> (&'static str, Result<CallToolResult, McpError>) {
    match cmd {}
}
