//! The tool registry: name → tool, plus the liter-llm tool-spec list advertised to the model.

use std::collections::BTreeMap;
use std::sync::Arc;

use liter_llm::ChatCompletionTool;

use super::ToolDyn;

/// Holds the tools available to a session, keyed by name (sorted for stable spec ordering).
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, Arc<dyn ToolDyn>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool (a later registration with the same name replaces the earlier one).
    pub fn register(&mut self, tool: Arc<dyn ToolDyn>) {
        self.tools.insert(tool.name(), tool);
    }

    /// Register many tools at once.
    pub fn register_all(&mut self, tools: impl IntoIterator<Item = Arc<dyn ToolDyn>>) {
        for tool in tools {
            self.register(tool);
        }
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn ToolDyn>> {
        self.tools.get(name)
    }

    /// The liter-llm tool definitions advertised to the model, in stable name order.
    pub fn specs(&self) -> Vec<ChatCompletionTool> {
        self.tools.values().map(|tool| tool.spec()).collect()
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ShellTool, code_nav_tools, git_history_tools};

    #[test]
    fn specs_expose_registered_tool_names_in_sorted_order() {
        let mut registry = ToolRegistry::new();
        registry.register_all(code_nav_tools());
        registry.register_all(git_history_tools());
        registry.register(Arc::new(ShellTool));

        let names: Vec<_> = registry.specs().into_iter().map(|s| s.function.name).collect();
        assert_eq!(
            names,
            vec![
                "code:call_graph",
                "code:find_callers",
                "code:find_references",
                "code:outline",
                "code:search_symbols",
                "code:workspace_grep",
                "git:blame_symbol",
                "git:diff_file",
                "git:recent_changes",
                "shell:exec",
            ]
        );
        assert_eq!(registry.len(), 10);
        assert!(registry.get("shell:exec").is_some());
        assert!(registry.get("nope").is_none());
    }
}
