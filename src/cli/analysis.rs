//! `basemind code` analysis commands: reports with no verb tool, answered by the daemon by name
//! through `src/mcp/cli_only.rs` (ADR-0018, ADR-0039 rule 4). [`AnalysisCmd`] is flattened into
//! [`super::code::CodeCmd`], so each command is `basemind code <name>`.

use clap::Subcommand;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::analysis_migration::{self, MigrationCmd};
use super::context::CliContext;

#[path = "analysis_cycles.rs"]
mod cycles;
#[path = "analysis_deadcode.rs"]
mod deadcode;
#[path = "analysis_explain.rs"]
mod explain;
#[path = "analysis_impact.rs"]
mod impact;

#[derive(Subcommand, Debug)]
pub enum AnalysisCmd {
    /// Files that depend on a file, on a symbol's definitions, or on a change, through the
    /// file-level import graph (CLI only).
    #[command(after_long_help = crate::cli::help_code::CODE_IMPACT)]
    Impact(impact::ImpactArgs),
    /// Import cycles, smallest first, each with an import chain that closes it; or whether a new
    /// import would close one (CLI only).
    #[command(after_long_help = crate::cli::help_code::CODE_CYCLES)]
    Cycles(cycles::CyclesArgs),
    /// One briefing on a symbol, a file, or the definition at `path:line`: definition, callers,
    /// callees, importers and history, or outline, imports, importers, hotspot and owners (CLI only).
    #[command(after_long_help = crate::cli::help_code::CODE_EXPLAIN)]
    Explain(explain::ExplainArgs),
    /// Files no entry point reaches, symbols nothing else names and imports that resolve to
    /// nothing, each with a confidence tier and its reason (CLI only).
    #[command(after_long_help = crate::cli::help_code::CODE_DEADCODE)]
    Deadcode(deadcode::DeadcodeArgs),
    /// Track a migration away from old forms (`check`).
    #[command(subcommand)]
    Migration(MigrationCmd),
}

/// A check run on a command's result after it is printed, which may fail the process.
pub(super) type Gate = Box<dyn Fn(&CallToolResult) -> anyhow::Result<()>>;

/// Run an analysis command, returning its tool name and result for `code::run` to render.
pub(super) async fn call(tools: &CliContext, cmd: AnalysisCmd) -> (&'static str, Result<CallToolResult, McpError>) {
    match cmd {
        AnalysisCmd::Impact(args) => ("code_impact", impact::call(tools, args).await),
        AnalysisCmd::Cycles(args) => ("code_cycles", cycles::call(tools, args).await),
        AnalysisCmd::Explain(args) => ("code_explain", explain::call(tools, args).await),
        AnalysisCmd::Deadcode(args) => (deadcode::TOOL, deadcode::call(tools, args).await),
        AnalysisCmd::Migration(cmd) => analysis_migration::call(tools, cmd).await,
    }
}

/// The exit check `code::run` applies after printing `cmd`'s result, if it has one.
pub(super) fn gate(cmd: &AnalysisCmd) -> Option<Gate> {
    match cmd {
        AnalysisCmd::Migration(cmd) => analysis_migration::gate(cmd),
        AnalysisCmd::Deadcode(args) if args.fail_on_dead_code => Some(if args.tier.is_some() {
            Box::new(deadcode::fail_on_listed)
        } else {
            Box::new(deadcode::fail_on_high)
        }),
        AnalysisCmd::Cycles(args) if args.fail_on_cycle => Some(if args.strict {
            Box::new(cycles::fail_on_any_cycle)
        } else {
            Box::new(cycles::fail_on_cycle)
        }),
        AnalysisCmd::Impact(args) => args.fail_above.map(impact::fail_above),
        AnalysisCmd::Cycles(_) | AnalysisCmd::Deadcode(_) | AnalysisCmd::Explain(_) => None,
    }
}
