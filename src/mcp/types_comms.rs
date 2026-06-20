//! Request / response shapes for the agent-comms MCP tools.
//!
//! Parameter structs derive `Deserialize + Serialize + JsonSchema` and use the validated
//! [`RoomId`](crate::comms::ids::RoomId) / [`AgentId`](crate::comms::ids::AgentId) newtypes for
//! identifier fields so a malformed id is rejected at the serde boundary rather than reaching
//! the broker. Response structs serialize the broker's [`MessageMeta`] front-matter directly —
//! history and inbox tools return front-matter ONLY; bodies come from `message_get`.

#![cfg(feature = "comms")]

use serde::{Deserialize, Serialize};

use crate::comms::cursor::Cursor;
use crate::comms::ids::RoomId;
use crate::comms::model::{Room, RoomScope};
use crate::comms::protocol::SeqMeta;

// ─── agent_register ───────────────────────────────────────────────────────────────────────

/// Params for `agent_register`: announce or update this agent's A2A card.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct AgentRegisterParams {
    /// Human-readable agent name.
    #[serde(default)]
    pub name: String,
    /// One-line description of the agent's purpose.
    #[serde(default)]
    pub description: String,
    /// Agent version string (e.g. "1.0.0").
    #[serde(default)]
    pub version: String,
    /// Optional skill labels advertised to peers.
    #[serde(default)]
    pub skills: Vec<String>,
}

/// Response for `agent_register`.
#[derive(Debug, Serialize)]
pub(super) struct AgentRegisterResponse {
    /// The agent id the card was registered under.
    pub agent_id: String,
    /// Always true on success.
    pub registered: bool,
}

// ─── agent_list ───────────────────────────────────────────────────────────────────────────

/// Params for `agent_list`: enumerate known agents, optionally restricted to one room.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct AgentListParams {
    /// Restrict to subscribers of this room when set.
    #[serde(default)]
    pub room: Option<RoomId>,
}

/// One agent row in an `agent_list` response (front-matter view of an `AgentRecord`).
#[derive(Debug, Serialize)]
pub(super) struct AgentSummary {
    /// Stable agent identity.
    pub agent_id: String,
    /// Self-described name.
    pub name: String,
    /// Self-described description.
    pub description: String,
    /// Self-described version.
    pub version: String,
    /// Advertised skill labels.
    pub skills: Vec<String>,
    /// First-seen time, microseconds since the unix epoch.
    pub first_seen: i64,
    /// Last-seen time, microseconds since the unix epoch.
    pub last_seen: i64,
}

/// Response for `agent_list`.
#[derive(Debug, Serialize)]
pub(super) struct AgentListResponse {
    /// Number of agents returned.
    pub total: usize,
    /// The agent rows.
    pub agents: Vec<AgentSummary>,
}

// ─── room scope (shared input shape) ────────────────────────────────────────────────────────

/// Room-scope selector for `room_create`. Mirrors [`RoomScope`] but is a flat, agent-friendly
/// MCP input: pick exactly one of `remote` / `path_prefix` / `global`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScopeInput {
    /// Scope to a normalised git remote (every clone of it auto-joins).
    Remote(String),
    /// Scope to a filesystem path prefix (an agent at/below it auto-joins).
    PathPrefix(std::path::PathBuf),
    /// Scope to every agent on the machine.
    #[default]
    Global,
}

impl From<ScopeInput> for RoomScope {
    fn from(value: ScopeInput) -> Self {
        match value {
            ScopeInput::Remote(r) => RoomScope::Remote(r),
            ScopeInput::PathPrefix(p) => RoomScope::PathPrefix(p),
            ScopeInput::Global => RoomScope::Global,
        }
    }
}

// ─── room_create ────────────────────────────────────────────────────────────────────────────

/// Params for `room_create`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RoomCreateParams {
    /// Id of the room to create.
    pub room: RoomId,
    /// Scope governing which agents auto-join. Defaults to `global`.
    #[serde(default)]
    pub scope: ScopeInput,
    /// Optional human-readable title.
    #[serde(default)]
    pub title: Option<String>,
}

