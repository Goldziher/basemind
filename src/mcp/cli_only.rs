//! The CLI-only commands a CLI session answers by name, off `tools/list` (ADR-0018, ADR-0039 rule
//! 4): the [`CLI_ONLY`] table and [`BasemindServer::cli_only_call`], which dispatches each to its
//! handler. A new CLI-only daemon op adds its name to the table and one match arm.

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, JsonObject};
use serde::de::DeserializeOwned;

use super::BasemindServer;
use crate::engine::Workspace;
use crate::engine::types::deadcode::DeadcodeParams;
use crate::engine::types::git::{FindCommitsByPathParams, HotFilesParams, HotspotsParams};
use crate::engine::types::graph::{CyclesParams, ImpactParams};
use crate::engine::types::memory::MemoryDeleteParams;
use crate::engine::types::migration::MigrationCheckParams;
use crate::engine::types::overview::OverviewParams;
use crate::engine::types::search::GetChunkParams;

/// The CLI-only commands (`git churn`, `git by-path`, `code chunk`, `memory delete`, and the
/// analyses of ADR-0047 such as `code impact`, `code cycles` and `overview`), which ADR-0018 keeps
/// off the agent surface. A CLI session ([`BasemindServer::for_cli`]) answers them by name,
/// off `tools/list`; any other session answers them as unknown tools.
pub(super) const CLI_ONLY: &[&str] = &[
    "git_churn",
    "git_by_path",
    "code_chunk",
    "memory_delete",
    "code_impact",
    "code_cycles",
    "overview",
    "git_hotspots",
    "code_migration_check",
    "code_deadcode",
];

/// `arguments` as the command's params; a mismatch is a usage error, as a verb tool's is.
fn params<P: DeserializeOwned>(name: &str, arguments: Option<JsonObject>) -> Result<P, McpError> {
    serde_json::from_value(serde_json::Value::Object(arguments.unwrap_or_default()))
        .map_err(|error| McpError::invalid_params(format!("`{name}`: {error}"), None))
}

impl BasemindServer {
    /// Answer `name`, one of [`CLI_ONLY`], for a CLI session. Any other session never gets here: the
    /// router answers it as an unknown tool.
    pub(super) async fn cli_only_call(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        debug_assert!(self.cli && CLI_ONLY.contains(&name));
        match name {
            "git_churn" => match params::<HotFilesParams>(name, arguments) {
                Ok(p) => self.cli_op(p, Workspace::git_churn).await,
                Err(error) => Err(error),
            },
            "git_by_path" => match params::<FindCommitsByPathParams>(name, arguments) {
                Ok(p) => self.cli_op(p, Workspace::git_by_path).await,
                Err(error) => Err(error),
            },
            "code_chunk" => match params::<GetChunkParams>(name, arguments) {
                Ok(p) => self.code_chunk_call(p).await,
                Err(error) => Err(error),
            },
            "memory_delete" => match params::<MemoryDeleteParams>(name, arguments) {
                Ok(p) => self.memory_delete_call(p).await,
                Err(error) => Err(error),
            },
            "code_impact" => match params::<ImpactParams>(name, arguments) {
                Ok(p) => self.cli_op(p, Workspace::code_impact).await,
                Err(error) => Err(error),
            },
            "code_cycles" => match params::<CyclesParams>(name, arguments) {
                Ok(p) => self.cli_op(p, Workspace::code_cycles).await,
                Err(error) => Err(error),
            },
            "overview" => match params::<OverviewParams>(name, arguments) {
                Ok(p) => self.cli_op(p, Workspace::overview).await,
                Err(error) => Err(error),
            },
            "git_hotspots" => match params::<HotspotsParams>(name, arguments) {
                Ok(p) => self.cli_op(p, Workspace::git_hotspots).await,
                Err(error) => Err(error),
            },
            "code_migration_check" => match params::<MigrationCheckParams>(name, arguments) {
                Ok(p) => self.cli_op(p, Workspace::code_migration_check).await,
                Err(error) => Err(error),
            },
            "code_deadcode" => match params::<DeadcodeParams>(name, arguments) {
                Ok(p) => self.cli_op(p, Workspace::code_deadcode).await,
                Err(error) => Err(error),
            },
            _ => Err(McpError::invalid_params(
                format!("`{name}` is not a CLI-only command"),
                None,
            )),
        }
    }
}
