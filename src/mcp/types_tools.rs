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
        pub const RETIRED_TOOLS: &[&str] = &["code", "git", "memory", "admin", "graph", "web", "shell", "workspace"];

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
    /// Also match the name as a fuzzy subsequence, ranked last.
    #[serde(default)]
    pub fuzzy: Option<bool>,
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
    /// Symbol whose body to return (exact name).
    #[serde(default)]
    pub symbol: Option<String>,
    /// With `symbol`: its kind, when several share the name.
    #[serde(default)]
    pub kind: Option<String>,
    /// First line of a span, 1-based. With `symbol`: a line inside the definition meant.
    #[serde(default)]
    pub line: Option<u32>,
    /// Last line of a span, inclusive (default: `line`). Not with `symbol`.
    #[serde(default)]
    pub end_line: Option<u32>,
}

/// Arguments of `code_grep`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeGrepParams {
    /// Text to find, not empty; a literal unless `regex` is true.
    pub pattern: String,
    /// Treat `pattern` as a Rust regex; `^` and `$` match at every line.
    #[serde(default)]
    pub regex: Option<bool>,
    /// Literal substring of the repo-relative path, e.g. `src/mcp/`; not a glob.
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
    /// Function, type, trait or module name; for `importers`, whole segments of the import path.
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
    /// Only commits touching this file; a trailing `/` means a directory (200 commits inspected per
    /// page; follow `next_cursor`). Not with `query` or `author`.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Words the commit message must all contain. Combines with `author` only.
    #[serde(default)]
    pub query: Option<String>,
    /// Words the author name or email must all contain. Combines with `query` only.
    #[serde(default)]
    pub author: Option<String>,
    /// History of this symbol's body, one window of commits per page; needs `path`, combines
    /// with nothing else but `line`.
    #[serde(default)]
    pub symbol: Option<String>,
    /// With `symbol`: 1-based line inside the definition meant, when several share the name.
    #[serde(default)]
    pub line: Option<u32>,
    /// Show this one commit in full; `max_tokens` bounds its file list. Combines with nothing.
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
    /// First line of a span, 1-based, as numbered at `rev`. With `symbol`: a line inside the
    /// definition meant.
    #[serde(default)]
    pub line: Option<u32>,
    /// Last line of a span, inclusive (default: `line`). Needs `line`; not with `symbol`.
    #[serde(default)]
    pub end_line: Option<u32>,
    /// Blame this symbol's lines, as they are at `rev`, instead of a span.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Revision to blame at (default HEAD).
    #[serde(default)]
    pub rev: Option<String>,
    /// Max hunks (default 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the list (default 6000).
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Arguments of `git_diff`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitDiffParams {
    /// File to diff, or a directory ending in `/` to narrow the status; omit every argument for
    /// the whole working-tree status.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Old revision (default HEAD). Needs `path`.
    #[serde(default)]
    pub from: Option<String>,
    /// New revision (default: the working tree). Needs `path`; not with `structural`.
    #[serde(default)]
    pub to: Option<String>,
    /// Symbol-level diff of `from` against the indexed working tree, instead of text hunks.
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
