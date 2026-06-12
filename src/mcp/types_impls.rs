//! Param and response types for the `find_implementations` MCP tool.
//!
//! Split out of `types.rs` to keep that file under the 1000-line cap.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::cursor::Cursor;
use crate::path::RelPath;

// ─── Params ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindImplementationsParams {
    /// The trait / interface / base-class name to find implementations of.
    pub trait_name: String,
    /// Optional language filter (e.g. "rust", "typescript"). When set, only matches
    /// from files in that language are returned.
    #[serde(default)]
    pub language: Option<String>,
    /// Cap on results returned. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Stable across rescans
    /// because the underlying Fjall keys are content-addressed.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

// ─── Response ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FindImplementationsResponse {
    pub trait_name: String,
    pub total: usize,
    /// True when `total` was capped by `scan_cap` and more matches exist on disk.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub total_is_partial: bool,
    pub hits: Vec<ImplementationHit>,
    /// Opaque cursor to pass back on the next call when more results are available.
    /// Stable across rescans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ImplementationHit {
    pub path: RelPath,
    pub trait_name: String,
    pub impl_type: String,
    /// 1-based row of the `impl`/`class`/`extends` declaration.
    pub start_row: u32,
    /// 0-based byte column from the start of the line.
    pub start_col: u32,
}
