//! Types of the `code_*` operations.

use serde::{Deserialize, Serialize};

use super::{LifecycleNotice, is_zero};
use crate::engine::cursor::Cursor;
use crate::engine::error::EngineError;
use crate::extract::SymbolKind;
use crate::path::RelPath;

/// Arguments of `code_outline`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeOutlineParams {
    /// Repo-relative file path.
    pub path: RelPath,
    /// Default 500, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// A prior `next_cursor`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Default 6000.
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
        SymbolKind::Route => "route",
        SymbolKind::Unknown => "unknown",
    }
}

/// The [`SymbolKind`] a `kind` argument names; the inverse of [`kind_to_str`], plus `variant`.
pub(crate) fn parse_kind(s: &str) -> Result<SymbolKind, EngineError> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "function" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        "struct" => SymbolKind::Struct,
        "enum" => SymbolKind::Enum,
        "class" => SymbolKind::Class,
        "interface" => SymbolKind::Interface,
        "trait" => SymbolKind::Trait,
        "type" => SymbolKind::Type,
        "const" => SymbolKind::Const,
        "module" => SymbolKind::Module,
        "macro" => SymbolKind::Macro,
        "impl" => SymbolKind::Impl,
        "namespace" => SymbolKind::Namespace,
        "getter" => SymbolKind::Getter,
        "setter" => SymbolKind::Setter,
        "field" => SymbolKind::Field,
        "variable" => SymbolKind::Variable,
        "enum_variant" | "variant" => SymbolKind::EnumVariant,
        "constructor" => SymbolKind::Constructor,
        "decorator" => SymbolKind::Decorator,
        "heading" => SymbolKind::Heading,
        "route" => SymbolKind::Route,
        other => {
            return Err(EngineError::invalid_argument(format!("unknown symbol kind: {other}")));
        }
    })
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
    pub name: String,
    /// Optional kind filter to disambiguate when `name` matches multiple symbols
    /// (e.g. `"function"`, `"method"`, `"struct"`, …). Same values as `search_symbols`.
    #[serde(default)]
    pub kind: Option<String>,
    /// 1-based line inside the definition meant, when several in the file share the name.
    #[serde(default)]
    pub line: Option<u32>,
}

/// Response from the `expand` MCP tool.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct ExpandResponse {
    /// Repo-relative path of the file that was read.
    pub path: String,
    /// Resolved symbol name.
    pub name: String,
    /// Resolved symbol kind (e.g. `"function"`, `"struct"`).
    pub kind: String,
    /// 1-based first line of the symbol.
    pub line: u32,
    /// 1-based last line of the returned body, inclusive: the symbol's last line unless `truncated`.
    pub end_line: u32,
    /// Raw source text of the symbol body (`file_bytes[start_byte..end_byte]`).
    pub body: String,
    /// Byte length of the returned body (before any truncation).
    pub bytes: usize,
    /// `true` when the body was cut at the 128 KiB size cap, after its last whole line that fits;
    /// absent otherwise.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `size_cap` whenever `truncated` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// Bytes of the symbol left out of `body` (bytes, where `omitted` elsewhere counts items).
    /// Read on with a line span from `end_line + 1`.
    #[serde(skip_serializing_if = "is_zero")]
    pub omitted_bytes: usize,
    /// The file changed since it was indexed, so the symbol was located by parsing the working-tree
    /// file; the other tools describe the indexed content until `index_rescan`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
}

/// Response of a `code_read` line-span read.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct ReadSpanResponse {
    pub path: String,
    /// 1-based first line returned.
    pub line: u32,
    /// 1-based last line returned, inclusive: clamped to the file, and to what fits when `truncated`.
    pub end_line: u32,
    pub total_lines: u32,
    pub body: String,
    /// Byte length of the span before any truncation.
    pub bytes: usize,
    /// `true` when the body was cut at the 128 KiB size cap, after its last whole line that fits;
    /// absent otherwise.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `size_cap` whenever `truncated` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// Bytes of the span left out of `body` (bytes, where `omitted` elsewhere counts items). Read
    /// on from `end_line + 1`; one line longer than the cap cannot be read past it.
    #[serde(skip_serializing_if = "is_zero")]
    pub omitted_bytes: usize,
    /// Set on a span of more than 2,000 lines: it names `code_outline`, the cheaper call to read
    /// first before a narrower span.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// Answer of `code_read`: a symbol's body, or a line span.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum CodeReadResult {
    Symbol(ExpandResponse),
    Span(ReadSpanResponse),
}

/// Arguments of `code_symbols`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeSymbolsParams {
    /// Symbol name or fragment (case-sensitive).
    pub name: String,
    /// function, method, struct, enum, class, interface, trait, type, const, module, macro, route.
    #[serde(default)]
    pub kind: Option<String>,
    /// Prefix, or a glob when it has `*`, e.g. `src/`.
    #[serde(default)]
    pub path: Option<String>,
    /// Language, e.g. `rust`.
    #[serde(default)]
    pub language: Option<String>,
    /// Also match the name as a fuzzy subsequence, ranked last.
    #[serde(default)]
    pub fuzzy: Option<bool>,
    /// Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// A prior `next_cursor`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Default 6000.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Changed since a rev (empty: HEAD), A..B or A...B; main...HEAD+importers adds importers.
    #[serde(default)]
    pub changed: Option<String>,
}

