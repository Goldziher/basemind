//! Types of the `code_*` operations.

use serde::{Deserialize, Serialize};

use super::{LifecycleNotice, is_zero};
use crate::engine::cursor::Cursor;
use crate::extract::SymbolKind;
use crate::path::RelPath;

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

/// Response of `code_outline`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct OutlineResponse {
    pub path: RelPath,
    pub language: String,
    pub size_bytes: u64,
    /// The parser reported syntax errors; the outline may be missing symbols.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub had_errors: bool,
    /// Symbols in the file, across every page.
    pub total: usize,
    /// True when the token budget dropped trailing symbols; page them with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// The file changed since it was indexed, so this outline was extracted from the file as it is
    /// now; the other tools describe the indexed content until `index_rescan`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    pub symbols: Vec<OutlineSymbol>,
    /// Imported modules, on the first page only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<String>,
    /// True when `imports` was cut at its cap of 200: `truncated_reason` is `imports` and
    /// `omitted` counts the imports left out. The symbol list is never cut this way; it pages.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    #[serde(default)]
    pub elapsed_us: u64,
}

/// One symbol of a `code_outline` page. `line` and `end_line` are 1-based and inclusive, the span
/// `code_read` and `git_blame` take.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct OutlineSymbol {
    pub name: String,
    pub kind: &'static str,
    pub line: u32,
    pub end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// The wire name of a symbol kind, as every tool reports it.
pub(crate) fn kind_to_str(k: SymbolKind) -> &'static str {
    match k {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Class => "class",
        SymbolKind::Interface => "interface",
        SymbolKind::Trait => "trait",
        SymbolKind::Type => "type",
        SymbolKind::Const => "const",
        SymbolKind::Module => "module",
        SymbolKind::Macro => "macro",
        SymbolKind::Impl => "impl",
        SymbolKind::Namespace => "namespace",
        SymbolKind::Getter => "getter",
        SymbolKind::Setter => "setter",
        SymbolKind::Field => "field",
        SymbolKind::Variable => "variable",
        SymbolKind::EnumVariant => "enum_variant",
        SymbolKind::Constructor => "constructor",
        SymbolKind::Decorator => "decorator",
        SymbolKind::Heading => "heading",
        SymbolKind::Unknown => "unknown",
    }
}
