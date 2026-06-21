//! Request / response shapes for the governance MCP tools (`memory_audit`, `proposals_mine`,
//! `proposals_list`, `proposal_accept`, `proposal_reject`).
//!
//! `MemoryAuditParams` is always compiled so the `not_enabled` fallback in
//! `tools_governance.rs` can deserialize the params correctly.  All response
//! types are `#[cfg(feature = "memory")]`-gated because they reference
//! `VerifyState`, which lives behind that gate.
//!
//! Proposal param structs are always compiled (shims need them regardless of the feature gate).
//! `ProposalRecord` and response types are `#[cfg(feature = "memory")]`-gated.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::cursor::Cursor;
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

// ─── W11 proposal types ───────────────────────────────────────────────────────

/// On-disk record for a co-change skill proposal. Stored as msgpack in the `proposals`
/// Fjall keyspace. `#[serde(default)]` on new fields ensures old blobs decode cleanly.
#[cfg(feature = "memory")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ProposalRecord {
    /// Proposal kind byte (mirrors `PROPOSAL_KIND_*` constants). Always `1` for skill proposals.
    pub kind: u8,
    /// Sorted list of co-changing files (the cluster). Used for content-addressed id and
    /// for populating `provenance.files` when the proposal is accepted.
    pub files: Vec<crate::path::RelPath>,
    /// Number of commits in which ALL files in the cluster co-changed.
    pub support: u32,
    /// Commit window over which support was measured.
    pub window: u32,
    /// `support / freq[anchor_file]` — fraction of anchor's commits that touched the cluster.
    pub confidence: f32,
    /// Human-readable description emitted when the proposal was mined.
    pub description: String,
    /// Git-derived importance in `[0,1)` (support/scope); never an LLM rating.
    pub importance: f32,
    /// Microsecond timestamp of when this proposal was mined.
    pub created_at: i64,
}

/// Parameters for `proposals_mine`. All thresholds are optional with sensible defaults.
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ProposalsMineParams {
    /// Number of recent commits to inspect (default 200, max 2000).
    #[serde(default)]
    pub window: Option<u32>,
    /// Minimum co-change count for a pair to be emitted (default 5).
    #[serde(default)]
    pub min_support: Option<u32>,
    /// Minimum confidence (support / anchor_freq) for a pair to be emitted (default 0.6).
    #[serde(default)]
    pub min_confidence: Option<f32>,
    /// Skip commits that touch more than this many files (avoids bulk/vendor commits
    /// dominating the co-change map; default 25).
    #[serde(default)]
    pub max_files_per_commit: Option<u32>,
}

/// Parameters for `proposals_list`. Paginates via Fjall-backed cursors (stable across rescans).
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ProposalsListParams {
    /// Filter by proposal kind: `"skill"` or `"memory"`. Omit for all non-tombstone proposals.
    #[serde(default)]
    pub kind: Option<String>,
    /// Cap the number of results (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Resume token returned by a previous call's `next_cursor`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

/// Parameters for `proposal_accept`. Promotes the proposal to a searchable skill memory.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ProposalAcceptParams {
    /// The proposal id (hex blake3 of the sorted file-set), as returned by `proposals_list`.
    pub id: String,
    /// Override the auto-derived memory key. Default: `"skill/cochange-<short_id>"`.
    #[serde(default)]
    pub key: Option<String>,
}

/// Parameters for `proposal_reject`. Deletes the proposal and writes a tombstone so
/// re-mining will not resurface the same candidate.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ProposalRejectParams {
    /// The proposal id, as returned by `proposals_list`.
    pub id: String,
    /// Optional human-readable reason (stored only in logs; not persisted).
    #[serde(default)]
    pub reason: Option<String>,
}

/// One entry in the `proposals_list` response.
#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct ProposalEntry {
    pub id: String,
    pub kind: u8,
    pub files: Vec<crate::path::RelPath>,
    pub support: u32,
    pub window: u32,
    pub confidence: f32,
    pub description: String,
    pub importance: f32,
    pub created_at: i64,
}

/// Response from `proposals_mine`.
#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct ProposalsMineResponse {
    /// Number of new proposals written (existing proposals for the same candidate are overwritten).
    pub mined: usize,
    /// Window of commits inspected.
    pub window_inspected: u32,
    /// Number of commits skipped due to `max_files_per_commit`.
    pub skipped_bulk: u32,
}

/// Response from `proposals_list`.
#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct ProposalsListResponse {
    pub total: usize,
    pub truncated: bool,
    pub proposals: Vec<ProposalEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
}

/// Response from `proposal_accept`.
#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct ProposalAcceptResponse {
    pub accepted: bool,
    /// The memory key under which the accepted proposal was stored.
    pub memory_key: String,
}

/// Response from `proposal_reject`.
#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct ProposalRejectResponse {
    pub rejected: bool,
}
