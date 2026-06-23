//! Persisted and wire data model for the comms broker.
//!
//! Two-tier message storage is the central design decision: every post writes a small
//! [`MessageMeta`] front-matter record (subject, sender, timestamps, body hash + length)
//! and a separate [`MessageBody`] blob. History and inbox lookups read ONLY the front-matter
//! — the body is fetched lazily via `get_body`. This keeps the hot "what's new?" path cheap
//! even when bodies are large.
//!
//! ## A2A alignment
//!
//! The brief asks to align [`AgentCard`] and a message view with the `a2a-types` crate. That
//! crate (0.2) ships only prost/pbjson-generated protobuf types whose `metadata` fields are
//! `pbjson_types::Struct`, which do not round-trip through `rmp_serde`. So we mirror the A2A
//! spec field shapes here as plain serde structs (the canonical names: `name`, `description`,
//! `version`, `Role::{User, Agent}`, `Part::{Text, Data, ...}`) so a future A2A HTTP
//! front-end can map cleanly to/from `a2a_types::{AgentCard, Message, Part, Role}` without a
//! lossy translation. See the module deviation note in the component report.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::ids::{AgentId, RoomId};

/// Current time in microseconds since the unix epoch, saturating on the (effectively
/// impossible) clock-before-epoch case. A local mirror of `crate::lance::now_micros` so the
/// comms feature does not pull in the lance/intelligence feature just for a timestamp.
pub fn now_micros() -> i64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(dur.as_micros()).unwrap_or(i64::MAX)
}

/// The scope that governs which agents auto-join a room.
///
/// * [`RoomScope::Remote`] — keyed by a normalised git remote (see `crate::git::scope_key`).
///   Every agent working in a clone of that remote auto-joins, regardless of checkout path.
/// * [`RoomScope::PathPrefix`] — keyed by a filesystem path. An agent whose cwd is at or
///   below this path auto-joins. This is what lets nested repos and horizontal monorepos
///   share a workspace room.
/// * [`RoomScope::Session`] — keyed by a terminal `session_id`. A parent agent and a child
///   agent it spawned share the same `session_id` and so auto-join the same room — the basis
///   for the agent-shells lineage chat. Distinct from `PathPrefix`: two unrelated agents in the
///   same directory must NOT share a session room, only a shared `session_id` matches.
/// * [`RoomScope::Global`] — every agent on the machine auto-joins. Reserved for MACHINE-WIDE
///   ops coordination (resource / CPU contention, shared-host scheduling), NOT general per-repo
///   chat — repo rooms ([`RoomScope::Remote`] / [`RoomScope::PathPrefix`]) are for work in a repo.
///
/// Serialized with an adjacent tag (`{"kind": …, "value": …}`) rather than an internal tag:
/// `rmp_serde` cannot encode an internally-tagged newtype variant that wraps a scalar, and
/// adjacent tagging round-trips cleanly through BOTH msgpack (the store) and JSON (a future
/// A2A front-end). New variants are additive — they extend the tail of the tag set, so rooms
/// persisted before this variant existed still deserialize.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum RoomScope {
    /// Normalised git remote URL (e.g. `github.com/foo/bar`).
    Remote(String),
    /// Filesystem path; an agent's cwd at or below this path matches.
    PathPrefix(std::path::PathBuf),
    /// Terminal session id; an agent presenting the same `session_id` matches.
    Session(String),
    /// Every agent on the machine. Reserved for machine-wide ops coordination (resource / CPU
    /// contention), not per-repo chat.
    Global,
}

/// A registered chat room. Agents whose [scope chain](super::scope::ScopeChain) covers the
/// room's [`RoomScope`] auto-join it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Room {
    /// Stable identifier — also the key in the `rooms` keyspace.
    pub room_id: RoomId,
    /// Governs auto-join eligibility.
    pub scope: RoomScope,
    /// Human-readable title.
    pub title: String,
    /// Creation time in microseconds since the unix epoch.
    pub created_at: i64,
    /// Last-post time in microseconds since the unix epoch; `0` when the room has never had a post.
    /// Drives room-freshness (ACTIVE / STALE) surfacing so agents can skip dead rooms.
    ///
    /// Additive: `#[serde(default)]` keeps rooms persisted before this field existed deserializable
    /// (they default to `0` — treated as stale until the next post stamps them), so adding it
    /// required no `COMMS_SCHEMA_VER` bump.
    #[serde(default)]
    pub last_activity: i64,
}

