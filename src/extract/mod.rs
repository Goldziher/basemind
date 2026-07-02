#[cfg(feature = "documents")]
pub mod doc;
pub mod l1;
pub mod l2;
pub mod l3;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::lang::{LangError, LangId, ParseOutcome, parse_with_default_timeout, with_parser};

use l1::extract_l1_from_tree;
use l2::extract_l2_from_tree;

/// Parse once with the default timeout and run both L1 and (optionally) L2 extraction against
/// the shared tree. Eliminates the duplicate parse that the scanner previously paid when
/// `eager_l2` was enabled — one `with_parser` call instead of two.
///
/// When `eager_l2` is `false` the function returns `(l1, None)` and pays only for the single
/// parse + L1 query walk. When `eager_l2` is `true` an L2 failure is non-fatal: the function
/// returns `(l1, None)` rather than propagating the error, matching the scanner's existing
/// tolerance for L2 failures.
pub fn extract_l1_l2(
    lang: LangId,
    source: &[u8],
    eager_l2: bool,
) -> Result<(FileMapL1, Option<FileMapL2>), ExtractError> {
    let outcome = with_parser(lang, |p| parse_with_default_timeout(p, source))?;
    let tree = match outcome {
        ParseOutcome::Ok(t) => t,
        ParseOutcome::Failed => return Err(ExtractError::ParseFailure),
        ParseOutcome::TimedOut => {
            return Err(ExtractError::ParseTimeout(crate::lang::DEFAULT_PARSE_TIMEOUT));
        }
    };
    let l1 = extract_l1_from_tree(lang, &tree, source)?;
    let l2 = if eager_l2 {
        // L2 failure is non-fatal: log nothing here (scanner already warns at the call site)
        // and let L1 stand on its own. The calls index stays empty for this file until the
        // lazy path populates it or the next scan retries.
        extract_l2_from_tree(lang, &tree, source).ok()
    } else {
        None
    };
    Ok((l1, l2))
}

