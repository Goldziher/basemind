//! Request and answer of `basemind code deadcode` (#123). CLI-only (ADR-0047 decision 1): these
//! types never reach `tools/list`, so their docs are for the reader of the code, not an agent's
//! schema.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::LifecycleNotice;
use super::graph::GraphCoverage;
use crate::path::RelPath;

/// `basemind code deadcode`: files no entry point reaches, symbols nothing names, and imports that
/// resolve to nothing, each with a confidence tier and its reason.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeadcodeParams {
    /// Only findings in a file at this path or under this directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// One kind of finding: `files`, `symbols` or `imports`; all three when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The lowest confidence reported: `high`, `medium` or `low` (the default, every finding).
    /// `--fail-on-dead-code` gates on `high` findings unless a tier is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Findings listed per kind (default 100, at most 10,000); the counts cover the rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// How sure a finding is. Ordered from the least to the most sure, so `>=` is "at least as sure".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// The index cannot see a use the language allows (dispatch, reflection, decorators, macros, an
    /// importer outside the repository): check before deleting.
    Low,
    /// Some evidence of use is missing but the name or file is still mentioned.
    Medium,
    /// Nothing in the indexed code names or imports it.
    High,
}

impl Tier {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            _ => None,
        }
    }
}

/// Findings of one kind per tier, before `limit`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TierCounts {
    pub high: usize,
    pub medium: usize,
    pub low: usize,
}

impl TierCounts {
    pub(crate) fn add(&mut self, tier: Tier) {
        match tier {
            Tier::High => self.high += 1,
            Tier::Medium => self.medium += 1,
            Tier::Low => self.low += 1,
        }
    }
}

/// A file no entry point reaches through the import graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeadFile {
    pub path: RelPath,
    pub language: String,
    pub tier: Tier,
    pub reason: String,
}

/// A definition whose name nothing else uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeadSymbol {
    pub path: RelPath,
    /// 1-based line of the definition.
    pub line: u32,
    pub name: String,
    pub kind: &'static str,
    pub language: String,
    pub tier: Tier,
    pub reason: String,
}

/// An import whose specifier resolves to no file of the repository and no package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeadImport {
    pub path: RelPath,
    /// 1-based line of the first occurrence of the specifier, when the file still holds its
    /// indexed content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub specifier: String,
    pub language: String,
    pub tier: Tier,
    pub reason: String,
}

/// What `deadcode` analysed in one language, and what it could not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeadcodeCoverage {
    pub language: String,
    pub files: usize,
    /// The kinds of finding analysed for this language (`files`, `symbols`, `imports`).
    pub analysed: Vec<&'static str>,
    /// What the analysis of this language cannot see.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The answer of `basemind code deadcode`.
#[derive(Debug, Serialize)]
pub struct DeadcodeResponse {
    /// Findings per kind and tier, after the `path`, `kind` and `tier` filters and before `limit`.
    pub counts: BTreeMap<&'static str, TierCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<DeadFile>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols: Option<Vec<DeadSymbol>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imports: Option<Vec<DeadImport>>,
    /// Findings per kind that `limit` left out.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub omitted: BTreeMap<&'static str, usize>,
    /// Files the manifests' targets, `[analysis] entry_points` and the `<script>`s of a package's
    /// `index.html` name. The walk also starts from each language's conventional entries (tests,
    /// examples, `__main__.py`, Go `package main`, …), which this count leaves out.
    pub entry_points: usize,
    /// Per indexed code language, what was analysed.
    pub coverage: Vec<DeadcodeCoverage>,
    /// What the import graph knows.
    pub graph: GraphCoverage,
    /// What the analysis cannot see that bears on this answer.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Why a requested kind could not be judged in full (the index is not ready, the resolve pass
    /// is unfinished, no resolver, unreadable files). `--fail-on-dead-code` fails on any of them
    /// rather than pass on a report that examined nothing.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub incomplete: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    pub elapsed_us: u64,
}
