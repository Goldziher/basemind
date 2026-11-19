//! `basemind code` analysis commands: reports with no verb tool, answered by the daemon by name
//! through `src/mcp/cli_only.rs` (ADR-0018, ADR-0039 rule 4). [`AnalysisCmd`] is flattened into
//! [`super::code::CodeCmd`], so each command is `basemind code <name>`.

use clap::Subcommand;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::analysis_migration::{self, MigrationCmd};
use super::context::Tools;

#[path = "analysis_cycles.rs"]
mod cycles;
#[path = "analysis_explain.rs"]
mod explain;
#[path = "analysis_impact.rs"]
mod impact;

#[derive(Subcommand, Debug)]
pub enum AnalysisCmd {
    /// Files that depend on a file, on a symbol's definitions, or on a change, through the
    /// file-level import graph (CLI only).
    Impact(impact::ImpactArgs),
    /// Import cycles, smallest first, each with an import chain that closes it; or whether a new
    /// import would close one (CLI only).
    Cycles(cycles::CyclesArgs),
    /// One briefing on a symbol, a file, or the definition at `path:line`: definition, callers,
    /// callees, importers and history, or outline, imports, importers, hotspot and owners (CLI only).
    Explain(explain::ExplainArgs),
    /// Track a migration away from old forms (`check`).
    #[command(subcommand)]
    Migration(MigrationCmd),
}

/// A check run on a command's result after it is printed, which may fail the process.
pub(super) type Gate = fn(&CallToolResult) -> anyhow::Result<()>;

/// Run an analysis command, returning its tool name and result for `code::run` to render.
pub(super) async fn call(tools: &Tools, cmd: AnalysisCmd) -> (&'static str, Result<CallToolResult, McpError>) {
    match cmd {
        AnalysisCmd::Impact(args) => ("code_impact", impact::call(tools, args).await),
        AnalysisCmd::Cycles(args) => ("code_cycles", cycles::call(tools, args).await),
        AnalysisCmd::Explain(args) => ("code_explain", explain::call(tools, args).await),
        AnalysisCmd::Migration(cmd) => analysis_migration::call(tools, cmd).await,
    }
}

/// The exit check `code::run` applies after printing `cmd`'s result, if it has one.
pub(super) fn gate(cmd: &AnalysisCmd) -> Option<Gate> {
    match cmd {
        AnalysisCmd::Migration(cmd) => analysis_migration::gate(cmd),
        AnalysisCmd::Impact(_) | AnalysisCmd::Cycles(_) | AnalysisCmd::Explain(_) => None,
    }
}