/// A room front-matter view shared by `room_create` and `room_list`.
#[derive(Debug, Serialize)]
pub(super) struct RoomSummary {
    /// Stable room id.
    pub room_id: String,
    /// Human-readable title.
    pub title: String,
    /// Creation time, microseconds since the unix epoch.
    pub created_at: i64,
}

impl From<&Room> for RoomSummary {
    fn from(room: &Room) -> Self {
        Self {
            room_id: room.room_id.as_str().to_string(),
            title: room.title.clone(),
            created_at: room.created_at,
        }
    }
}

/// Response for `room_create`.
#[derive(Debug, Serialize)]
pub(super) struct RoomCreateResponse {
    /// The created (or re-confirmed) room.
    pub room: RoomSummary,
}

// ─── room_list ──────────────────────────────────────────────────────────────────────────────

/// Params for `room_list`: list rooms whose scope matches the calling agent's chain. No fields
/// — scope context (remote + cwd) is injected by the server from its root.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RoomListParams {}

/// Response for `room_list`.
#[derive(Debug, Serialize)]
pub(super) struct RoomListResponse {
    /// Number of rooms returned.
    pub total: usize,
    /// The room rows.
    pub rooms: Vec<RoomSummary>,
}

// ─── room_join / room_leave ──────────────────────────────────────────────────────────────────

/// Params for `room_join`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RoomJoinParams {
    /// The room to join (subscribe to).
    pub room: RoomId,
}

/// Params for `room_leave`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RoomLeaveParams {
    /// The room to leave (unsubscribe from).
    pub room: RoomId,
}

/// Response for `room_join` / `room_leave`.
#[derive(Debug, Serialize)]
pub(super) struct RoomMembershipResponse {
    /// The room acted on.
    pub room: String,
    /// True after a successful join.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub joined: bool,
    /// True after a successful leave.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub left: bool,
}

// ─── room_post ───────────────────────────────────────────────────────────────────────────────

/// Params for `room_post`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RoomPostParams {
    /// Target room.
    pub room: RoomId,
    /// Short human subject line.
    pub subject: String,
    /// Message body (markdown). Empty when omitted.
    #[serde(default)]
    pub body: Option<String>,
    /// Free-form tags for filtering.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Id of the message this one replies to, for threading.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Glob / path patterns (or repo / workspace tags) describing WHERE this message applies, so
    /// peers can filter relevance from front-matter without fetching the body. Empty when omitted.
    #[serde(default)]
    pub scope: Option<Vec<String>>,
}

/// Response for `room_post`.
#[derive(Debug, Serialize)]
pub(super) struct RoomPostResponse {
    /// The id of the message just stored.
    pub message_id: String,
}

// ─── room_history ────────────────────────────────────────────────────────────────────────────

/// Params for `room_history`: read a room's front-matter, oldest-first, paginated.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RoomHistoryParams {
    /// The room to read.
    pub room: RoomId,
    /// Resume token from a previous page's `next_cursor` (opaque string).
    #[serde(default)]
    pub cursor: Option<String>,
    /// Maximum messages to return (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Front-matter view of a message. Surfaces [`MessageMeta`] front-matter plus its per-room `seq`
/// — NO body. Fetch the body with `message_get`. `seq` lets callers drive `inbox_ack` (the
/// `to_seq` bulk mode) without an extra round-trip.
#[derive(Debug, Serialize)]
pub(super) struct MessageFrontMatter {
    /// Globally unique message id (pass to `message_get` or `inbox_ack`).
    pub id: String,
    /// Room the message was posted to.
    pub room: String,
    /// Authoring agent.
    pub from: String,
    /// Short human subject line.
    pub subject: String,
    /// Post time, microseconds since the unix epoch.
    pub ts_micros: i64,
    /// Free-form tags.
    pub tags: Vec<String>,
    /// Glob / path patterns describing where the message applies (empty when unscoped).
    pub scope: Vec<String>,
    /// Id of the message this one replies to, if any.
    pub reply_to: Option<String>,
    /// Per-room sequence number — the message's position in its room's append-only log. Pass as
    /// `inbox_ack`'s `to_seq` to bulk-ack everything up to and including this message.
    pub seq: u64,
    /// Length of the separately-stored body in bytes.
    pub body_len: u32,
    /// Hex SHA-256 of the body for integrity.
    pub body_sha: String,
}

