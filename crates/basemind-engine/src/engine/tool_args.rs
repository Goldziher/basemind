//! The verb tools' argument types and the wire name each one answers to (ADR-0018).

use super::paging::ToolArgs;
use super::types::code::{CodeOutlineParams, CodeReadParams, CodeSymbolsParams};
use super::types::files::FindFilesToolParams;
use super::types::git::{GitBlameParams, GitDiffParams, GitLogParams};
use super::types::grep::CodeGrepParams;
use super::types::index::{IndexRescanParams, IndexStatusParams};
use super::types::memory::{MemorySaveParams, MemorySearchToolParams};
use super::types::refs::CodeRefsParams;
use super::types::search::{CodeSearchParams, DocsSearchParams};

macro_rules! tool_args {
    ($($params:ident => $tool:literal),+ $(,)?) => {
        $(impl ToolArgs for $params {
            const TOOL: &'static str = $tool;
        })+

        /// Every verb tool's MCP name, in declaration order.
        pub const VERB_TOOLS: &[&str] = &[$($tool),+];
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