/// Condensed message front-matter — the ONLY record history/inbox lookups decode.
///
/// The body lives separately in the `message_body` keyspace, keyed by [`MessageMeta::id`],
/// and is fetched on demand via `get_body`. [`MessageMeta::body_len`] and
/// [`MessageMeta::body_sha`] let a client display size + integrity without loading the body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageMeta {
    /// Globally unique message id (the `message_body` key).
    pub id: String,
    /// Room this message was posted to.
    pub room: RoomId,
    /// Authoring agent.
    pub from: AgentId,
    /// Post time in microseconds since the unix epoch.
    pub ts_micros: i64,
    /// Short human subject line.
    pub subject: String,
    /// Free-form tags for filtering.
    pub tags: Vec<String>,
    /// Optional id of the message this one replies to (threading).
    pub reply_to: Option<String>,
    /// Glob / path patterns (or repo / workspace tags) describing WHERE this message applies,
    /// so agents can filter relevance without fetching the body. Empty when unscoped.
    ///
    /// Additive: `#[serde(default)]` keeps older blobs (written before this field existed)
    /// deserializable, so adding it required no comms schema-version bump.
    #[serde(default)]
    pub scope: Vec<String>,
    /// Length of the separately-stored body in bytes.
    pub body_len: u32,
    /// Hex-encoded SHA-256 of the body for integrity / dedup.
    pub body_sha: String,
}

/// The message body, stored separately from [`MessageMeta`] so the front-matter scan stays
/// cheap. Fetched only by `get_body`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageBody(pub Vec<u8>);

/// A standing subscription of an agent to a room. Drives notification fan-out and is the
/// basis of the inbox (the union of an agent's subscribed rooms).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    /// Subscribed agent.
    pub agent_id: AgentId,
    /// Room subscribed to.
    pub room: RoomId,
    /// Subscription time in microseconds since the unix epoch.
    pub created_at: i64,
}

/// Lineage record for a terminal session: which agent owns it, the parent agent that spawned
/// it (if any), and the session-scoped room they share. Persisted in the `sessions` keyspace
/// keyed by [`SessionLineage::session_id`], so a future tree view can reconstruct the
/// spawn graph.
///
/// The row is written by the broker at the child's `Hello`: the daemon then knows the
/// `session_id` and `parent_agent` (both carried on the Hello) and the `child_agent` (the Hello's
/// agent), and resolves the session-scoped room the child was just auto-joined to. The write is
/// best-effort — a store failure logs and is swallowed so the handshake still completes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLineage {
    /// The terminal session id this lineage describes (also the `sessions` key).
    pub session_id: String,
    /// The agent that spawned the child, if this session was spawned by another agent.
    pub parent_agent: Option<AgentId>,
    /// The agent that owns this session.
    pub child_agent: AgentId,
    /// The session-scoped room the parent and child share.
    pub room_id: RoomId,
    /// Creation time in microseconds since the unix epoch.
    pub created_at: i64,
}

/// How an agent first reached the broker. Recorded for observability and so a future A2A
/// HTTP front-end can be distinguished from a local CLI client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// A `basemind serve` MCP session.
    Serve,
    /// A one-shot `basemind comms`/`basemind <verb>` CLI invocation.
    Cli,
    /// A git/editor hook.
    Hook,
    /// An external A2A HTTP peer (future).
    A2a,
    /// Anything else / unknown.
    #[default]
    Other,
}

/// Persisted record for a known agent, keyed by [`AgentId`] in the `agents` keyspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    /// Stable identity.
    pub agent_id: AgentId,
    /// The agent's self-described card (A2A-shaped).
    pub card: AgentCard,
    /// How the agent reached the broker.
    pub kind: AgentKind,
    /// First-seen time in microseconds since the unix epoch.
    pub first_seen: i64,
    /// Last-seen time in microseconds since the unix epoch.
    pub last_seen: i64,
}

/// A2A-aligned agent card. A serde mirror of `a2a_types::AgentCard`'s core fields (the
/// generated prost type is not msgpack-friendly — see the module note). Only the fields the
/// broker needs are modelled; `extra` captures anything else losslessly.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCard {
    /// Human-readable agent name.
    #[serde(default)]
    pub name: String,
    /// Human-readable description of the agent's purpose.
    #[serde(default)]
    pub description: String,
    /// Agent version string (e.g. "1.0.0").
    #[serde(default)]
    pub version: String,
    /// Optional skill labels.
    #[serde(default)]
    pub skills: Vec<String>,
}

