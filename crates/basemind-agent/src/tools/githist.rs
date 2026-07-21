//! Git-history tools backed by the in-process basemind server.
//!
//! The same context-economy story as [`super::codenav`], applied to history: recent commits,
//! per-symbol blame, and file diffs across revisions come back as structured JSON instead of
//! shelling out to `git` and parsing text. Each tool wraps one `agent_api` facade call.

use std::sync::Arc;

use async_trait::async_trait;
use basemind::mcp::params::{BlameSymbolParams, DiffFileParams, RecentChangesParams};
use basemind::path::RelPath;
use schemars::JsonSchema;
use serde::Deserialize;

use super::codenav::{code_map_err, require_server};
use super::{Tool, ToolCtx, ToolDyn, ToolOutput};
use crate::error::Result;
use crate::permission::PermissionClaim;

/// The read-only git-history tools available to the agent.
pub fn git_history_tools() -> Vec<Arc<dyn ToolDyn>> {
    vec![
        Arc::new(RecentChangesTool),
        Arc::new(BlameSymbolTool),
        Arc::new(DiffFileTool),
    ]
}

/// `git:recent_changes` — recent commits with their file lists.
struct RecentChangesTool;

/// Arguments for [`RecentChangesTool`].
#[derive(Deserialize, JsonSchema)]
struct RecentChangesArgs {
    /// Number of commits to walk back from HEAD (default 20, max 100).
    #[serde(default)]
    limit: Option<u32>,
}

#[async_trait]
impl Tool for RecentChangesTool {
    type Args = RecentChangesArgs;

    fn name(&self) -> &'static str {
        "git:recent_changes"
    }

    fn description(&self) -> &'static str {
        "Recent commits walking back from HEAD, each with author, summary, and the per-file change \
         list. Prefer this over shelling out to `git log`."
    }

    fn permission(&self, _args: &RecentChangesArgs) -> PermissionClaim {
        PermissionClaim::read("git:recent_changes")
    }

    async fn execute(&self, args: RecentChangesArgs, ctx: &ToolCtx) -> Result<ToolOutput> {
        let params = RecentChangesParams {
            limit: args.limit,
            include_files: true,
            cursor: None,
        };
        let server = require_server(ctx, Tool::name(self))?;
        let result = basemind::mcp::agent_api::recent_changes(server, params)
            .await
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        let value = basemind::cli::render::result_to_value(&result)
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        Ok(ToolOutput::ok(value.to_string()))
    }
}

/// `git:blame_symbol` — per-symbol blame hunks.
struct BlameSymbolTool;

/// Arguments for [`BlameSymbolTool`].
#[derive(Deserialize, JsonSchema)]
struct BlameSymbolArgs {
    /// Repository-relative path of the file holding the symbol (forward slashes).
    path: String,
    /// Name of the symbol to blame.
    name: String,
    /// Optional kind filter for resolving the symbol (function/method/class/...).
    #[serde(default)]
    kind: Option<String>,
    /// Optional revision to blame at (defaults to HEAD).
    #[serde(default)]
    rev: Option<String>,
}

#[async_trait]
impl Tool for BlameSymbolTool {
    type Args = BlameSymbolArgs;

    fn name(&self) -> &'static str {
        "git:blame_symbol"
    }

    fn description(&self) -> &'static str {
        "Per-symbol blame: resolves the symbol by path + name + optional kind, then returns the \
         blame hunks over its line span. Prefer this over `git blame` on a whole file."
    }

    fn permission(&self, args: &BlameSymbolArgs) -> PermissionClaim {
        PermissionClaim::read(args.path.clone())
    }

    async fn execute(&self, args: BlameSymbolArgs, ctx: &ToolCtx) -> Result<ToolOutput> {
        let params = BlameSymbolParams {
            path: RelPath::from(args.path.as_str()),
            name: args.name,
            kind: args.kind,
            rev: args.rev,
            limit: None,
            cursor: None,
        };
        let server = require_server(ctx, Tool::name(self))?;
        let result = basemind::mcp::agent_api::blame_symbol(server, params)
            .await
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        let value = basemind::cli::render::result_to_value(&result)
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        Ok(ToolOutput::ok(value.to_string()))
    }
}

/// `git:diff_file` — a file's diff across two revisions.
struct DiffFileTool;

/// Arguments for [`DiffFileTool`].
#[derive(Deserialize, JsonSchema)]
struct DiffFileArgs {
    /// The older revision (sha, ref, or tag).
    rev_old: String,
    /// The newer revision (sha, ref, or tag).
    rev_new: String,
    /// Repository-relative path of the file to diff (forward slashes).
    path: String,
}

#[async_trait]
impl Tool for DiffFileTool {
    type Args = DiffFileArgs;

    fn name(&self) -> &'static str {
        "git:diff_file"
    }

    fn description(&self) -> &'static str {
        "Unified diff of one file across two revisions (`rev_old` → `rev_new`). Prefer this over \
         shelling out to `git diff`."
    }

    fn permission(&self, args: &DiffFileArgs) -> PermissionClaim {
        PermissionClaim::read(args.path.clone())
    }

    async fn execute(&self, args: DiffFileArgs, ctx: &ToolCtx) -> Result<ToolOutput> {
        let params = DiffFileParams {
            rev_old: args.rev_old,
            rev_new: args.rev_new,
            path: RelPath::from(args.path.as_str()),
        };
        let server = require_server(ctx, Tool::name(self))?;
        let result = basemind::mcp::agent_api::diff_file(server, params)
            .await
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        let value = basemind::cli::render::result_to_value(&result)
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        Ok(ToolOutput::ok(value.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_history_tools_are_named() {
        let tools = git_history_tools();
        let names: Vec<_> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["git:recent_changes", "git:blame_symbol", "git:diff_file"]);
    }

    #[test]
    fn recent_changes_claims_a_static_read() {
        let claim = RecentChangesTool.permission_of(r#"{"limit":10}"#).expect("parses");
        assert_eq!(claim, PermissionClaim::read("git:recent_changes"));
    }

    #[test]
    fn blame_symbol_claims_a_read_on_the_path() {
        let claim = BlameSymbolTool
            .permission_of(r#"{"path":"src/lib.rs","name":"scan"}"#)
            .expect("parses");
        assert_eq!(claim, PermissionClaim::read("src/lib.rs"));
    }

    #[test]
    fn diff_file_claims_a_read_on_the_path() {
        let claim = DiffFileTool
            .permission_of(r#"{"rev_old":"HEAD~1","rev_new":"HEAD","path":"src/lib.rs"}"#)
            .expect("parses");
        assert_eq!(claim, PermissionClaim::read("src/lib.rs"));
    }
}
