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

/// Arguments of `code_outline`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeOutlineParams {
    /// Repo-relative file path.
    pub path: RelPath,
    /// Max symbols (default 500).
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the list (default 6000).
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Arguments of `code_symbols`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeSymbolsParams {
    /// Symbol name or fragment (case-sensitive).
    pub name: String,
    /// function, method, struct, enum, class, interface, trait, type, const, module, macro.
    #[serde(default)]
    pub kind: Option<String>,
    /// Max hits (default 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the list (default 6000).
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Arguments of `code_read`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeReadParams {
    /// Repo-relative file path.
    pub path: RelPath,
    /// Symbol whose body to return (exact name). Alternative to `line`.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Disambiguates `symbol` when several share the name.
    #[serde(default)]
    pub kind: Option<String>,
    /// First line, 1-based. Alternative to `symbol`.
    #[serde(default)]
    pub line: Option<u32>,
    /// Last line, inclusive (default: `line`).
    #[serde(default)]
    pub end_line: Option<u32>,
}

/// Arguments of `code_grep`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeGrepParams {
    /// Text to find; a literal unless `regex` is true.
    pub pattern: String,
    /// Treat `pattern` as a Rust regex.
    #[serde(default)]
    pub regex: Option<bool>,
    /// Substring of the repo-relative path (not a glob).
    #[serde(default)]
    pub path: Option<String>,
    /// Language filter, e.g. `rust`.
    #[serde(default)]
    pub language: Option<String>,
    /// Add one line before and after each hit.
    #[serde(default)]
    pub context: Option<bool>,
    /// Max hits (default 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the list (default 6000).
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

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
    /// Function, type, trait or module name.
    pub name: String,
    /// What to list (default `callers`).
    #[serde(default)]
    #[schemars(extend("enum" = ["callers", "implementations", "importers", "definition"]))]
    pub relation: Option<String>,
    /// File defining `name` (pins one definition); for `definition`, the file holding the reference.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// `definition` only: 1-based line of the reference.
    #[serde(default)]
    pub line: Option<u32>,
    /// `definition` only: 1-based column (default: where `name` occurs on the line).
    #[serde(default)]
    pub column: Option<u32>,
    /// Max hits (default 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the list (default 6000).
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Arguments of `find_files`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindFilesToolParams {
    /// Fuzzy fragment of a file name or path; omit to list.
    #[serde(default)]
    pub query: Option<String>,
    /// Only paths starting with this, e.g. `src/mcp/`.
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Language filter, e.g. `rust`, `markdown`.
    #[serde(default)]
    pub language: Option<String>,
    /// Max files (default 200).
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the list (default 6000).
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
    /// Keep chunks whose keywords contain this.
    #[serde(default)]
    pub keywords: Option<String>,
    /// Keep chunks naming an entity of this category, e.g. `PERSON`.
    #[serde(default)]
    pub entity: Option<String>,
    /// Document scope (default: this repository).
    #[serde(default)]
    pub scope: Option<String>,
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

/// Arguments of `git_log`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitLogParams {
    /// Only commits touching this file.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Full-text search of commit messages.
    #[serde(default)]
    pub query: Option<String>,
    /// Search author names and emails.
    #[serde(default)]
    pub author: Option<String>,
    /// History of this symbol's body (needs `path`).
    #[serde(default)]
    pub symbol: Option<String>,
    /// Show this one commit in full.
    #[serde(default)]
    pub rev: Option<String>,
    /// Max commits (default 20).
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the list (default 6000).
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Arguments of `git_blame`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitBlameParams {
    /// Repo-relative file path.
    pub path: RelPath,
    /// First line, 1-based (needs `end_line`).
    #[serde(default)]
    pub line: Option<u32>,
    /// Last line, inclusive.
    #[serde(default)]
    pub end_line: Option<u32>,
    /// Blame this symbol's lines instead of a span.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Revision to blame at (default HEAD).
    #[serde(default)]
    pub rev: Option<String>,
    /// Max hunks.
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

/// Arguments of `git_diff`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitDiffParams {
    /// File to diff; omit (with no revisions) for the working-tree status.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Old revision (default HEAD).
    #[serde(default)]
    pub from: Option<String>,
    /// New revision (default: the working tree).
    #[serde(default)]
    pub to: Option<String>,
    /// Symbol-level diff instead of text hunks.
    #[serde(default)]
    pub structural: Option<bool>,
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
    /// Search notes by meaning.
    #[serde(default)]
    pub query: Option<String>,
    /// Read one note by exact key.
    #[serde(default)]
    pub key: Option<String>,
    /// List notes whose key starts with this (`""` lists all).
    #[serde(default)]
    pub prefix: Option<String>,
    /// Only notes carrying this tag.
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
    /// Max notes.
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
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
    /// Repo-relative files or directories to re-index (default: everything that changed).
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// Re-index the whole workspace.
    #[serde(default)]
    pub full: Option<bool>,
}
