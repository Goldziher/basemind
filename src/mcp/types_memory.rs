//! Request / response shapes for the `memory` domain tool.
//!
//! [`MemoryParams`] is what crosses the wire: one flat parameter object for the single `memory`
//! tool, with a required [`MemoryMode`] selecting the operation and every per-mode field an
//! optional sibling. The per-operation structs below stay as the helpers' internal shapes.
//!
//! Split out of `types.rs` to keep that file within the per-file size budget. The param structs are
//! always compiled — the `memory` tool is advertised in every build and gates per mode, so the
//! shapes it deserializes into must exist without the `memory` feature. The response structs and
//! the blob-record types (`MemoryRecord`, `SymbolRef`, `Provenance`, `VerifyState`) are
//! `#[cfg(feature = "memory")]`-gated: they only exist when the LanceDB-backed memory store is
//! compiled in to construct them. The [`Visibility`] tier selector is always compiled — it is part
//! of the wire params.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::cursor::Cursor;
use super::mode::MemoryMode;
use super::types::default_true;

/// Memory tier selector. `group` (the default) is the shared, cross-agent tier — today's
/// behavior, with an empty owner segment. `individual` scopes the entry to the calling
/// agent (owner = its `AgentId`), so two agents can keep private same-key entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Shared, cross-agent memory (owner segment is empty). The default.
    #[default]
    Group,
    /// Per-agent memory (owner segment is the caller's `AgentId`).
    Individual,
}

impl schemars::JsonSchema for Visibility {
    /// Inlined, not `$ref`'d into `$defs` — see [`crate::path::RelPath`]'s impl and GH #50.
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Visibility".into()
    }

    /// A flat string enum, written by hand rather than derived.
    ///
    /// The derive turns per-variant doc comments into `oneOf: [{const, description}, …]`, and
    /// `oneOf` is outside the Anthropic `input_schema` subset — which silently drops the server's
    /// entire tool registry (GH #50). The variant docs live in the description instead, so the
    /// meaning survives without the rejected construct.
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "enum": ["group", "individual"],
            "description": "Memory tier: `group` (default) is shared across agents; `individual` \
                            scopes the entry to the calling agent."
        })
    }
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

