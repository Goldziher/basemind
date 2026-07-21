//! The tool system: a strongly-typed [`Tool`] trait, an object-safe [`ToolDyn`] erasure so the
//! registry can hold heterogeneous tools, and the built-in tools.
//!
//! The differentiator is [`codenav`]: tools that answer structural questions by querying the
//! in-process basemind server (outline, symbol search) instead of grepping and reading whole files
//! — a fraction of the tokens per turn.

mod codenav;
mod registry;
mod shell;

pub use codenav::code_nav_tools;
pub use registry::ToolRegistry;
pub use shell::ShellTool;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use basemind::mcp::BasemindServer;
use liter_llm::{ChatCompletionTool, FunctionDefinition, ToolType};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::error::{AgentError, Result};
use crate::permission::PermissionClaim;

/// Shared context handed to every tool during execution. Additional fields (progress sink,
/// cancellation token) are added by later slices without changing tool signatures.
pub struct ToolCtx {
    /// Repository root — working directory for shell and base for relative paths.
    pub root: PathBuf,
    /// In-process basemind server for code-map queries.
    pub server: Arc<BasemindServer>,
}

/// The output of a tool: the text fed back to the model, and whether it is an error (errors are
/// still fed back so the model can adapt rather than aborting the turn).
pub struct ToolOutput {
    /// The content returned to the model as the tool result.
    pub text: String,
    /// Whether this represents an error.
    pub is_error: bool,
}

impl ToolOutput {
    /// A successful result.
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    /// An error result (fed back to the model, not a turn abort).
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// A strongly-typed tool. `Args` are deserialized from the model's JSON, and their JSON Schema is
/// advertised to the model.
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    /// The typed arguments this tool accepts.
    type Args: DeserializeOwned + JsonSchema + Send;

    /// The tool's namespaced name (e.g. `code:outline`).
    fn name(&self) -> &'static str;

    /// A one-line description advertised to the model.
    fn description(&self) -> &'static str;

    /// The permission claim this call requires, derived from its arguments.
    fn permission(&self, args: &Self::Args) -> PermissionClaim;

    /// Execute the tool.
    async fn execute(&self, args: Self::Args, ctx: &ToolCtx) -> Result<ToolOutput>;
}

/// Object-safe erasure of [`Tool`] so a registry can hold heterogeneous tools and dispatch by name.
#[async_trait]
pub trait ToolDyn: Send + Sync {
    /// The tool's name.
    fn name(&self) -> &'static str;
    /// The liter-llm tool definition (name + description + JSON-schema parameters).
    fn spec(&self) -> ChatCompletionTool;
    /// The permission claim for the given raw JSON arguments.
    fn permission_of(&self, raw_args: &str) -> Result<PermissionClaim>;
    /// Execute with raw JSON arguments.
    async fn call(&self, raw_args: &str, ctx: &ToolCtx) -> Result<ToolOutput>;
}

#[async_trait]
impl<T: Tool> ToolDyn for T {
    fn name(&self) -> &'static str {
        Tool::name(self)
    }

    fn spec(&self) -> ChatCompletionTool {
        ChatCompletionTool {
            tool_type: ToolType::Function,
            function: FunctionDefinition {
                name: Tool::name(self).to_string(),
                description: Some(Tool::description(self).to_string()),
                parameters: Some(args_schema::<T::Args>()),
                strict: None,
            },
        }
    }

    fn permission_of(&self, raw_args: &str) -> Result<PermissionClaim> {
        let args = parse_args::<T::Args>(raw_args)?;
        Ok(Tool::permission(self, &args))
    }

    async fn call(&self, raw_args: &str, ctx: &ToolCtx) -> Result<ToolOutput> {
        let args = parse_args::<T::Args>(raw_args)?;
        Tool::execute(self, args, ctx).await
    }
}

/// Deserialize tool arguments, treating an empty string as an empty object (some providers send
/// `""` for a no-argument call).
fn parse_args<A: DeserializeOwned>(raw: &str) -> Result<A> {
    let raw = if raw.trim().is_empty() { "{}" } else { raw };
    serde_json::from_str(raw).map_err(AgentError::ToolArgs)
}

/// Generate the JSON Schema for a tool's argument type.
fn args_schema<A: JsonSchema>() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(A)).unwrap_or_else(|_| serde_json::json!({ "type": "object" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopTool;

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct NoopArgs {
        #[allow(dead_code)]
        path: String,
    }

    #[async_trait]
    impl Tool for NoopTool {
        type Args = NoopArgs;
        fn name(&self) -> &'static str {
            "test:noop"
        }
        fn description(&self) -> &'static str {
            "noop"
        }
        fn permission(&self, args: &NoopArgs) -> PermissionClaim {
            PermissionClaim::read(args.path.clone())
        }
        async fn execute(&self, args: NoopArgs, _ctx: &ToolCtx) -> Result<ToolOutput> {
            Ok(ToolOutput::ok(args.path))
        }
    }

    #[test]
    fn spec_carries_name_description_and_object_schema() {
        let spec = ToolDyn::spec(&NoopTool);
        assert_eq!(spec.function.name, "test:noop");
        assert_eq!(spec.function.description.as_deref(), Some("noop"));
        let params = spec.function.parameters.expect("has parameters");
        assert_eq!(params["type"], "object");
        assert!(params["properties"].get("path").is_some());
    }

    #[test]
    fn permission_of_derives_a_read_claim_from_args() {
        let claim = NoopTool.permission_of(r#"{"path":"src/lib.rs"}"#).expect("parses");
        assert_eq!(claim, PermissionClaim::read("src/lib.rs"));
    }

    #[test]
    fn permission_of_rejects_malformed_json() {
        assert!(matches!(
            NoopTool.permission_of("{not json"),
            Err(AgentError::ToolArgs(_))
        ));
    }
}
