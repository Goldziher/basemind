//! Code-navigation tools backed by the in-process basemind server.
//!
//! These are the differentiator: instead of grepping and reading whole files, the agent asks the
//! code map structural questions and gets back paths + line ranges + signatures at a fraction of
//! the tokens. Each tool wraps one `agent_api` facade call and returns its JSON verbatim.

use std::sync::Arc;

use async_trait::async_trait;
use basemind::mcp::BasemindServer;
use basemind::mcp::params::{
    CallGraphParams, FindCallersParams, FindReferencesParams, OutlineParams, SearchSymbolsParams, WorkspaceGrepParams,
};
use basemind::path::RelPath;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Tool, ToolCtx, ToolDyn, ToolOutput};
use crate::error::{AgentError, Result};
use crate::permission::PermissionClaim;

/// The read-only code-navigation tools available to the agent.
pub fn code_nav_tools() -> Vec<Arc<dyn ToolDyn>> {
    vec![
        Arc::new(OutlineTool),
        Arc::new(SearchSymbolsTool),
        Arc::new(FindReferencesTool),
        Arc::new(FindCallersTool),
        Arc::new(CallGraphTool),
        Arc::new(WorkspaceGrepTool),
    ]
}

pub(super) fn code_map_err(tool: &'static str) -> impl Fn(String) -> AgentError {
    move |message| AgentError::CodeMap { tool, message }
}

/// The basemind server from the context, or a clean code-map error if none is wired.
pub(super) fn require_server<'a>(ctx: &'a ToolCtx, tool: &'static str) -> Result<&'a BasemindServer> {
    ctx.server.as_deref().ok_or_else(|| AgentError::CodeMap {
        tool,
        message: "code map unavailable (no index for this workspace)".into(),
    })
}

/// `code:outline` — structural outline of a file.
struct OutlineTool;

/// Arguments for [`OutlineTool`].
#[derive(Deserialize, JsonSchema)]
struct OutlineArgs {
    /// Repository-relative path to outline (forward slashes).
    path: String,
}

#[async_trait]
impl Tool for OutlineTool {
    type Args = OutlineArgs;

    fn name(&self) -> &'static str {
        "code:outline"
    }

    fn description(&self) -> &'static str {
        "Structural outline of a file: every symbol (name, kind, start line/col) plus imports, \
         without reading the file body. Prefer this over reading a whole file to learn its shape."
    }

    fn permission(&self, args: &OutlineArgs) -> PermissionClaim {
        PermissionClaim::read(args.path.clone())
    }

    async fn execute(&self, args: OutlineArgs, ctx: &ToolCtx) -> Result<ToolOutput> {
        let params = OutlineParams {
            path: RelPath::from(args.path.as_str()),
            l2: false,
            max_tokens: None,
            format: None,
        };
        let server = require_server(ctx, Tool::name(self))?;
        let result = basemind::mcp::agent_api::outline(server, params)
            .await
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        let value = basemind::cli::render::result_to_value(&result)
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        Ok(ToolOutput::ok(value.to_string()))
    }
}

/// `code:search_symbols` — substring search over indexed symbol names.
struct SearchSymbolsTool;

/// Arguments for [`SearchSymbolsTool`].
#[derive(Deserialize, JsonSchema)]
struct SearchSymbolsArgs {
    /// Substring to match against symbol names (case-sensitive).
    needle: String,
    /// Optional kind filter (function/method/struct/enum/class/interface/trait/type/const/module/macro).
    #[serde(default)]
    kind: Option<String>,
    /// Cap on results (default 100, max 1000).
    #[serde(default)]
    limit: Option<u32>,
}

#[async_trait]
impl Tool for SearchSymbolsTool {
    type Args = SearchSymbolsArgs;

    fn name(&self) -> &'static str {
        "code:search_symbols"
    }

    fn description(&self) -> &'static str {
        "Find where symbols are defined by substring name match across every indexed file. Returns \
         path + line/col + signature. Prefer this over grepping for a definition."
    }

    fn permission(&self, args: &SearchSymbolsArgs) -> PermissionClaim {
        PermissionClaim::read(format!("symbols:{}", args.needle))
    }

    async fn execute(&self, args: SearchSymbolsArgs, ctx: &ToolCtx) -> Result<ToolOutput> {
        let params = SearchSymbolsParams {
            needle: args.needle,
            kind: args.kind,
            limit: args.limit,
            max_tokens: None,
            format: None,
            cursor: None,
        };
        let server = require_server(ctx, Tool::name(self))?;
        let result = basemind::mcp::agent_api::search_symbols(server, params)
            .await
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        let value = basemind::cli::render::result_to_value(&result)
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        Ok(ToolOutput::ok(value.to_string()))
    }
}

