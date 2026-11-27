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
    /// Import hops from the nearest target: 1 for a direct importer (or, in `dependencies`, a
    /// direct import).
    pub depth: u8,
    /// The file one hop nearer the targets through which it was reached.
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
    /// 1-based line the definition starts on.
    pub line: u32,
}

/// One call site of a symbol target, or of a definition holding such a site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImpactCaller {
    pub path: RelPath,
    /// 1-based line of the call site.
    pub line: u32,
    /// Call hops from the target: 1 for a direct caller.
    pub depth: u8,
    /// The callable definition holding the site; absent for a site outside any (module level).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// The definition the site calls: the target at depth 1, else the `symbol` one hop nearer.
    pub calls: String,
    /// How the site was established: `same_file` or `imported` (bound to `calls` by the resolution
    /// facts), `path_match` (an HTTP call linked to a route by its path) or `name_only` (the callee
    /// is named like `calls`, nothing more).
    pub resolution: &'static str,
    /// The site is proven to call `calls`: only `same_file` and `imported`. Only a proven caller
    /// is followed to the next depth.
    pub proven: bool,
}

/// One call a symbol target makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImpactCallee {
    /// The definition the call is in.
    pub from: RelPath,
    /// The name called.
    pub name: String,
    /// The file defining it, when the resolution facts bind the call to a definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<RelPath>,
    /// `same_file` or `imported` when `path` is set, else `name_only`.
    pub resolution: &'static str,
    pub proven: bool,
}

/// What an impacted file is, judged from its path, in the order its work is done within a depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImpactBucket {
    /// Source that ships.
    Prod,
    /// Configuration, build and automation files.
    Config,
    /// Tests and their fixtures.
    Test,
    /// Code the repository does not own: vendored or third-party directories, files indexed from
    /// outside the root.
    External,
}

/// Impacted files per [`ImpactBucket`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ImpactBuckets {
    pub prod: usize,
    pub config: usize,
    pub test: usize,
    pub external: usize,
}

impl ImpactBuckets {
    /// Count one more file in `bucket`.
    pub fn add(&mut self, bucket: ImpactBucket) {
        match bucket {
            ImpactBucket::Prod => self.prod += 1,
            ImpactBucket::Config => self.config += 1,
            ImpactBucket::Test => self.test += 1,
            ImpactBucket::External => self.external += 1,
        }
    }
}

/// One file in the order of work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImpactStep {
    pub path: RelPath,
    /// Hops from the nearest target, by import or call: 0 for a target itself.
    pub depth: u8,
    pub bucket: ImpactBucket,
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
    /// Symbol target: the call sites of its definitions, transitively to `depth` over proven
    /// edges, by depth then path and line. A `name_only` row is listed and never followed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callers: Vec<ImpactCaller>,
    /// `depth` (callers lie deeper), `limit` (more callers than are listed) or `scan_cap` (more
    /// definitions to look up than the walk examines).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callers_truncated_reason: Option<&'static str>,
    /// The files the targets import, transitively to `depth`, by depth and then path.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<ImpactHit>,
    /// `depth` or `scan_cap`, as for `truncated_reason`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependencies_truncated_reason: Option<&'static str>,
    /// Symbol target: the calls its definitions' bodies make, each graded like a caller.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<ImpactCallee>,
    /// The files impacted, targets excluded: the importers reached and the files holding a call
    /// site, each counted once. `--fail-above` compares against it.
    pub impacted_files: usize,
    /// `impacted_files` per bucket.
    pub buckets: ImpactBuckets,
    /// What to change, first to last: the targets, then the impacted files nearest first, production
    /// before configuration before tests before external code within a depth.
    pub order: Vec<ImpactStep>,
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
pub const RUST_CYCLE_NOTE: &str =
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
