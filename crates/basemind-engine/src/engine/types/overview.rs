//! Arguments and answer of `basemind overview` (#131): a repository's shape in one call, counts and
//! names only. A CLI-only command (ADR-0047 decision 1), so these types are no schema.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::LifecycleNotice;
use super::files::SizedFile;
use crate::engine::manifests::{Manifest, Target};
use crate::path::RelPath;

/// Arguments of `overview`.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OverviewParams {
    /// Comma-separated sections to report; all when absent.
    #[serde(default)]
    pub sections: Option<String>,
    /// Rows per list section (default 10, at most 100).
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Answer of `overview`. Every section is absent when not asked for.
#[derive(Debug, Serialize)]
pub(crate) struct OverviewResponse {
    /// Indexed code files, their lines and bytes.
    pub files: usize,
    pub lines: u64,
    pub bytes: u64,
    /// Per language, most lines first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub languages: Option<Vec<LanguageRow>>,
    /// Top-level directories (`.` for files at the root), most lines first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<Vec<DirectoryRow>>,
    /// Build manifests, by path, without their targets (see `entry_points`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifests: Option<Vec<Manifest>>,
    /// The targets the manifests declare, except tests, benchmarks and examples.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_points: Option<Vec<EntryPoint>>,
    /// Tests, benchmarks and examples the manifests declare, counted per kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_points_counted: Option<BTreeMap<&'static str, usize>>,
    /// The longest code files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub largest: Option<Vec<SizedFile>>,
    /// The files most other files import, over the runtime import graph (ADR-0047 decision 4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub most_imported: Option<Vec<ImportedFile>>,
    /// What the import graph behind `most_imported` covers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_graph: Option<GraphCoverage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routes: Option<Routes>,
    /// The dominant case of file names and of each symbol kind's names, per language.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub naming: Option<Vec<NamingRow>>,
    /// The latest commits; absent outside a git repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent: Option<Vec<RecentCommit>>,
    /// Rows `limit` left out, per section.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub omitted: BTreeMap<&'static str, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LanguageRow {
    pub language: String,
    pub files: usize,
    pub lines: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DirectoryRow {
    pub path: String,
    pub files: usize,
    pub lines: u64,
    /// The language holding most of the directory's lines.
    pub language: String,
}

/// One target, with the manifest declaring it.
#[derive(Debug, Serialize)]
pub(crate) struct EntryPoint {
    pub manifest: RelPath,
    #[serde(flatten)]
    pub target: Target,
}

/// What the import graph covers.
#[derive(Debug, Serialize)]
pub(crate) struct GraphCoverage {
    /// Indexed languages with no import resolver: their files take no part in the count.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub no_resolver: Vec<String>,
    /// `false` while the import resolution of the last scan is incomplete: counts may be low.
    pub complete: bool,
}

/// A file and how many files import it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub(crate) struct ImportedFile {
    pub path: RelPath,
    pub importers: usize,
}

/// HTTP route definitions extracted from the code (ADR-0048).
#[derive(Debug, Default, Serialize)]
pub(crate) struct Routes {
    pub count: usize,
    pub files: usize,
    /// Routes per HTTP method.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub methods: BTreeMap<String, usize>,
}

/// The naming conventions of one language.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct NamingRow {
    pub language: String,
    /// File names, without their extension.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<CaseShare>,
    /// Per symbol kind.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub symbols: BTreeMap<&'static str, CaseShare>,
}

/// The most common case among a set of names, and its share of the names that have a case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CaseShare {
    pub case: &'static str,
    pub percent: u8,
    /// Names counted: those with a case (a single lower-case word has none).
    pub names: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct RecentCommit {
    pub sha: String,
    pub summary: String,
    pub author: String,
    pub time_unix: i64,
}
