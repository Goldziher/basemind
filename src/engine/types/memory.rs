//! Types of `memory_save`, `memory_search` and `basemind memory delete`: the wire arguments, the
//! per-operation requests the bodies in `engine::memory` take, and their responses.
//!
//! The wire param structs are always compiled: the memory tools are advertised in every build and
//! gate in the body, so the shapes must exist without the `memory` feature. The response structs and the blob-record types
//! (`MemoryRecord`, `SymbolRef`, `Provenance`, `VerifyState`) are `#[cfg(feature = "memory")]`-gated:
//! they only exist when the LanceDB-backed memory store is compiled in to construct them. The
//! [`Visibility`] tier selector is re-exported from `crate::config`, which owns it because the
//! memory config names its default tier.

use serde::{Deserialize, Serialize};

#[cfg(feature = "memory")]
use super::default_true;
use crate::engine::cursor::Cursor;

pub use crate::config::Visibility;

/// Arguments of `memory_save`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemorySaveParams {
    /// Stable identifier, e.g. `decision/auth-tokens`.
    pub key: String,
    /// The note.
    pub value: String,
    /// Labels to filter on later.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

/// Arguments of `memory_search`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemorySearchToolParams {
    /// Search by meaning. At most one of `query`, `key`, `prefix`; none lists every note.
    #[serde(default)]
    pub query: Option<String>,
    /// Read one note by exact key.
    #[serde(default)]
    pub key: Option<String>,
    /// List notes whose key starts with this.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Notes carrying any of these. Not with `key`.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
    /// Max notes (default 10 for `query`, 100 for a listing). Not with `key`.
    #[serde(default)]
    pub limit: Option<u32>,
    /// A prior listing's `next_cursor`. Not with `query` or `key`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

/// What `memory_search` answers, by the shape of its arguments: hits by meaning, one note, the
/// absence of the note asked for, or a listing. Every variant is an object: MCP types
/// `structuredContent` as one, so a missing note is never `null`.
#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum MemorySearchAnswer {
    Search(MemorySearchResponse),
    Get(MemoryEntry),
    Missing(MemoryMissing),
    List(MemoryListResponse),
}

/// `memory_search {key}` for a key no note has.
#[cfg(feature = "memory")]
#[derive(Debug, Serialize)]
pub(crate) struct MemoryMissing {
    pub key: String,
    /// Always `false`: no note has this key in the tier read.
    pub found: bool,
}

#[cfg(feature = "memory")]
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryPutParams {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Embed the note as part of the save. `false` leaves that to the next search by meaning,
    /// which embeds every note without a current vector row: the note is findable by `key` and
    /// `prefix` at once and by `query` from that search on.
    #[serde(default = "default_true")]
    pub embed: bool,
    /// Memory tier: `group` (shared, default) or `individual` (per-agent).
    #[serde(default)]
    pub visibility: Visibility,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct MemoryPutResponse {
    pub key: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Whether the note has a vector row, i.e. whether `memory_search` by `query` can find it.
    pub embedded: bool,
    /// Why a saved note is not `embedded`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

#[cfg(feature = "memory")]
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryGetParams {
    pub key: String,
    /// Memory tier: `group` (shared, default) or `individual` (per-agent).
    #[serde(default)]
    pub visibility: Visibility,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[cfg(feature = "memory")]
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryListParams {
    #[serde(default)]
    pub prefix: Option<String>,
    /// Notes carrying any of these; empty lists every note.
    #[serde(default)]
    pub tags: Vec<String>,
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
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct MemoryListResponse {
    /// Notes matching the listing. A lower bound when `truncated`.
    pub total: usize,
    /// True when counting stopped at the scan cap, so `total` is a lower bound. A page cut at
    /// `limit` is paging (`next_cursor` alone), not truncation.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `scan_cap` whenever `truncated` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    pub entries: Vec<MemoryEntry>,
    /// Opaque cursor to pass back on the next call when more results are available.
    /// Stable across rescans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / vector
    /// search + response construction), excluding MCP transport, argument
    /// deserialization, and response serialization. A first call against a cold server also
    /// includes index warm-up; such responses carry a `notice`. See
    /// [`crate::engine::timing`] for the full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[cfg(feature = "memory")]
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemorySearchParams {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Notes carrying any of these; empty searches every note.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Memory tier: `group` (shared, default) or `individual` (per-agent). An individual
    /// search never returns another agent's rows; a group search only sees group rows.
    #[serde(default)]
    pub visibility: Visibility,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct MemorySearchHit {
    pub key: String,
    pub value: String,
    pub tags: Vec<String>,
    pub distance: f32,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct MemorySearchResponse {
    pub query: String,
    pub hits: Vec<MemorySearchHit>,
    /// Set while the vector index is behind the saved notes: the hits may be incomplete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / vector
    /// search + response construction), excluding MCP transport, argument
    /// deserialization, and response serialization. A first call against a cold server also
    /// includes index warm-up; such responses carry a `notice`. See
    /// [`crate::engine::timing`] for the full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryDeleteParams {
    pub key: String,
    /// Memory tier: `group` (shared, default) or `individual` (per-agent).
    #[serde(default)]
    pub visibility: Visibility,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct MemoryDeleteResponse {
    pub deleted: bool,
}

/// Verification verdict of a memory's code references against the live index, set by the
/// W10 audit engine. `#[serde(default)]` on the record field means pre-W10 blobs (written
/// before this existed) decode as `Unverified` — no schema bump required.
#[cfg(feature = "memory")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifyState {
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
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SymbolRef {
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
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Provenance {
    #[serde(default)]
    pub symbols: Vec<SymbolRef>,
    #[serde(default)]
    pub files: Vec<crate::path::RelPath>,
    #[serde(default)]
    pub commands: Vec<String>,
}

#[cfg(feature = "memory")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
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
        let bytes = rmp_serde::to_vec_named(&legacy).expect("encode legacy record");
        let decoded: MemoryRecord = rmp_serde::from_slice(&bytes).expect("decode legacy bytes into current record");

        assert_eq!(decoded.value, "build with cargo test");
        assert_eq!(decoded.tags, vec!["build".to_string()]);
        assert_eq!(decoded.created_at, 111);
        assert_eq!(decoded.updated_at, 222);
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
        assert_eq!(decoded.provenance.symbols[0].structural_hash, Some([7u8; 32]));
    }
}