impl From<&SeqMeta> for MessageFrontMatter {
    fn from(sm: &SeqMeta) -> Self {
        let meta = &sm.meta;
        Self {
            id: meta.id.clone(),
            room: meta.room.as_str().to_string(),
            from: meta.from.as_str().to_string(),
            subject: meta.subject.clone(),
            ts_micros: meta.ts_micros,
            tags: meta.tags.clone(),
            scope: meta.scope.clone(),
            reply_to: meta.reply_to.clone(),
            seq: sm.seq,
            body_len: meta.body_len,
            body_sha: meta.body_sha.clone(),
        }
    }
}

/// Response for `room_history`.
#[derive(Debug, Serialize)]
pub(super) struct RoomHistoryResponse {
    /// Number of messages in this page.
    pub total: usize,
    /// Front-matter rows, oldest-first.
    pub messages: Vec<MessageFrontMatter>,
    /// Opaque cursor for the next page; absent means no more results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
}

// ─── message_get ─────────────────────────────────────────────────────────────────────────────

/// Params for `message_get`: fetch a single message body by id.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MessageGetParams {
    /// The message id (the `id` of a front-matter record).
    pub message_id: String,
}

/// Response for `message_get`.
#[derive(Debug, Serialize)]
pub(super) struct MessageGetResponse {
    /// The message id queried.
    pub message_id: String,
    /// True when a body was found for the id.
    pub found: bool,
    /// The body decoded as UTF-8 (lossy — bodies are markdown). `None` when the id is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

// ─── inbox_read ──────────────────────────────────────────────────────────────────────────────

/// Params for `inbox_read`: read new front-matter across subscribed rooms.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct InboxReadParams {
    /// Resume token from a previous page's `next_cursor` (opaque string).
    #[serde(default)]
    pub cursor: Option<String>,
    /// Maximum messages to return (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<u32>,
    /// When true, advance read cursors past the returned messages.
    #[serde(default)]
    pub mark_read: bool,
}

/// Response for `inbox_read`.
#[derive(Debug, Serialize)]
pub(super) struct InboxReadResponse {
    /// Number of messages in this page.
    pub total: usize,
    /// Count of unread messages remaining after this page.
    pub unread: u32,
    /// Front-matter rows across subscribed rooms.
    pub messages: Vec<MessageFrontMatter>,
    /// Opaque cursor for the next page; absent means no more results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
}

// ─── inbox_ack ───────────────────────────────────────────────────────────────────────────────

/// Params for `inbox_ack`: advance this agent's per-room read cursors past acked messages.
///
/// Two modes, combinable:
/// * `message_ids` — resolve each id to its `(room, seq)`, then advance each room's cursor to the
///   max acked seq in that room.
/// * `room` + `to_seq` — advance that one room's cursor straight to `to_seq` ("ack everything up
///   to here" / stale-room cleanup).
///
/// At least one mode must be supplied; an empty request is rejected.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct InboxAckParams {
    /// Message ids to ack (mode a). Empty when only the bulk mode is used.
    #[serde(default)]
    pub message_ids: Vec<String>,
    /// Target room for the bulk `to_seq` mode (mode b).
    #[serde(default)]
    pub room: Option<RoomId>,
    /// Advance `room`'s cursor straight to this seq (mode b). Requires `room`.
    #[serde(default)]
    pub to_seq: Option<u64>,
}

/// One `(room, new_seq)` cursor advance recorded by `inbox_ack`.
#[derive(Debug, Serialize)]
pub(super) struct CursorAdvance {
    /// The room whose per-agent read cursor advanced.
    pub room: String,
    /// The cursor's new seq after the advance.
    pub seq: u64,
}

/// Response for `inbox_ack`.
#[derive(Debug, Serialize)]
pub(super) struct InboxAckResponse {
    /// Number of message ids that resolved and were acked (the bulk `to_seq` mode does not
    /// contribute to this count).
    pub acked: usize,
    /// The `(room, new_seq)` cursor advances this call produced.
    pub cursors_advanced: Vec<CursorAdvance>,
}
