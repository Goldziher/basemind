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
//! (`crate::intel::js`, `code-intel-js`) additionally fills `imports`/`exports`/`reexports` for JS/TS.

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
/// - 2: Python `import a.b` records one import of the whole dotted module bound to `a`, and aliased
///   imports (`import a.b as n`, `from m import x as y`) record theirs bound to the alias.
/// - 3: JS/TS `export { a as b } from` and `export * from` re-exports are recorded as
///   [`ReexportEdge`]s, which the cross-file join follows through barrel files.
pub const RESOLVE_EPOCH: u16 = 3;

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

/// A re-export this file forwards from another module: `export { a as b } from './m'` (named) or
/// `export * from './m'` (star). Feeds the cross-file join's hops through barrel files: a name the
/// file does not define itself resolves through the module it forwards from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReexportEdge {
    /// Module specifier the forwarded names come from, e.g. `./impl`.
    pub specifier: String,
    /// Start byte of the module specifier.
    pub specifier_start: u32,
    /// The one name a named re-export forwards; `None` for `export * from`, which forwards every
    /// name of the source module except `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named: Option<NamedReexport>,
    /// Type-only re-export (`export type { T } from`, `export type * from`) — runtime-erased, so the
    /// join does not follow it.
    #[serde(default)]
    pub is_type: bool,
}

/// The names of a named re-export `export { imported as exported } from './m'`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamedReexport {
    /// The name this file exports (`b`).
    pub exported: String,
    /// The name in the source module (`a`).
    pub imported: String,
}

impl ReexportEdge {
    /// The module request behind this re-export, as the `module_only` [`ImportEdge`] a
    /// `crate::intel::resolver::SpecifierResolver` resolves.
    pub fn module_request(&self) -> ImportEdge {
        ImportEdge {
            local: String::new(),
            specifier: self.specifier.clone(),
            imported: None,
            is_type: self.is_type,
            local_start: self.specifier_start,
            module_only: true,
        }
    }

    /// The name to look up in the source module when this re-export provides `wanted`, or `None`
    /// when it does not forward `wanted`.
    pub fn forwards<'a>(&'a self, wanted: &'a str) -> Option<&'a str> {
        match &self.named {
            Some(named) => (named.exported == wanted).then_some(named.imported.as_str()),
            None => (wanted != "default").then_some(wanted),
        }
    }
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
    /// Names this file re-exports from other modules (cross-file join hops).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reexports: Vec<ReexportEdge>,
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
            reexports: Vec::new(),
        }
    }

    /// True when this file yielded no resolution facts at all — the second pass can skip writing
    /// a blob and any index entries for it.
    pub fn is_empty(&self) -> bool {
        self.intra.is_empty() && self.imports.is_empty() && self.exports.is_empty() && self.reexports.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reexport(named: Option<(&str, &str)>) -> ReexportEdge {
        ReexportEdge {
            specifier: "./impl".to_string(),
            specifier_start: 7,
            named: named.map(|(exported, imported)| NamedReexport {
                exported: exported.to_string(),
                imported: imported.to_string(),
            }),
            is_type: false,
        }
    }

    #[test]
    fn a_named_reexport_forwards_only_its_exported_name_under_the_source_name() {
        let renamed = reexport(Some(("tool", "helper")));
        assert_eq!(renamed.forwards("tool"), Some("helper"));
        assert_eq!(renamed.forwards("helper"), None);
    }

    #[test]
    fn a_star_reexport_forwards_every_name_but_default() {
        let star = reexport(None);
        assert_eq!(star.forwards("helper"), Some("helper"));
        assert_eq!(star.forwards("default"), None);
    }

    #[test]
    fn the_module_request_is_a_module_only_import_of_the_specifier() {
        assert_eq!(
            reexport(None).module_request(),
            ImportEdge {
                local: String::new(),
                specifier: "./impl".to_string(),
                imported: None,
                is_type: false,
                local_start: 7,
                module_only: true,
            }
        );
    }
}