/// Bumped any time the FileMap layout changes in an incompatible way OR the on-disk
/// directory shape changes. Stored in every serialized FileMap. Mismatch on read =
/// auto-wipe + re-scan.
///
/// - v3: per-view index directories under `.basemind/views/`.
/// - v4: path keys in the index and msgpack store became `RelPath` (BString) — the wire
///   format is identical for ASCII/UTF-8 paths but non-UTF-8 paths now round-trip via a
///   discriminated `{"bytes": [u8...]}` object.
pub const SCHEMA_VER: u16 = crate::version::RELEASE_MINOR;

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("non-utf8 source")]
    NonUtf8,
    #[error("tree-sitter parse failure")]
    ParseFailure,
    #[error("tree-sitter parse timed out (> {0:?}) — file likely pathological")]
    ParseTimeout(std::time::Duration),
    #[error(transparent)]
    Lang(#[from] LangError),
    /// Document-tier extraction failure (xberg). Only constructable when the
    /// `documents` feature is enabled.
    #[cfg(feature = "documents")]
    #[error("xberg extraction failed: {0}")]
    Document(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMapL1 {
    pub schema_ver: u16,
    pub language: String,
    pub size_bytes: u64,
    /// True when tree-sitter recovered from one or more syntax errors.
    /// The map still contains every symbol/import the parser was able to identify.
    pub had_errors: bool,
    pub error_count: u32,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<Import>,
    /// Inheritance / interface-implementation relationships detected in this file.
    /// Populated from the `;; section: implementations` query in each language's
    /// `.scm` override (or from `@reference.implementation` captures in TSLP's
    /// `tags.scm` adapted by `adapt_tslp_tags`). `#[serde(default)]` keeps existing
    /// L1 blobs without this field deserializable — no `SCHEMA_VER` bump needed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implementations: Vec<Implementation>,
}

/// A single inheritance or interface-implementation relationship found in a source file.
///
/// Examples by language:
/// - Rust: `impl Drawable for Circle` → `trait_name = "Drawable"`, `impl_type = "Circle"`
/// - Python: `class Circle(Drawable):` → `trait_name = "Drawable"`, `impl_type = "Circle"`
/// - TypeScript: `class Circle extends Shape` → `trait_name = "Shape"`, `impl_type = "Circle"`
/// - Java: `class Circle implements Drawable` → `trait_name = "Drawable"`, `impl_type = "Circle"`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Implementation {
    /// The parent / trait / interface name (e.g. `Drawable` in `impl Drawable for Circle`).
    pub trait_name: String,
    /// The implementing type / subclass name (e.g. `Circle`).
    pub impl_type: String,
    pub start_byte: u32,
    pub start_row: u32,
    pub start_col: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_row: u32,
    pub start_col: u32,
    pub signature: Option<String>,
    /// Decorator/annotation strings attached to the symbol — currently populated for Python
    /// (`@dataclass`, `@property`, …). Empty for languages that don't surface a decorator
    /// concept. Serde-default so old msgpack indices without this field still deserialize.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decorators: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Class,
    Interface,
    Trait,
    Type,
    Const,
    Module,
    Macro,
    /// Rust `impl` blocks. The captured name is the type the impl is for (e.g. `Foo` in
    /// `impl Foo { ... }`), trait impls show the trait + type concatenated by the query.
    Impl,
    /// TypeScript `namespace Foo {…}` and ambient `module "foo" {…}` declarations.
    Namespace,
    /// TypeScript / JavaScript class accessors — `get x() {…}` and `set x(v) {…}`.
    /// Surfaced as distinct kinds so callers can search for accessors specifically and so
    /// `outline` rendering can highlight the read/write split.
    Getter,
    Setter,
    Unknown,
    // ─── Tail-only additions ──────────────────────────────────────────────────
    //
    // Variants below this line are appended to keep `symbol_kind_byte()` ordinals in
    // `src/index/keys.rs` stable. Append-only is the contract — reordering would silently
    // miscategorize cached entries. See the `index-keyspace-evolution` skill.
    //
    /// Struct / class field. Captured by TSLP `tags.scm` under `@definition.field` in many
    /// languages; surfaced so symbol search can target data members.
    Field,
    /// Local or top-level binding — `let`/`var`/`const` in JS, Python `x = …` at module scope,
    /// `var` in Go, etc. Anything outside the override set lands here when TSLP tags it.
    Variable,
    /// Enum case / variant. Distinct from the parent `Enum` so callers can disambiguate.
    EnumVariant,
    /// Constructor / `__init__` / Rust `Self::new`-style associated fn marked as constructor
    /// by the grammar. Useful for "find all constructors" navigation.
    Constructor,
    /// Decorator / annotation symbol (`@Component`, `@dataclass`, Java `@Override`). We already
    /// surface decorator *strings* on `Symbol.decorators`; this kind covers grammars whose
    /// `tags.scm` emits the decorator as a standalone definition.
    Decorator,
    /// Markdown / Obsidian heading (ATX `#`/`##`… or setext underline). The captured name is the
    /// heading text; the heading hierarchy is implicit in document (line) order. Lets `outline` and
    /// `search_symbols` navigate a notes vault by section, mirroring how source symbols work.
    Heading,
}

impl SymbolKind {
    pub fn from_capture_suffix(suffix: &str) -> Self {
        match suffix {
            "function" => Self::Function,
            "method" => Self::Method,
            "struct" => Self::Struct,
            "enum" => Self::Enum,
            "class" => Self::Class,
            "interface" => Self::Interface,
            "trait" => Self::Trait,
            "type" => Self::Type,
            "const" | "constant" => Self::Const,
            "module" => Self::Module,
            "macro" => Self::Macro,
            "impl" => Self::Impl,
            "namespace" => Self::Namespace,
            "getter" => Self::Getter,
            "setter" => Self::Setter,
            "field" => Self::Field,
            "variable" | "var" => Self::Variable,
            "enum_variant" | "variant" => Self::EnumVariant,
            "constructor" => Self::Constructor,
            "decorator" => Self::Decorator,
            "heading" => Self::Heading,
            _ => Self::Unknown,
        }
    }

    /// Rank used to break ties when two query patterns capture the same `(start_byte, name)`
    /// pair — the higher-scoring kind wins (e.g. `function` beats `const` for `const foo = () => …`).
    /// Bump scores carefully; tests assert kinds directly.
    pub(crate) fn specificity(self) -> u8 {
        use SymbolKind::*;
        match self {
            Unknown => 0,
            Const | Variable | Field | Decorator => 1,
            // Everything below is "concrete": one specific shape of declaration.
            // Same score — first-seen wins among them, which keeps document order intact
            // when the same symbol is captured twice as e.g. both function and method.
            Function | Method | Struct | Enum | Class | Interface | Trait | Type | Module | Macro | Impl
            | Namespace | Getter | Setter | EnumVariant | Constructor | Heading => 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Import {
    /// Best-effort module path / symbol; None when the language doesn't expose one cleanly.
    pub module: Option<String>,
    pub raw: String,
    pub start_byte: u32,
    pub end_byte: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMapL2 {
    pub schema_ver: u16,
    pub language: String,
    pub calls: Vec<Call>,
    pub docs: Vec<DocComment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Call {
    pub callee: String,
    pub start_byte: u32,
    pub end_byte: u32,
    /// 0-based row. Older L2 blobs predating this field deserialize to 0 — readers should
    /// treat (0, 0) as "unknown" and fall back to byte offsets when precise location matters.
    #[serde(default)]
    pub start_row: u32,
    /// 0-based byte column.
    #[serde(default)]
    pub start_col: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocComment {
    pub text: String,
    pub start_byte: u32,
    pub end_byte: u32,
}
