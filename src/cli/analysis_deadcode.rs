//! `basemind code deadcode` (#123): unreachable files, unreferenced symbols and unresolved imports,
//! each with a confidence tier and its reason. Engine op: `Workspace::code_deadcode`, answered by
//! the daemon by name as `code_deadcode`.

use clap::Args;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use crate::engine::Workspace;
use crate::engine::types::deadcode::DeadcodeParams;

use super::super::context::Tools;
use super::super::exit::{CliExit, ERROR};
use super::super::resolve_path;

/// The CLI-only command name the daemon answers.
pub(super) const TOOL: &str = "code_deadcode";

#[derive(Args, Debug)]
pub struct DeadcodeArgs {
    /// Only findings in a file at this path or under this directory.
    #[arg(long)]
    path: Option<String>,
    /// One kind of finding.
    #[arg(long, value_parser = ["files", "symbols", "imports"])]
    kind: Option<String>,
    /// The lowest confidence reported: `high` lists only the surest findings, `low` (the default)
    /// every one.
    #[arg(long, value_parser = ["high", "medium", "low"])]
    tier: Option<String>,
    /// Findings listed per kind (default 100, at most 10000); the counts cover the rest.
    #[arg(long)]
    limit: Option<u32>,
    /// Exit 1 when any finding is reported, for CI.
    #[arg(long)]
    pub(super) fail_on_dead_code: bool,
}

pub(super) async fn call(tools: &Tools, args: DeadcodeArgs) -> Result<CallToolResult, McpError> {
    let p = DeadcodeParams {
        path: args.path.map(|path| resolve_path(tools, &path).to_string()),
        kind: args.kind,
        tier: args.tier,
        limit: args.limit,
    };
    tools
        .call(TOOL, p, async |s, p| s.cli_op(p, Workspace::code_deadcode).await)
        .await
}

/// `--fail-on-dead-code`: exit 1 when the report counts a finding.
pub(super) fn fail_on_dead_code(result: &CallToolResult) -> anyhow::Result<()> {
    let report = super::super::render::result_to_value(result)?;
    let total: u64 = report["counts"]
        .as_object()
        .into_iter()
        .flat_map(|counts| counts.values())
        .flat_map(|tiers| ["high", "medium", "low"].map(|tier| tiers[tier].as_u64().unwrap_or(0)))
        .sum();
    if total == 0 {
        return Ok(());
    }
    Err(CliExit::with_code(
        ERROR,
        format!("deadcode: {total} finding(s) at or above the requested tier"),
    ))
}