/// `code:find_references` — call sites of any callee matching `name`.
struct FindReferencesTool;

/// Arguments for [`FindReferencesTool`].
#[derive(Deserialize, JsonSchema)]
struct FindReferencesArgs {
    /// Callee identifier to look up (substring, case-sensitive; no scope resolution).
    name: String,
    /// Cap on results (default 100, max 1000).
    #[serde(default)]
    limit: Option<u32>,
}

#[async_trait]
impl Tool for FindReferencesTool {
    type Args = FindReferencesArgs;

    fn name(&self) -> &'static str {
        "code:find_references"
    }

    fn description(&self) -> &'static str {
        "Find call sites of any callee whose identifier matches `name` (substring, name-only — \
         `Foo::bar()` and `bar()` both match `bar`). Prefer this over grepping for call sites."
    }

    fn permission(&self, args: &FindReferencesArgs) -> PermissionClaim {
        PermissionClaim::read(format!("references:{}", args.name))
    }

    async fn execute(&self, args: FindReferencesArgs, ctx: &ToolCtx) -> Result<ToolOutput> {
        let params = FindReferencesParams {
            name: args.name,
            limit: args.limit,
            max_tokens: None,
            format: None,
            cursor: None,
        };
        let server = require_server(ctx, Tool::name(self))?;
        let result = basemind::mcp::agent_api::find_references(server, params)
            .await
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        let value = basemind::cli::render::result_to_value(&result)
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        Ok(ToolOutput::ok(value.to_string()))
    }
}

/// `code:find_callers` — callers of a specific definition (path + name).
struct FindCallersTool;

/// Arguments for [`FindCallersTool`].
#[derive(Deserialize, JsonSchema)]
struct FindCallersArgs {
    /// Repository-relative path of the definition file (forward slashes).
    path: String,
    /// Name of the definition.
    name: String,
    /// Optional kind filter for resolving the definition (function/method/class/...).
    #[serde(default)]
    kind: Option<String>,
    /// Cap on results (default 100, max 1000).
    #[serde(default)]
    limit: Option<u32>,
}

#[async_trait]
impl Tool for FindCallersTool {
    type Args = FindCallersArgs;

    fn name(&self) -> &'static str {
        "code:find_callers"
    }

    fn description(&self) -> &'static str {
        "Find callers of a specific definition (resolved by path + name + optional kind), then the \
         same name-based scan as find_references. Prefer this over grepping for a function's callers."
    }

    fn permission(&self, args: &FindCallersArgs) -> PermissionClaim {
        PermissionClaim::read(args.path.clone())
    }

    async fn execute(&self, args: FindCallersArgs, ctx: &ToolCtx) -> Result<ToolOutput> {
        let params = FindCallersParams {
            path: RelPath::from(args.path.as_str()),
            name: args.name,
            kind: args.kind,
            limit: args.limit,
            max_tokens: None,
            cursor: None,
        };
        let server = require_server(ctx, Tool::name(self))?;
        let result = basemind::mcp::agent_api::find_callers(server, params)
            .await
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        let value = basemind::cli::render::result_to_value(&result)
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        Ok(ToolOutput::ok(value.to_string()))
    }
}

/// `code:call_graph` — BFS call graph rooted at a function name.
struct CallGraphTool;

/// Arguments for [`CallGraphTool`].
#[derive(Deserialize, JsonSchema)]
struct CallGraphArgs {
    /// Root function name (exact match against captured call-site identifiers).
    name: String,
    /// `"callers"` (default) walks upward; `"callees"` walks downward.
    #[serde(default)]
    direction: Option<String>,
    /// Optional path to disambiguate `name` when several functions share it.
    #[serde(default)]
    path: Option<String>,
    /// BFS depth from the root (default 3, capped at 6).
    #[serde(default)]
    max_depth: Option<u32>,
    /// Hard upper bound on total node count (default 100, max 500).
    #[serde(default)]
    max_nodes: Option<u32>,
}

#[async_trait]
impl Tool for CallGraphTool {
    type Args = CallGraphArgs;

