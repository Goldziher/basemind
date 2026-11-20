//! Requests and answers of the import-graph analysis commands `basemind code impact` (#121) and
//! `basemind code cycles` (#135). Both are CLI-only (ADR-0047 decision 1): these types never reach
//! `tools/list`, so their docs are for the reader of the code, not an agent's schema.

use serde::{Deserialize, Serialize};

use super::LifecycleNotice;
use crate::path::RelPath;

/// `basemind code impact`: the files that depend on a file, on the files defining a symbol, or on
/// the files a change touches. Exactly one of `path`, `symbol` and `diff` names the target.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactParams {
    /// A repository-relative file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<RelPath>,
    /// A symbol name: every file defining it is a target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// A change: a revision's merge base with `HEAD` against the working tree (`""` for `HEAD`),
    /// `A..B` or `A...B`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Importer levels to walk, 1 to 5 (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u8>,
    /// Also follow type-only imports.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_type_imports: bool,
    /// Also follow Rust `mod x;` declarations.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_module_decls: bool,
}

/// One file the change can break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImpactHit {
    pub path: RelPath,
    /// Import hops from the nearest target: 1 for a direct importer.
    pub depth: u8,
    /// The file one hop nearer the targets that it imports.
    pub via: RelPath,
    /// The file is part of an import cycle.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub in_cycle: bool,
}

/// A definition a symbol target named.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImpactDefinition {
    pub path: RelPath,
    pub kind: &'static str,
}

/// The answer of `basemind code impact`.
#[derive(Debug, Serialize)]
pub struct ImpactResponse {
    /// `file`, `symbol` or `diff`.
    pub target: &'static str,
    /// The files the walk started from, in path order.
    pub targets: Vec<RelPath>,
    /// The definitions of a symbol target.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub definitions: Vec<ImpactDefinition>,
    /// Files the change touches, indexed or not: `0` means nothing changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_files: Option<usize>,
    /// Targets that are part of an import cycle.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub targets_in_cycles: Vec<RelPath>,
    /// Targets no resolved import reaches or leaves: the graph knows nothing of them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_in_graph: Vec<RelPath>,
    pub depth: u8,
    /// Files reached, by depth and then path.
    pub affected_files: usize,
    pub affected: Vec<ImpactHit>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `depth` (files lie deeper) or `scan_cap` (more files than the walk reports).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    pub coverage: GraphCoverage,
    /// What the graph cannot see that bears on this answer.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    pub elapsed_us: u64,
}

/// `basemind code cycles`: the import cycles, or with `check_from` and `check_to` whether a new
/// import would close one.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CyclesParams {
    /// Only cycles with a file at this path or under this directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Leave out cycles of more files than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<u32>,
    /// The file that would add the import; needs `check_to`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_from: Option<RelPath>,
    /// The file it would import; needs `check_from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_to: Option<RelPath>,
    /// Also follow type-only imports.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_type_imports: bool,
    /// Also follow Rust `mod x;` declarations.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_module_decls: bool,
}

/// The [`CycleRow::note`] of a cycle whose files are all Rust: modules of one crate, which the
/// compiler accepts. `--fail-on-cycle` fails on such a cycle only with `--strict`.
pub(crate) const RUST_CYCLE_NOTE: &str =
    "rust: modules of one crate may use each other; the compiler accepts this cycle (information)";

/// One import cycle: a strongly connected component of the import graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CycleRow {
    pub size: usize,
    /// One concrete import chain that closes the cycle: `a.rs:12 -> b.rs:40 -> a.rs`, each line the
    /// import of the next file (left out where the file changed since the scan).
    pub chain: String,
    /// Every file of the component, in path order.
    pub files: Vec<RelPath>,
    /// What a cycle means in the language of its files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'static str>,
}

/// The import cycle list.
#[derive(Debug, Serialize)]
pub struct CyclesList {
    /// Cycles reported, smallest first.
    pub cycle_count: usize,
    /// Files in the reported cycles.
    pub files_in_cycles: usize,
    /// Cycles left out by `max_size`.
    #[serde(skip_serializing_if = "is_zero")]
    pub over_max_size: usize,
    pub cycles: Vec<CycleRow>,
    pub coverage: GraphCoverage,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    pub elapsed_us: u64,
}

/// Whether importing `to` from `from` would close a cycle.
#[derive(Debug, Serialize)]
pub struct CycleCheck {
    pub from: RelPath,
    pub to: RelPath,
    pub would_close_cycle: bool,
    /// The existing chain from `to` back to `from` that the new import would close, as
    /// `to.rs:3 -> mid.rs:8 -> from.rs`; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closing_chain: Option<String>,
    pub coverage: GraphCoverage,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    pub elapsed_us: u64,
}

/// The answer of `basemind code cycles`.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum CyclesResponse {
    List(CyclesList),
    Check(CycleCheck),
}

/// What the import graph knows, which every graph answer reports so that "no edges" is never read
/// as "no dependents" or "no cycles".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GraphCoverage {
    /// Files in the graph: those with a resolved import or imported through one.
    pub files: usize,
    /// Resolved edges, every tier.
    pub edges: usize,
    /// Per language with a resolver, what it resolved.
    pub languages: Vec<LanguageCoverageRow>,
    /// Indexed languages with no resolver: their imports are not in the graph.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub without_resolver: Vec<String>,
}

/// One language's share of [`GraphCoverage`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LanguageCoverageRow {
    pub language: String,
    pub files: u32,
    /// Files the resolver read an import of.
    pub importers: u32,
    pub edges: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub type_only_edges: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub module_decl_edges: u32,
    /// Imports of modules outside the repository.
    pub external: u32,
    /// Imports naming a repository file that no indexed file matches.
    pub unresolved: u32,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}
