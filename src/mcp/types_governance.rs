//! Request / response shapes for the `memory_audit` MCP tool.
//!
//! `MemoryAuditParams` is always compiled so the `not_enabled` fallback in
//! `tools_governance.rs` can deserialize the params correctly.  All response
//! types are `#[cfg(feature = "memory")]`-gated because they reference
//! `VerifyState`, which lives behind that gate.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::types_memory::Visibility;

/// Parameters for the `memory_audit` tool. All fields default so an empty `{}` call
/// runs a full group-scope audit with a limit of 100.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryAuditParams {
    /// When set, audit exactly this one key instead of the whole scope.
    #[serde(default)]
    pub key: Option<String>,
    /// Memory tier to audit: `group` (shared, default) or `individual` (per-agent).
    #[serde(default)]
    pub visibility: Visibility,
    /// When `true`, compute verdicts and return them but do NOT persist any mutations
    /// (no importance decay, no archive, no `verified` field updates).
    #[serde(default)]
    pub dry_run: bool,
    /// Maximum number of records to audit (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<u32>,
    /// When `true`, also scan the `memory_archive` keyspace (archived/stale records).
    #[serde(default)]
    pub include_archived: bool,
}

/// Per-record audit outcome.
#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct AuditResult {
    /// The memory key.
    pub key: String,
    /// Verdict string: `"verified"`, `"stale"`, or `"unverified"`.
    pub state: String,
    /// Human-readable reasons for the verdict (empty when `Verified` or `Unverified`
    /// with no code references to check).
    pub reasons: Vec<String>,
    /// True when the record was moved to `memory_archive` during this audit run
    /// (Stale for > 90 days). Only set when `dry_run = false`.
    pub archived: bool,
}

/// Response from `memory_audit`.
#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct MemoryAuditResponse {
    /// Number of records examined.
    pub audited: usize,
    /// Per-record results.
    pub results: Vec<AuditResult>,
}

/// Internal verdict — not serialised to JSON; only used within the governance helpers.
#[cfg(feature = "memory")]
pub(super) struct AuditVerdict {
    pub state: super::types_memory::VerifyState,
    pub reasons: Vec<String>,
}

#[cfg(feature = "memory")]
impl AuditVerdict {
    pub fn state_str(&self) -> &'static str {
        match self.state {
            super::types_memory::VerifyState::Unverified => "unverified",
            super::types_memory::VerifyState::Verified => "verified",
            super::types_memory::VerifyState::Stale => "stale",
        }
    }
}
