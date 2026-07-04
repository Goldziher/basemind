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