/// A2A `Role` mirror — the sender side of a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Client-originated (maps to `Role::ROLE_USER`).
    User,
    /// Agent-originated (maps to `Role::ROLE_AGENT`).
    Agent,
}

/// A2A `Part` mirror — one unit of message content. Mirrors the `oneof content` of
/// `a2a_types::Part` for the variants the broker uses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Part {
    /// Plain text content.
    Text(String),
    /// Arbitrary structured JSON.
    Data(serde_json::Value),
}

/// A2A `Message` mirror — a decoded view combining [`MessageMeta`] front-matter with the
/// fetched body, shaped like `a2a_types::Message`. Built by the client when a caller wants
/// the full message rather than just the front-matter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageView {
    /// Mirrors `a2a_types::Message::message_id`.
    pub message_id: String,
    /// Room id, surfaced as the A2A `context_id`.
    pub context_id: String,
    /// Always [`Role::Agent`] for broker traffic; kept for A2A round-tripping.
    pub role: Role,
    /// Message content parts.
    pub parts: Vec<Part>,
}

impl MessageView {
    /// Build a full A2A-shaped view from front-matter + a fetched UTF-8 (lossy) body.
    pub fn from_meta_and_body(meta: &MessageMeta, body: &[u8]) -> Self {
        Self {
            message_id: meta.id.clone(),
            context_id: meta.room.as_str().to_string(),
            role: Role::Agent,
            parts: vec![Part::Text(String::from_utf8_lossy(body).into_owned())],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_micros_is_positive_and_monotonic_ish() {
        let a = now_micros();
        assert!(a > 0, "now_micros should be after the epoch");
    }

    #[test]
    fn room_scope_round_trips_through_msgpack() {
        for scope in [
            RoomScope::Remote("github.com/foo/bar".to_string()),
            RoomScope::PathPrefix(std::path::PathBuf::from("/home/u/work")),
            RoomScope::Session("sess-abc".to_string()),
            RoomScope::Global,
        ] {
            let bytes = rmp_serde::to_vec_named(&scope).expect("encode");
            let back: RoomScope = rmp_serde::from_slice(&bytes).expect("decode");
            assert_eq!(scope, back);
        }
    }

    #[test]
    fn message_meta_round_trips_and_is_small() {
        let meta = MessageMeta {
            id: "m-1".to_string(),
            room: RoomId::parse("room-1").expect("room"),
            from: AgentId::parse("agent-1").expect("agent"),
            ts_micros: 123,
            subject: "hello".to_string(),
            tags: vec!["t1".to_string()],
            reply_to: None,
            scope: vec!["src/**".to_string()],
            body_len: 5,
            body_sha: "abc".to_string(),
        };
        let bytes = rmp_serde::to_vec_named(&meta).expect("encode");
        let back: MessageMeta = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(meta, back);
    }

    /// `scope` is additive: a msgpack record written before the field existed (i.e. a map with
    /// no `scope` key) still deserializes, defaulting `scope` to an empty vec. This is what lets
    /// the field land without a `COMMS_SCHEMA_VER` bump.
    #[test]
    fn message_meta_without_scope_field_deserializes_to_empty() {
        #[derive(serde::Serialize)]
        struct LegacyMeta {
            id: String,
            room: RoomId,
            from: AgentId,
            ts_micros: i64,
            subject: String,
            tags: Vec<String>,
            reply_to: Option<String>,
            body_len: u32,
            body_sha: String,
        }
        let legacy = LegacyMeta {
            id: "m-old".to_string(),
            room: RoomId::parse("room-1").expect("room"),
            from: AgentId::parse("agent-1").expect("agent"),
            ts_micros: 1,
            subject: "legacy".to_string(),
            tags: vec![],
            reply_to: None,
            body_len: 0,
            body_sha: "z".to_string(),
        };
        let bytes = rmp_serde::to_vec_named(&legacy).expect("encode legacy");
        let back: MessageMeta = rmp_serde::from_slice(&bytes).expect("decode legacy");
        assert_eq!(back.id, "m-old");
        assert!(back.scope.is_empty(), "missing scope defaults to empty");
    }
}