/// Wire parameters for the `memory` tool.
///
/// `mode` is the only required field. Every other field belongs to a subset of the modes and is
/// rejected — not ignored — when passed to a mode that has no use for it (see
/// [`super::mode::reject_unsupported`]); a field a mode requires but did not receive is reported by
/// name (`mode="put" requires \`value\``) rather than as a bare "missing parameter".
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryParams {
    /// Which operation to run.
    pub mode: MemoryMode,

    /// Memory key. Required by `put` / `get` / `delete`. Optional for `audit` (audit this one
    /// record instead of the whole scope) and for `accept` (override the auto-derived
    /// `skill/cochange-<short_id>` key).
    #[serde(default, alias = "name")]
    pub key: Option<String>,
    /// `put` only, required: the note body to store.
    #[serde(default)]
    pub value: Option<String>,
    /// `put` only: tags attached to the entry, matched exactly by `list` / `search`.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// `put` only: also embed the value into LanceDB so `search` can reach it. Default true.
    #[serde(default)]
    pub embed: Option<bool>,
    /// Memory tier for `put` / `get` / `list` / `search` / `delete` / `audit`: `group` (shared
    /// across agents, the default) or `individual` (private to the calling agent).
    #[serde(default)]
    pub visibility: Option<Visibility>,

    /// `list` only: key-PREFIX filter (not substring).
    #[serde(default)]
    pub prefix: Option<String>,
    /// `list` and `search` only: exact tag filter.
    #[serde(default)]
    pub tag: Option<String>,
    /// Result cap for `list` / `search` / `audit` / `documents` / `proposals`. `list`, `audit` and
    /// `proposals` default to 100 (max 1000); `search` and `documents` default to 10 (max 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Resume token from the previous call's `next_cursor` — `list` and `proposals` only. Stable
    /// across rescans because the underlying Fjall keys are content-addressed.
    #[serde(default)]
    pub cursor: Option<Cursor>,

    /// Search text. Required by `search` (vector KNN over stored memory) and by `documents`
    /// (semantic search over indexed document chunks).
    #[serde(
        default,
        alias = "needle",
        alias = "pattern",
        alias = "q",
        alias = "text",
        alias = "search"
    )]
    pub query: Option<String>,

    /// `audit` only: compute verdicts and return them without persisting any mutation.
    #[serde(default)]
    pub dry_run: Option<bool>,
    /// `audit` only: also scan the archived (`memory_archive`) keyspace.
    #[serde(default)]
    pub include_archived: Option<bool>,

    /// `documents` only: token budget bounding the returned `hits` list (best-first; sets
    /// `budgeted`).
    #[serde(default, alias = "token_budget", alias = "budget")]
    pub max_tokens: Option<u32>,
    /// `documents` only: wire format for the response — `"json"` (default) or `"toon"`.
    #[serde(default, alias = "encoding")]
    pub format: Option<String>,
    /// `documents` only: exact MIME-type filter.
    #[serde(default)]
    pub mime_type: Option<String>,
    /// `documents` only: which ingestion scope to search. Defaults to this repo's; pages ingested
    /// by the `web` tool live under `web:<host>` (it echoes the scope back).
    #[serde(default)]
    pub scope: Option<String>,
    /// `documents` only: case-insensitive substring match against a parent document's entity
    /// categories (NER). Combined with `keywords_contains` via AND semantics when both are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_category: Option<String>,
    /// `documents` only: case-insensitive substring match against a parent document's keywords.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords_contains: Option<String>,

    /// `mine` only: number of recent commits to inspect (default 200, max 2000).
    #[serde(default)]
    pub window: Option<u32>,
    /// `mine` only: minimum co-change count for a candidate to be emitted (default 5).
    #[serde(default)]
    pub min_support: Option<u32>,
    /// `mine` only: minimum `support / anchor_freq` for a candidate (default 0.6).
    #[serde(default)]
    pub min_confidence: Option<f32>,
    /// `mine` only: skip commits touching more than this many files (default 25), so bulk/vendor
    /// commits do not dominate the co-change map.
    #[serde(default)]
    pub max_files_per_commit: Option<u32>,

    /// `proposals` only: filter by proposal kind — `"skill"` or `"memory"`. Omit for all.
    #[serde(default)]
    pub kind: Option<String>,
    /// Proposal id as returned by `proposals`. Required by `accept` and `reject`.
    #[serde(default)]
    pub id: Option<String>,
    /// `reject` only: human-readable reason. Logged, never persisted.
    #[serde(default)]
    pub reason: Option<String>,

    /// `documents` only: per-query overrides for any `documents.*` config knob, taking precedence
    /// over serve-time config and CLI flags. Unrecognized fields are ignored — flatten semantics.
    #[serde(flatten, default)]
    pub overrides: crate::config::DocumentsCliOverrides,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryPutParams {
    #[serde(alias = "name")]
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
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct MemoryPutResponse {
    pub key: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryGetParams {
    #[serde(alias = "name")]
    pub key: String,
    /// Memory tier: `group` (shared, default) or `individual` (per-agent).
    #[serde(default)]
    pub visibility: Visibility,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct MemoryListResponse {
    pub total: usize,
    pub truncated: bool,
    pub entries: Vec<MemoryEntry>,
    /// Opaque cursor to pass back on the next call when more results are available.
    /// Stable across rescans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / vector
    /// search / graph walk + response construction), excluding MCP transport, argument
    /// deserialization, and response serialization. A first call against a cold server also
    /// includes index warm-up; such responses carry a `notice`. See
    /// [`crate::mcp::helpers::timing`] for the full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemorySearchParams {
    #[serde(alias = "needle", alias = "pattern", alias = "q", alias = "search")]
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
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct MemorySearchHit {
    pub key: String,
    pub value: String,
    pub tags: Vec<String>,
    pub distance: f32,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct MemorySearchResponse {
    pub query: String,
    pub hits: Vec<MemorySearchHit>,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / vector
    /// search / graph walk + response construction), excluding MCP transport, argument
    /// deserialization, and response serialization. A first call against a cold server also
    /// includes index warm-up; such responses carry a `notice`. See
    /// [`crate::mcp::helpers::timing`] for the full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryDeleteParams {
    #[serde(alias = "name")]
    pub key: String,
    /// Memory tier: `group` (shared, default) or `individual` (per-agent).
    #[serde(default)]
    pub visibility: Visibility,
}

#[cfg(feature = "memory")]
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct MemoryDeleteResponse {
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

#[cfg(test)]
mod param_alias_tests {
    use super::*;

    #[test]
    fn memory_search_accepts_query_aliases() {
        let by_needle: MemorySearchParams = serde_json::from_value(serde_json::json!({ "needle": "retry" })).unwrap();
        assert_eq!(by_needle.query, "retry");
        let by_q: MemorySearchParams = serde_json::from_value(serde_json::json!({ "q": "retry" })).unwrap();
        assert_eq!(by_q.query, "retry");
        let by_query: MemorySearchParams = serde_json::from_value(serde_json::json!({ "query": "retry" })).unwrap();
        assert_eq!(by_query.query, "retry");
    }

    #[test]
    fn memory_get_accepts_name_alias_for_key() {
        let params: MemoryGetParams = serde_json::from_value(serde_json::json!({ "name": "skill/foo" })).unwrap();
        assert_eq!(params.key, "skill/foo");
    }

    /// `mode` carries no `Default` and no `#[serde(default)]`: an omitted mode must fail loudly
    /// rather than silently pick an operation the caller did not ask for.
    #[test]
    fn memory_params_reject_an_omitted_mode() {
        let error = serde_json::from_value::<MemoryParams>(serde_json::json!({ "key": "k" }))
            .expect_err("a `memory` call without a mode must fail");
        assert!(error.to_string().contains("mode"), "{error}");
    }

    #[test]
    fn memory_params_report_every_accepted_mode_on_an_unknown_one() {
        let error = serde_json::from_value::<MemoryParams>(serde_json::json!({ "mode": "recall" }))
            .expect_err("an unknown mode must fail");
        assert!(error.to_string().contains("put|get|list|search|delete"), "{error}");
    }

    #[test]
    fn memory_params_accept_the_query_and_key_aliases() {
        let by_needle: MemoryParams =
            serde_json::from_value(serde_json::json!({ "mode": "search", "needle": "retry" })).unwrap();
        assert_eq!(by_needle.query.as_deref(), Some("retry"));
        let by_name: MemoryParams =
            serde_json::from_value(serde_json::json!({ "mode": "get", "name": "skill/foo" })).unwrap();
        assert_eq!(by_name.key.as_deref(), Some("skill/foo"));
    }

    /// The flattened documents overrides must keep landing in `overrides`, not vanish, or a
    /// per-call `reranker_preset` would be accepted and ignored.
    #[test]
    fn memory_params_capture_flattened_documents_overrides() {
        let params: MemoryParams = serde_json::from_value(serde_json::json!({
            "mode": "documents",
            "query": "retry",
            "reranker_preset": "bge-reranker-base",
        }))
        .unwrap();
        assert!(params.overrides.any(), "documents overrides must survive the flatten");
    }

    /// The published input schema must stay inside the Anthropic subset — one `oneOf`/`$ref`
    /// anywhere silently drops the WHOLE tool registry (GH #50).
    #[test]
    fn memory_params_schema_stays_within_the_anthropic_subset() {
        let mut generator = schemars::SchemaGenerator::default();
        let schema =
            serde_json::to_string(&<MemoryParams as schemars::JsonSchema>::json_schema(&mut generator)).unwrap();
        for forbidden in ["$ref", "$defs", "oneOf", "anyOf", "allOf"] {
            assert!(!schema.contains(forbidden), "{forbidden} leaked into {schema}");
        }
    }

    #[test]
    fn memory_put_accepts_name_alias_for_key() {
        let params: MemoryPutParams = serde_json::from_value(serde_json::json!({ "name": "k", "value": "v" })).unwrap();
        assert_eq!(params.key, "k");
        assert_eq!(params.value, "v");
    }
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