/// The `code_symbols` sweep's own arguments, filled from [`CodeSymbolsParams`].
#[derive(Debug)]
pub(crate) struct SearchSymbolsParams {
    /// Substring matched against symbol name (case-sensitive).
    pub needle: String,
    /// Optional kind filter: function, method, struct, enum, class, interface,
    /// trait, type, const, module, macro.
    pub kind: Option<String>,
    /// Only definitions in files passing this `path` filter (a prefix, or a glob when it holds `*`).
    pub path: Option<crate::path_filter::PathFilter>,
    /// Only definitions in files of this language.
    pub language: Option<String>,
    /// Also match names the needle is a fuzzy subsequence of, ranked below every literal match.
    pub fuzzy: bool,
    /// Cap the number of results returned. Default 100, max 1000.
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned result list (not the whole envelope).
    /// Items are kept best-first until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    pub max_tokens: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Cursors are scoped to
    /// the in-RAM index snapshot and invalidate on rescan.
    pub cursor: Option<Cursor>,
}

/// One `code_symbols` row: where a definition is and what it is. Rows are ranked: the exact name,
/// then names it prefixes, then names holding it, then (with `fuzzy`) fuzzy matches.
///
/// Written as one string, `"<path>:<line>: <signature>"`: the signature names the definition and
/// shows its kind (`fn`, `struct`, `class`), as the line `git grep -n` prints would. A signature
/// that does not hold the name, or a symbol without one, is written `"<path>:<line>: <kind> <name>"`,
/// followed by `; <signature>` when there is one. Rows keep their rank, so they are not grouped by
/// file.
#[derive(Debug)]
pub(crate) struct SearchHitView {
    pub path: RelPath,
    /// 1-based line of the definition.
    pub line: u32,
    pub kind: &'static str,
    pub name: String,
    pub signature: Option<String>,
    /// The file changed since it was indexed: `line` is where the definition is now, or where it
    /// was when it can no longer be found. Listed in the response's `stale`, not on the row.
    pub stale: bool,
}

impl SearchHitView {
    /// The row after its path: `"<line>: <signature>"`, or `"<line>: <kind> <name>[; <signature>]"`.
    pub(crate) fn row_text(&self) -> String {
        match self.signature.as_deref() {
            Some(signature) if holds_identifier(signature, &self.name) => format!("{}: {signature}", self.line),
            Some(signature) => format!("{}: {} {}; {signature}", self.line, self.kind, self.name),
            None => format!("{}: {} {}", self.line, self.kind, self.name),
        }
    }
}

/// Whether `text` holds `name` as a whole identifier: not inside a longer word. `$` (JS, PHP) and
/// every byte of a non-ASCII character continue a word, so `foo` is not held by `$foo` or `cafoo`
/// written with a non-ASCII letter before it.
fn holds_identifier(text: &str, name: &str) -> bool {
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || !b.is_ascii();
    let bytes = text.as_bytes();
    !name.is_empty()
        && memchr::memmem::find_iter(bytes, name.as_bytes()).any(|at| {
            let end = at + name.len();
            !(at > 0 && word(bytes[at - 1])) && !bytes.get(end).is_some_and(|&b| word(b))
        })
}

/// The row as written: `"<path>:<line>: <signature>"`, the line `git grep -n` prints.
impl Serialize for SearchHitView {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{}:{}", self.path.to_str_lossy(), self.row_text()))
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct SearchResponse {
    /// Matching symbols in the corpus, across every page. Written only beside a `next_cursor`: a
    /// page without one lists every match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// True when a fuzzy sweep stopped at its candidate cap, so `total` is a lower bound. Never
    /// true because `limit` cut the page: that is paging, and `next_cursor` says it. Absent when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `scan_cap` whenever `truncated` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// True when a `max_tokens` budget dropped trailing results. The kept prefix is
    /// best-first; page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// The matches, best first: `"<path>:<line>: <signature>"`.
    #[schemars(with = "Vec<String>")]
    pub results: Vec<SearchHitView>,
    /// Files edited since the scan whose rows were re-read from disk: a row's line is where the
    /// definition is now. Absent when none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stale: Vec<RelPath>,
    /// Opaque cursor to pass back on the next call when more results are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// True when the caller passed a `cursor` minted against a different in-RAM snapshot
    /// (a rescan happened between calls). The caller must restart pagination from the top.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cursor_invalidated: bool,
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    /// Files the `changed` scope holds, when one was passed: `0` is a change that touched nothing, told
    /// apart from a change with no hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_files: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
}

#[cfg(test)]
mod tests {
    use super::holds_identifier;

    #[test]
    fn an_identifier_is_held_only_as_a_whole_word() {
        assert!(holds_identifier("fn foo(a: u8)", "foo"));
        assert!(holds_identifier("function $foo()", "$foo"));
        assert!(!holds_identifier("function $foo()", "foo"));
        assert!(!holds_identifier("fn éfoo()", "foo"));
        assert!(!holds_identifier("fn foo_bar()", "foo"));
        assert!(!holds_identifier("fn foo()", ""));
    }
}
