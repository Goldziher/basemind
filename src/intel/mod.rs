//! Code-intelligence engines: scope- and import-resolved navigation.
//!
//! Each submodule is a per-ecosystem resolution engine, gated on the crate feature that pulls
//! its backing library. Unlike the tree-sitter code-map, these engines run their *own* parser
//! for the target language and produce resolved reference/definition edges that the scanner's
//! second pass persists into the `refs_by_def` index.
//!
//! - [`js`] (feature `code-intel-js`) — JavaScript/TypeScript via oxc (`oxc_semantic` +
//!   `oxc_resolver`). Self-contained: needs no tree-sitter grammar.
//!
//! The grammar-native intra-file layer (tree-sitter `locals`) lives in
//! [`crate::extract::locals`] and needs no feature flag.

#[cfg(feature = "code-intel-js")]
pub mod js;
pub mod model;
pub mod resolve;
pub(crate) mod resolve_pass;
/// Per-language module-specifier resolution (importer specifier → repo-relative target file),
/// shared by the cross-file stitch and its incremental re-stitch. JS/TS via oxc; Python/Java via
/// path arithmetic over conventional package/source-root layouts.
pub(crate) mod resolver;
/// Stack-graph resolution engine (feature `code-intel-stack`): runs `.tsg` name-binding rules to
/// produce precise intra-file resolution for Python and Java, plus the import/export facts the
/// cross-file join consumes.
#[cfg(feature = "code-intel-stack")]
pub mod stackgraph;
/// Cross-file resolution stitch (importer binding → resolved target export), run once at the end
/// of the scanner's resolve pass. Resolves each importer's specifiers via the per-language
/// [`resolver::SpecifierResolver`], so it covers any language with a compiled-in resolver (JS/TS
/// under `code-intel-js`; Python/Java under `code-intel-stack`).
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
pub mod xfile;
