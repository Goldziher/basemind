//! Types of `basemind code migration check` (#130, ADR-0047 decision 1): the arguments the CLI
//! sends to the daemon by name and the report it renders. CLI-only, so no field doc here is a
//! tool schema description.

use serde::{Deserialize, Serialize};

use super::LifecycleNotice;
use crate::path::RelPath;

/// Arguments of `code_migration_check`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MigrationCheckParams {
    /// The migration to check: `.basemind/migrations/<name>.toml`. `None` checks every rule file.
    #[serde(default)]
    pub name: Option<String>,
    /// Also give each file's remaining uses as merged read ranges (5 lines of context).
    #[serde(default)]
    pub ranges: Option<bool>,
}

/// The report of `code_migration_check`: one entry per migration checked.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MigrationCheckResponse {
    pub migrations: Vec<MigrationReport>,
    /// Lifecycle notice when the index is not fully ready; absent when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    /// Server-side handler latency in microseconds ([`crate::engine::timing`]).
    #[serde(default)]
    pub elapsed_us: u64,
}

/// One migration's rules, totals and recorded progress.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MigrationReport {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Lines that still hold an old form, summed over the rules.
    pub remaining: u32,
    /// Lines that hold a new form, summed over the rules that declare `expect`.
    pub expected: u32,
    /// Rules that declare `expect` and found none of it: a presence assertion that failed.
    pub failed_rules: u32,
    pub rules: Vec<RuleReport>,
    /// This run against the runs recorded before it.
    pub progress: Progress,
}

/// One rule's answer.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RuleReport {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance: Option<String>,
    /// Indexed code files the rule's `include` / `exclude` select; `0` means it checked nothing.
    pub files_checked: u32,
    /// Selected files a `text` or `import` pattern could not read (gone, too large, not UTF-8).
    #[serde(skip_serializing_if = "is_zero")]
    pub skipped_files: u32,
    /// Lines holding an old form (`match`).
    pub remaining: u32,
    /// Files holding an old form.
    pub remaining_files: u32,
    /// Lines holding a new form (`expect`); absent when the rule declares no `expect`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<u32>,
    /// Files holding a new form; absent when the rule declares no `expect`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_files: Option<u32>,
    /// `expected / (expected + remaining)`, to three decimals; absent without `expect` or hits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migrated_share: Option<f64>,
    /// The rule declares `expect` and none of it was found.
    pub failed: bool,
    /// Files holding both an old and a new form, sorted (at most [`MIXED_FILES_MAX`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mixed_files: Vec<RelPath>,
    /// Files with remaining uses, most uses first (at most [`FILES_MAX`]).
    pub files: Vec<FileUses>,
    /// Files with remaining uses left out of `files` by its cap.
    #[serde(skip_serializing_if = "is_zero")]
    pub omitted_files: u32,
}

/// The remaining uses of one rule in one file.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FileUses {
    pub path: RelPath,
    /// Lines holding an old form.
    pub count: u32,
    /// The first [`LINES_MAX`] of those lines, 1-based and ascending.
    pub lines: Vec<u32>,
    /// With `ranges`: every one of those lines widened by 5 lines of context and merged
    /// (`"12-20,31-40"`), each span one `code_read` of `line` to `end_line`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ranges: Option<String>,
}

/// Progress of a migration across checks, from the history kept in the workspace cache directory.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Progress {
    /// Runs recorded, this one included.
    pub runs: u32,
    /// The first run recorded; absent when this is the first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first: Option<RunPoint>,
    /// The run recorded just before this one; absent when this is the first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<RunPoint>,
    /// The latest runs, oldest first, this one last (at most [`RECENT_RUNS`]).
    pub recent: Vec<RunPoint>,
    /// Where the history is kept; absent when it could not be written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_file: Option<String>,
}

/// One recorded check of a migration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunPoint {
    /// Seconds since the Unix epoch.
    pub at: u64,
    /// Short HEAD of the repository at the run; absent outside git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub remaining: u32,
    pub expected: u32,
}

/// Files a rule lists in [`RuleReport::files`].
pub(crate) const FILES_MAX: usize = 500;
/// Lines a file lists in [`FileUses::lines`].
pub(crate) const LINES_MAX: usize = 50;
/// Files a rule lists in [`RuleReport::mixed_files`].
pub(crate) const MIXED_FILES_MAX: usize = 100;
/// Runs [`Progress::recent`] holds.
pub(crate) const RECENT_RUNS: usize = 10;

fn is_zero(n: &u32) -> bool {
    *n == 0
}