    fn name(&self) -> &'static str {
        "code:call_graph"
    }

    fn description(&self) -> &'static str {
        "BFS call graph rooted at a function name: `direction=callers` (default) walks upward, \
         `callees` walks downward. Prefer this over manually chaining find_references calls."
    }

    fn permission(&self, args: &CallGraphArgs) -> PermissionClaim {
        PermissionClaim::read(format!("call_graph:{}", args.name))
    }

    async fn execute(&self, args: CallGraphArgs, ctx: &ToolCtx) -> Result<ToolOutput> {
        let params = CallGraphParams {
            name: args.name,
            direction: args.direction.unwrap_or_else(|| "callers".into()),
            path: args.path.as_deref().map(RelPath::from),
            max_depth: args.max_depth,
            max_nodes: args.max_nodes,
        };
        let server = require_server(ctx, Tool::name(self))?;
        let result = basemind::mcp::agent_api::call_graph(server, params)
            .await
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        let value = basemind::cli::render::result_to_value(&result)
            .map_err(|e| code_map_err(Tool::name(self))(e.to_string()))?;
        Ok(ToolOutput::ok(value.to_string()))
    }
}

/// `code:workspace_grep` — indexed regex over file contents.
struct WorkspaceGrepTool;

/// Arguments for [`WorkspaceGrepTool`].
#[derive(Deserialize, JsonSchema)]
struct WorkspaceGrepArgs {
    /// Rust regex syntax (`regex` crate).
    pattern: String,
    /// Optional language filter (e.g. `"rust"`, `"typescript"`).
    #[serde(default)]
    language: Option<String>,
    /// Optional substring filter on path.
    #[serde(default)]
    path_contains: Option<String>,
    /// Cap on hits (default 100, max 1000).
    #[serde(default)]
    limit: Option<u32>,
}

#[async_trait]
impl Tool for WorkspaceGrepTool {
    type Args = WorkspaceGrepArgs;

    fn name(&self) -> &'static str {
        "code:workspace_grep"
    }

    fn description(&self) -> &'static str {
        "Indexed regex search over file contents (whole corpus, after optional language / \
         path filters), returning path + line/col + a line of context. Prefer this over shelling ripgrep."
    }

    fn permission(&self, args: &WorkspaceGrepArgs) -> PermissionClaim {
        PermissionClaim::read(format!("grep:{}", args.pattern))
    }

    async fn execute(&self, args: WorkspaceGrepArgs, ctx: &ToolCtx) -> Result<ToolOutput> {
        let params = WorkspaceGrepParams {
            pattern: args.pattern,
            language: args.language,
            path_contains: args.path_contains,
            limit: args.limit,
            max_tokens: None,
            format: None,
            include_context: true,
            cursor: None,
        };
        let server = require_server(ctx, Tool::name(self))?;
        let result = basemind::mcp::agent_api::workspace_grep(server, params)
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
    fn code_nav_tools_are_named_and_read_only() {
        let tools = code_nav_tools();
        let names: Vec<_> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            vec![
                "code:outline",
                "code:search_symbols",
                "code:find_references",
                "code:find_callers",
                "code:call_graph",
                "code:workspace_grep",
            ]
        );
    }

    #[test]
    fn outline_requires_a_read_claim_on_the_path() {
        let claim = OutlineTool.permission_of(r#"{"path":"src/lib.rs"}"#).expect("parses");
        assert_eq!(claim, PermissionClaim::read("src/lib.rs"));
    }

    #[test]
    fn find_references_claims_a_read_on_the_name() {
        let claim = FindReferencesTool.permission_of(r#"{"name":"spawn"}"#).expect("parses");
        assert_eq!(claim, PermissionClaim::read("references:spawn"));
    }

    #[test]
    fn find_callers_claims_a_read_on_the_path() {
        let claim = FindCallersTool
            .permission_of(r#"{"path":"src/lib.rs","name":"scan"}"#)
            .expect("parses");
        assert_eq!(claim, PermissionClaim::read("src/lib.rs"));
    }

    #[test]
    fn call_graph_claims_a_read_on_the_name() {
        let claim = CallGraphTool
            .permission_of(r#"{"name":"process_file"}"#)
            .expect("parses");
        assert_eq!(claim, PermissionClaim::read("call_graph:process_file"));
    }

    #[test]
    fn workspace_grep_claims_a_read_on_the_pattern() {
        let claim = WorkspaceGrepTool
            .permission_of(r#"{"pattern":"TODO"}"#)
            .expect("parses");
        assert_eq!(claim, PermissionClaim::read("grep:TODO"));
    }
}
