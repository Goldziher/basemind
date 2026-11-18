//! Persistent, engine-independent code-intelligence facts for a single file.
//!
//! These are the per-file **facts** the scanner's second pass caches as a content-addressed
//! `<hash>.rref.msgpack` blob: intra-file resolved edges plus this file's own import/export list.
//! They are a pure function of the file's bytes, so content-addressing is valid — a file whose
//! bytes are unchanged skips re-analysis on the next scan.
//!
//! What is deliberately NOT stored here is any *cross-file* resolved edge: the second pass
//! recomputes only the join (an importer's [`ImportEdge`] → the matching [`ExportEdge`] in the
//! resolved target file) each scan and writes the result straight to the Fjall `refs_by_def`
//! index. That keeps the blob valid even when an unchanged file's *dependency* moved.
//!
//! The model is unconditional (no feature gate): the tree-sitter `locals` engine
//! ([`crate::extract::locals`]) populates `intra` for any language, while the oxc engine
//! ([`crate::intel::js`], `code-intel-js`) additionally fills `imports`/`exports` for JS/TS.

use serde::{Deserialize, Serialize};

/// Schema version for the resolution blob. Shares the single source of truth with the other
/// blobs so a bump wipes + rebuilds resolution alongside L1/L2.
pub use crate::extract::SCHEMA_VER;

/// Resolver epoch: what [`crate::extract::EXTRACT_EPOCH`] is to the outline blobs, for the
/// resolution blobs. Bump it when a resolver change alters the facts an UNCHANGED file should yield
/// without changing how they serialize; a blob stamped with an older epoch reads as absent, so the
/// resolve pass recomputes and overwrites it. Blobs written before the field existed read as `0`.
///
/// History:
/// - 1: JS/TS side-effect imports, namespace imports and re-exports are recorded as
///   [`ImportEdge::module_only`] imports.
pub const RESOLVE_EPOCH: u16 = 1;

/// A resolved intra-file reference edge: a use identifier binds to a definition in the SAME file.
/// Both endpoints are byte spans into the file. Cross-file edges are never stored here (see the
/// module docs) — they are derived into the index at scan time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedEdge {
    pub use_start: u32,
    pub use_end: u32,
    pub def_start: u32,
    pub def_end: u32,
}

/// An import binding this file introduces: the local name, the module specifier it came from, and
/// the imported name in the source module (`None` for default / namespace). Feeds the cross-file
/// join's importer side.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportEdge {
    pub local: String,
    pub specifier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported: Option<String>,
    /// Type-only import (`import type`) — runtime-erased, so the join must not emit a runtime edge.
    #[serde(default)]
    pub is_type: bool,
    pub local_start: u32,
    /// An import that binds no name: a side-effect import (`import './x'`), a namespace import
    /// (`import * as ns from`) or a re-export (`export * from`, `export { x } from`). It is a file
    /// edge for the import graph and never joins an export; `local` is empty and `local_start` is the
    /// specifier's offset.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub module_only: bool,
}

/// A name this file exports. Feeds the cross-file join's target side.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportEdge {
    pub name: String,
    pub name_start: u32,
}

/// Per-file resolution facts — a pure function of the file's bytes, hence content-addressable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileResolvedRefs {
    pub schema_ver: u16,
    /// The [`RESOLVE_EPOCH`] the facts were computed under.
    #[serde(default)]
    pub resolve_epoch: u16,
    pub language: String,
    /// Intra-file resolved reference edges (use span → in-file definition span).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intra: Vec<ResolvedEdge>,
    /// Import bindings this file introduces (cross-file join source side).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<ImportEdge>,
    /// Names this file exports (cross-file join target side).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<ExportEdge>,
}

impl FileResolvedRefs {
    /// A resolution record carrying the current schema version and language, ready to fill.
    pub fn new(language: impl Into<String>) -> Self {
        Self {
            schema_ver: SCHEMA_VER,
            resolve_epoch: RESOLVE_EPOCH,
            language: language.into(),
            intra: Vec::new(),
            imports: Vec::new(),
            exports: Vec::new(),
        }
    }

    /// True when this file yielded no resolution facts at all — the second pass can skip writing
    /// a blob and any index entries for it.
    pub fn is_empty(&self) -> bool {
        self.intra.is_empty() && self.imports.is_empty() && self.exports.is_empty()
    }
}
