//! Request / response shapes for the memory MCP tools (`memory_put` / `_get` / `_list` /
//! `_search` / `_delete`).
//!
//! Split out of `types.rs` to keep that file within the per-file size budget. Parameter structs
//! derive `Deserialize + Serialize + JsonSchema`; response/record structs are `#[cfg(feature =
//! "memory")]`-gated since they only exist when the LanceDB-backed memory store is compiled in.
//! The [`Visibility`] tier selector is always compiled — it is part of every memory param shape.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::cursor::Cursor;
use super::types::default_true;

/// Memory tier selector. `group` (the default) is the shared, cross-agent tier — today's
/// behavior, with an empty owner segment. `individual` scopes the entry to the calling
/// agent (owner = its `AgentId`), so two agents can keep private same-key entries.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Shared, cross-agent memory (owner segment is empty). The default.
    #[default]
    Group,
    /// Per-agent memory (owner segment is the caller's `AgentId`).
    Individual,
}

impl Visibility {
    /// Stable, append-only on-disk ordinal for this tier — matches the `vis_byte`
    /// encoded by [`crate::index::keys::memory_by_key`].
    pub fn vis_byte(self) -> u8 {
        match self {
            Visibility::Group => crate::index::keys::MEMORY_VIS_GROUP,
            Visibility::Individual => crate::index::keys::MEMORY_VIS_INDIVIDUAL,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryPutParams {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub embed: bool,
    /// Memory tier: `group` (shared, default) or `individual` (per-agent).
    #[serde(default)]
    pub visibility: Visibility,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct MemoryPutResponse {
    pub key: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryGetParams {
    pub key: String,
    /// Memory tier: `group` (shared, default) or `individual` (per-agent).
    #[serde(default)]
    pub visibility: Visibility,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryListParams {
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Stable across rescans
    /// because the underlying Fjall keys are content-addressed.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Memory tier: `group` (shared, default) or `individual` (per-agent).
    #[serde(default)]
    pub visibility: Visibility,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct MemoryListResponse {
    pub total: usize,
    pub truncated: bool,
    pub entries: Vec<MemoryEntry>,
    /// Opaque cursor to pass back on the next call when more results are available.
    /// Stable across rescans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemorySearchParams {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub tag: Option<String>,
    /// Memory tier: `group` (shared, default) or `individual` (per-agent). An individual
    /// search never returns another agent's rows; a group search only sees group rows.
    #[serde(default)]
    pub visibility: Visibility,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct MemorySearchHit {
    pub key: String,
    pub value: String,
    pub tags: Vec<String>,
    pub distance: f32,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct MemorySearchResponse {
    pub query: String,
    pub hits: Vec<MemorySearchHit>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryDeleteParams {
    pub key: String,
    /// Memory tier: `group` (shared, default) or `individual` (per-agent).
    #[serde(default)]
    pub visibility: Visibility,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(super) struct MemoryDeleteResponse {
    pub deleted: bool,
}

/// Verification verdict of a memory's code references against the live index, set by the
/// W10 audit engine. `#[serde(default)]` on the record field means pre-W10 blobs (written
/// before this existed) decode as `Unverified` — no schema bump required.
#[cfg(feature = "memory")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum VerifyState {
    /// Never audited — the default for legacy records and freshly-written memories.
    #[default]
    Unverified,
    /// Every code reference resolved against the index as of `last_verified`.
    Verified,
    /// A referenced symbol/file moved, was deleted, or its structural hash changed.
    Stale,
}

/// A code symbol a memory claims to describe. Resolved against the in-RAM map on audit; a
/// `structural_hash` mismatch is what flags the memory `Stale` ("the body this note describes
/// changed") — the code-grounded signal no other memory system has.
#[cfg(feature = "memory")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct SymbolRef {
    pub path: crate::path::RelPath,
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    /// blake3 structural hash (`HashMode::Structural`) captured at write/verify time.
    #[serde(default)]
    pub structural_hash: Option<[u8; 32]>,
}

/// What a memory claims about the codebase — the surface the audit engine verifies. All fields
/// default-empty so a legacy `MemoryRecord` decodes cleanly and simply has nothing to verify.
#[cfg(feature = "memory")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct Provenance {
    #[serde(default)]
    pub symbols: Vec<SymbolRef>,
    #[serde(default)]
    pub files: Vec<crate::path::RelPath>,
    #[serde(default)]
    pub commands: Vec<String>,
}

#[cfg(feature = "memory")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct MemoryRecord {
    pub value: String,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Code references this memory claims (W10). Default-empty for legacy records.
    #[serde(default)]
    pub provenance: Provenance,
    /// Verification verdict from the last audit (W10).
    #[serde(default)]
    pub verified: VerifyState,
    /// Micros of the last audit; `0` = never audited.
    #[serde(default)]
    pub last_verified: i64,
    /// Git-derived importance in `[0,1)`; decays when the memory goes stale. Never an LLM rating.
    #[serde(default)]
    pub importance: f32,
}

#[cfg(all(test, feature = "memory"))]
mod tests {
    use super::*;

    /// A pre-W10 `MemoryRecord` carried only these four fields.
    #[derive(Serialize)]
    struct LegacyMemoryRecord {
        value: String,
        tags: Vec<String>,
        created_at: i64,
        updated_at: i64,
    }

    /// Blob-compat guarantee: a record written before the W10 fields existed must decode into
    /// the current struct with the new fields defaulted. This is what lets W10 ship without an
    /// `INDEX_SCHEMA_VER` / `RELEASE_MINOR` bump — old `.basemind` memory blobs stay readable.
    #[test]
    fn should_decode_legacy_memory_record_with_defaulted_w10_fields() {
        let legacy = LegacyMemoryRecord {
            value: "build with cargo test".to_string(),
            tags: vec!["build".to_string()],
            created_at: 111,
            updated_at: 222,
        };
        // `to_vec_named` mirrors `write_memory_record`'s on-disk encoding exactly.
        let bytes = rmp_serde::to_vec_named(&legacy).expect("encode legacy record");
        let decoded: MemoryRecord =
            rmp_serde::from_slice(&bytes).expect("decode legacy bytes into current record");

        assert_eq!(decoded.value, "build with cargo test");
        assert_eq!(decoded.tags, vec!["build".to_string()]);
        assert_eq!(decoded.created_at, 111);
        assert_eq!(decoded.updated_at, 222);
        // New fields default cleanly — no panic, no data loss.
        assert_eq!(decoded.verified, VerifyState::Unverified);
        assert_eq!(decoded.last_verified, 0);
        assert_eq!(decoded.importance, 0.0);
        assert!(decoded.provenance.symbols.is_empty());
        assert!(decoded.provenance.files.is_empty());
        assert!(decoded.provenance.commands.is_empty());
    }

    /// Full round-trip with the new fields populated, including a `SymbolRef` with a
    /// structural hash — the audit engine's read-modify-write path depends on this.
    #[test]
    fn should_round_trip_memory_record_with_provenance() {
        let record = MemoryRecord {
            value: "retry cap lives in fetch_user".to_string(),
            tags: vec!["skill".to_string()],
            created_at: 1,
            updated_at: 2,
            provenance: Provenance {
                symbols: vec![SymbolRef {
                    path: crate::path::RelPath::from("src/net.rs"),
                    name: "fetch_user".to_string(),
                    kind: Some("function".to_string()),
                    structural_hash: Some([7u8; 32]),
                }],
                files: vec![crate::path::RelPath::from("src/net.rs")],
                commands: vec!["cargo test".to_string()],
            },
            verified: VerifyState::Stale,
            last_verified: 999,
            importance: 0.42,
        };
        let bytes = rmp_serde::to_vec_named(&record).expect("encode");
        let decoded: MemoryRecord = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(decoded.verified, VerifyState::Stale);
        assert_eq!(decoded.importance, 0.42);
        assert_eq!(decoded.provenance.symbols.len(), 1);
        assert_eq!(
            decoded.provenance.symbols[0].structural_hash,
            Some([7u8; 32])
        );
    }
}
