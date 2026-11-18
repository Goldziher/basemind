//! Wire arguments of the verb tools (ADR-0018): one struct per tool.
//!
//! Every struct is `deny_unknown_fields` and is deserialized through [`Strict`](super::strict::Strict),
//! so a key the tool does not take is an error naming the accepted set, never a silently dropped
//! filter. A field is `Option` only when the tool genuinely runs without it; "exactly one of"
//! constraints cannot be stated in the flat schema subset hosts accept (`tests/mcp_schema_wire.rs`)
//! and are checked in the tool body instead.
//!
//! The field docs are the schema descriptions an agent reads on every session, so they are kept to
//! a clause: the whole `tools/list` has a byte ceiling.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::strict::ToolArgs;

macro_rules! tool_args {
    ($($params:ident => $tool:literal),+ $(,)?) => {
        $(impl ToolArgs for $params {
            const TOOL: &'static str = $tool;
        })+

        /// Every verb tool's MCP name, in declaration order.
        pub const VERB_TOOLS: &[&str] = &[$($tool),+];

        /// The mode-dispatched tools the verb tools replaced (ADR-0018). None may be advertised.
        pub const RETIRED_TOOLS: &[&str] = &["code", "git", "memory", "admin", "graph", "web", "shell", "workspace", "agents"];

        /// Run `tool`'s own argument parse on `arguments`: `Some(Err(message))` is the rejection an
        /// agent would be shown, `None` a name that is not a verb tool.
        #[cfg(feature = "test-support")]
        pub fn check_arguments(tool: &str, arguments: serde_json::Value) -> Option<Result<(), String>> {
            match tool {
                $($tool => Some(super::strict::parse::<$params>(arguments).map(drop)),)+
                _ => None,
            }
        }
    };
}

tool_args! {
    CodeOutlineParams => "code_outline",
    CodeSymbolsParams => "code_symbols",
    CodeReadParams => "code_read",
    CodeGrepParams => "code_grep",
    CodeSearchParams => "code_search",
    CodeRefsParams => "code_refs",
    FindFilesToolParams => "find_files",
    DocsSearchParams => "docs_search",
    GitLogParams => "git_log",
    GitBlameParams => "git_blame",
    GitDiffParams => "git_diff",
    MemorySaveParams => "memory_save",
    MemorySearchToolParams => "memory_search",
    IndexStatusParams => "index_status",
    IndexRescanParams => "index_rescan",
}

pub use crate::engine::types::code::{CodeOutlineParams, CodeReadParams, CodeSymbolsParams};
pub use crate::engine::types::files::FindFilesToolParams;
pub use crate::engine::types::git::{GitBlameParams, GitDiffParams, GitLogParams};
pub use crate::engine::types::grep::CodeGrepParams;
pub use crate::engine::types::memory::{MemorySaveParams, MemorySearchToolParams};
pub use crate::engine::types::refs::CodeRefsParams;
pub use crate::engine::types::search::{CodeSearchParams, DocsSearchParams};

/// Arguments of `index_status`: none.
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IndexStatusParams {}

/// Arguments of `index_rescan`.
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IndexRescanParams {
    /// Repo-relative files or directories to re-index (default: the whole workspace, skipping
    /// unchanged files).
    #[serde(default)]
    pub paths: Option<Vec<String>>,
}
