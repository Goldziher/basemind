//! Code-navigation tools backed by the in-process basemind server.
//!
//! These are the differentiator: instead of grepping and reading whole files, the agent asks the
//! code map structural questions and gets back paths + line ranges + signatures at a fraction of
//! the tokens. Each tool wraps one `agent_api` facade call and returns its JSON verbatim.

use std::sync::Arc;

use async_trait::async_trait;
use basemind::mcp::params::{OutlineParams, SearchSymbolsParams};
use basemind::path::RelPath;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Tool, ToolCtx, ToolDyn, ToolOutput};
use crate::error::{AgentError, Result};
use crate::permission::PermissionClaim;

/// The read-only code-navigation tools available to the agent.
pub fn code_nav_tools() -> Vec<Arc<dyn ToolDyn>> {
    vec![Arc::new(OutlineTool), Arc::new(SearchSymbolsTool)]
}

fn code_map_err(tool: &'static str) -> impl Fn(String) -> AgentError {
    move |message| AgentError::CodeMap { tool, message }
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
        let result = basemind::mcp::agent_api::outline(&ctx.server, params)
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
        let result = basemind::mcp::agent_api::search_symbols(&ctx.server, params)
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
        assert!(names.contains(&"code:outline"));
        assert!(names.contains(&"code:search_symbols"));
    }

    #[test]
    fn outline_requires_a_read_claim_on_the_path() {
        let claim = OutlineTool.permission_of(r#"{"path":"src/lib.rs"}"#).expect("parses");
        assert_eq!(claim, PermissionClaim::read("src/lib.rs"));
    }
}
