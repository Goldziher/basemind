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

use super::cursor::Cursor;
use super::strict::ToolArgs;
use super::types_memory::Visibility;
use crate::path::RelPath;

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

/// Arguments of `code_search`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeSearchParams {
    /// What the code does, in plain words.
    pub query: String,
    /// Retrieval lane (default `hybrid`).
    #[serde(default)]
    #[schemars(extend("enum" = ["hybrid", "semantic", "keyword"]))]
    pub lane: Option<String>,
    /// Max hits (default 10).
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the list (default 6000).
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Arguments of `code_refs`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeRefsParams {
    /// Name called, implemented or defined; for `importers`, whole segments of the import path.
    pub name: String,
    /// What to list (default `callers`).
    #[serde(default)]
    #[schemars(extend("enum" = ["callers", "implementations", "importers", "definition"]))]
    pub relation: Option<String>,
    /// `callers`: file defining `name`, pinning that definition. `definition`: file holding the
    /// reference (required). Not with `implementations` or `importers`.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Needs `path`. `callers`: 1-based line inside the definition meant, when several share the
    /// name. `definition`: line of the reference (required).
    #[serde(default)]
    pub line: Option<u32>,
    /// `definition` only: 1-based column (default: where `name` occurs on the line).
    #[serde(default)]
    pub column: Option<u32>,
    /// Name matching (default `exact`). Not with `importers` or `definition`.
    #[serde(default, rename = "match")]
    #[schemars(extend("enum" = ["exact", "substring"]))]
    pub match_mode: Option<String>,
    /// Max hits (default 100). Not with `definition`.
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page. Not with `definition`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the list (default 6000). Not with `definition`.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Arguments of `docs_search`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocsSearchParams {
    /// What to look for, in plain words.
    pub query: String,
    /// Exact MIME filter, e.g. `application/pdf`.
    #[serde(default)]
    pub mime_type: Option<String>,
    /// Format: pdf, office, html, markdown, email, notebook, text or other.
    #[serde(default)]
    pub kind: Option<String>,
    /// Glob over document paths (`docs/**/*.md`).
    #[serde(default)]
    pub path: Option<String>,
    /// Whole keywords, comma-separated; any one matches.
    #[serde(default)]
    pub keywords: Option<String>,
    /// Keep chunks naming an entity of this category, e.g. `PERSON`.
    #[serde(default)]
    pub entity: Option<String>,
    /// Max hits (default 10).
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the list (default 6000).
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Arguments of `memory_save`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemorySaveParams {
    /// Stable identifier, e.g. `decision/auth-tokens`.
    pub key: String,
    /// The note.
    pub value: String,
    /// Labels to filter on later.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

/// Arguments of `memory_search`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemorySearchToolParams {
    /// Search notes by meaning. At most one of `query`, `key`, `prefix`; none lists every note.
    #[serde(default)]
    pub query: Option<String>,
    /// Read one note by exact key.
    #[serde(default)]
    pub key: Option<String>,
    /// List notes whose key starts with this.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Only notes carrying this tag. Not with `key`.
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
    /// Max notes (default 10 for `query`, 100 for a listing). Not with `key`.
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous listing page. Not with `query` or `key`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

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
