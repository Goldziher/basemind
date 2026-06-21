//! Param and response types for the `compress` MCP tool.
//!
//! `compress` is a code-aware token-reduction tool: for indexed source files it
//! returns the structural L1 outline (signatures + imports, no bodies); for prose
//! text it applies a lexical pass (whitespace collapsing, filler removal, paragraph
//! deduplication) that always runs, and optionally a kreuzberg prose-compression
//! pass when the `documents` feature is enabled.
//!
//! Split into its own file to keep `types.rs` under the 1000-line cap.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::path::RelPath;

/// Parameters for the `compress` MCP tool.
///
/// Exactly one of `text` or `path` must be supplied; both or neither is an error.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CompressParams {
    /// Raw text to compress (prose path). Mutually exclusive with `path`.
    pub text: Option<String>,
    /// Repo-relative path of a source file to compress structurally (code path).
    /// Mutually exclusive with `text`.
    pub path: Option<RelPath>,
    /// Reduction intensity: `off`, `light`, `moderate` (default), `aggressive`,
    /// `maximum`. Only meaningful on the prose path; the code/structural path
    /// always returns the L1 outline regardless of this setting.
    #[serde(default)]
    pub level: Option<String>,
    /// When `true` (default), code blocks inside prose are left intact. Has no
    /// effect on the structural (code file) path.
    #[serde(default = "default_true")]
    pub preserve_code: bool,
    /// Soft token budget hint. Returned in the response but does not hard-cap
    /// output in this version — accurate tokenizer is pending.
    pub target_tokens: Option<u32>,
}

fn default_true() -> bool {
    true
}

/// Parameters for the `expand` MCP tool.
///
/// Resolves one symbol by `name` (and optionally `kind`) in the file's L1 outline, then
/// reads the source file from disk and returns the raw bytes in `[start_byte..end_byte]`.
/// When more than one symbol matches `name` (e.g. overloads), the tool returns an error
/// listing the matches so the caller can disambiguate by supplying `kind`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ExpandParams {
    /// Repo-relative path of the indexed source file.
    pub path: RelPath,
    /// Symbol name to expand. Matched exactly (case-sensitive) against the L1 outline.
    /// Aliases: `symbol`, `needle`.
    #[serde(alias = "symbol", alias = "needle")]
    pub name: String,
    /// Optional kind filter to disambiguate when `name` matches multiple symbols
    /// (e.g. `"function"`, `"method"`, `"struct"`, …). Same values as `search_symbols`.
    #[serde(default)]
    pub kind: Option<String>,
}

/// Response from the `expand` MCP tool.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ExpandResponse {
    /// Repo-relative path of the file that was read.
    pub path: String,
    /// Resolved symbol name.
    pub name: String,
    /// Resolved symbol kind (e.g. `"function"`, `"struct"`).
    pub kind: String,
    /// One-based start row of the symbol in the source file.
    pub start_row: u32,
    /// One-based end row of the symbol in the source file (computed from the byte slice).
    pub end_row: u32,
    /// Raw source text of the symbol body (`file_bytes[start_byte..end_byte]`).
    pub body: String,
    /// Byte length of the returned body (before any truncation).
    pub bytes: usize,
    /// `true` when the body was truncated to the `EXPAND_BODY_CAP`.
    pub truncated: bool,
}

/// Response from the `compress` MCP tool.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct CompressResponse {
    /// Byte length of the original input (file contents or text).
    pub original_bytes: usize,
    /// Rough token estimate of the original input (bytes / 4).
    pub original_tokens_est: u64,
    /// Byte length of the compressed output.
    pub compressed_bytes: usize,
    /// Rough token estimate of the compressed output (bytes / 4).
    pub compressed_tokens_est: u64,
    /// Compression ratio: `compressed_bytes as f32 / original_bytes as f32`.
    /// Values below 1.0 indicate a reduction; 1.0 means no change.
    pub ratio: f32,
    /// The strategy that was applied: `"structural"` for indexed code files,
    /// `"lexical"` for prose-only compression (no kreuzberg), or
    /// `"lexical+prose"` when kreuzberg prose reduction ran.
    pub strategy: String,
    /// The compressed output text.
    pub output: String,
    /// Disclosure note about token counting accuracy.
    pub tokens_note: String,
}
