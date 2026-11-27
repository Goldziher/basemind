//! Types of `basemind git hotspots` (CLI-only, ADR-0047).

use serde::{Deserialize, Serialize};

use crate::path::RelPath;

/// Arguments of `basemind git hotspots` (CLI-only, ADR-0047).
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HotspotsParams {
    /// Window ending at HEAD's commit time: `<N>d`, `<N>w` or `<N>y`; default `90d`.
    #[serde(default)]
    pub since: Option<String>,
    /// Only files at this path or under this directory.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Rows returned; default 20, at most 200.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// The smallest value in each signal's top slice; absent when no file has the signal.
#[derive(Debug, Default, Serialize)]
pub(crate) struct HotspotThresholds {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authors: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longest_function: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importers: Option<u32>,
}

/// One file where two or more signals are in their top slice. Every number is raw; there is no score.
#[derive(Debug, Serialize)]
pub(crate) struct HotspotRow {
    pub path: RelPath,
    /// The signals whose top slice holds this file, in [`HotspotThresholds`] order.
    pub signals: Vec<&'static str>,
    /// Commits in the window that touched the file.
    pub commits: u32,
    /// Distinct author emails (the name when a commit has none) among those commits.
    pub authors: u32,
    pub lines: u32,
    pub symbols: u32,
    /// Lines of the longest function or method (0 when it has none); `null` when the file changed
    /// since the scan.
    pub longest_function: Option<u32>,
    /// `name:line` of that function.
    pub longest_function_at: Option<String>,
    /// Files importing this one (runtime imports); `null` when no resolver reads its language.
    pub importers: Option<u32>,
}

/// The answer of `basemind git hotspots`.
#[derive(Debug, Serialize)]
pub(crate) struct HotspotsResponse {
    /// The window as asked (`90d`).
    pub since: String,
    /// Unix seconds: the window is `[window_start, window_end]`, ending at HEAD's commit time.
    pub window_start: i64,
    pub window_end: i64,
    /// Commits in the window.
    pub commits: u32,
    /// Indexed files in scope that a commit in the window touched: the population ranked.
    pub files_considered: u32,
    /// The top slice of each signal, as a percentage of `files_considered`.
    pub slice_percent: u32,
    pub thresholds: HotspotThresholds,
    /// Files with two or more signals in their top slice, before `limit`.
    pub total: u32,
    pub files: Vec<HotspotRow>,
    /// Set when the commit walk stopped before the window's start.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// Signals that were not measured everywhere, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub elapsed_us: u64,
}
