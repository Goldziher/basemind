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

#[cfg(feature = "memory")]
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct MemoryRecord {
    pub value: String,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}
