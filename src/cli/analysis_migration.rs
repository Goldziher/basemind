//! `basemind code migration check [<name>]` (#130): the rules of `.basemind/migrations/<name>.toml`
//! checked against the index ([`crate::engine::Workspace::code_migration_check`]), answered by the
//! daemon by name as `code_migration_check`.

use clap::Subcommand;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::context::Tools;
use super::exit::{CliExit, ERROR};
use crate::engine::Workspace;
use crate::engine::types::migration::MigrationCheckParams;

/// The CLI-only command name the daemon answers.
pub(super) const TOOL: &str = "code_migration_check";

#[derive(Subcommand, Debug)]
pub enum MigrationCmd {
    /// Remaining uses of the old forms a migration's rules name, per rule and file, and its
    /// progress across checks (CLI only). Rules live in `.basemind/migrations/<name>.toml`; each
    /// run is recorded in the workspace cache directory, never the repository.
    #[command(after_long_help = crate::cli::help_code::CODE_MIGRATION_CHECK)]
    Check {
        /// The migration (`.basemind/migrations/<name>.toml`); every rule file when omitted.
        name: Option<String>,
        /// Exit 1 when an old form remains or an `expect` found nothing, for CI.
        #[arg(long)]
        fail_on_remaining: bool,
        /// Give each file's remaining uses as merged line spans, each one `code read --line --end-line`.
        #[arg(long)]
        ranges: bool,
    },
}

/// Run a migration command, returning its tool name and result.
pub(super) async fn call(tools: &Tools, cmd: MigrationCmd) -> (&'static str, Result<CallToolResult, McpError>) {
    match cmd {
        MigrationCmd::Check { name, ranges, .. } => {
            let params = MigrationCheckParams {
                name,
                ranges: ranges.then_some(true),
            };
            let result = tools
                .call(TOOL, params, async |s, p| {
                    s.cli_op(p, Workspace::code_migration_check).await
                })
                .await;
            (TOOL, result)
        }
    }
}

/// The exit check of `cmd` after its report is printed: `--fail-on-remaining`'s, when given.
pub(super) fn gate(cmd: &MigrationCmd) -> Option<super::analysis::Gate> {
    match cmd {
        MigrationCmd::Check {
            fail_on_remaining: true,
            ..
        } => Some(Box::new(fail_on_remaining)),
        MigrationCmd::Check { .. } => None,
    }
}

/// `--fail-on-remaining`: exit 1 when the report counts a remaining use or a failed `expect`.
fn fail_on_remaining(result: &CallToolResult) -> anyhow::Result<()> {
    let report = super::render::result_to_value(result)?;
    let migrations = report["migrations"].as_array().map(Vec::as_slice).unwrap_or_default();
    let remaining: u64 = migrations.iter().filter_map(|m| m["remaining"].as_u64()).sum();
    let failed: u64 = migrations.iter().filter_map(|m| m["failed_rules"].as_u64()).sum();
    if remaining == 0 && failed == 0 {
        return Ok(());
    }
    Err(CliExit::with_code(
        ERROR,
        format!("migration check: {remaining} line(s) still hold an old form; {failed} rule(s) found no `expect`"),
    ))
}
