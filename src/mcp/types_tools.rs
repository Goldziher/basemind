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

pub use crate::engine::tool_args::VERB_TOOLS;

/// Run `tool`'s own argument parse on `arguments`: `Some(Err(message))` is the rejection an agent
/// would be shown, `None` a name that is not a verb tool.
#[cfg(feature = "test-support")]
pub fn check_arguments(tool: &str, arguments: serde_json::Value) -> Option<Result<(), String>> {
    macro_rules! parse_as {
        ($($params:ident),+ $(,)?) => {
            match tool {
                $(<$params as super::strict::ToolArgs>::TOOL => {
                    Some(super::strict::parse::<$params>(arguments).map(drop))
                })+
                _ => None,
            }
        };
    }
    parse_as!(
        CodeOutlineParams,
        CodeSymbolsParams,
        CodeReadParams,
        CodeGrepParams,
        CodeSearchParams,
        CodeRefsParams,
        FindFilesToolParams,
        DocsSearchParams,
        GitLogParams,
        GitBlameParams,
        GitDiffParams,
        MemorySaveParams,
        MemorySearchToolParams,
        IndexStatusParams,
        IndexRescanParams,
    )
}

pub use crate::engine::tool_names::RETIRED_TOOLS;
pub use crate::engine::types::code::{CodeOutlineParams, CodeReadParams, CodeSymbolsParams};
pub use crate::engine::types::files::FindFilesToolParams;
pub use crate::engine::types::git::{GitBlameParams, GitDiffParams, GitLogParams};
pub use crate::engine::types::grep::CodeGrepParams;
pub use crate::engine::types::index::{IndexRescanParams, IndexStatusParams};
pub use crate::engine::types::memory::{MemorySaveParams, MemorySearchToolParams};
pub use crate::engine::types::refs::CodeRefsParams;
pub use crate::engine::types::search::{CodeSearchParams, DocsSearchParams};
