//! Persisted and wire data model for the comms broker.
//!
//! Two-tier message storage is the central design decision: every post writes a small
//! [`MessageMeta`] front-matter record (subject, sender, timestamps, body hash + length)
//! and a separate [`MessageBody`] blob. History and inbox lookups read ONLY the front-matter
//! — the body is fetched lazily via `get_body`. This keeps the hot "what's new?" path cheap
//! even when bodies are large.
//!
//! ## Thread model
//!
//! A [`Thread`] is a conversation addressed by AT LEAST TWO of three dimensions: a `subject`
//! (topic string), a `path` (a path or GLOB pattern matched with `globset`), and an explicit
//! `members` set of [`AgentId`]s. Discovery is scoped — a thread is never globally visible;
//! an agent sees it only when it is a member, when its cwd matches the thread's path-glob, or
//! when a subject substring filter matches. There is no auto-join: agents register, then
//! explicitly START a thread or JOIN one.
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

use super::ids::{AgentId, ThreadId};

/// Current time in microseconds since the unix epoch, saturating on the (effectively
/// impossible) clock-before-epoch case. A local mirror of `crate::lance::now_micros` so the
/// comms feature does not pull in the lance/intelligence feature just for a timestamp.
pub fn now_micros() -> i64 {
    let dur = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    i64::try_from(dur.as_micros()).unwrap_or(i64::MAX)
}

/// A registered conversation thread.
///
/// Addressed by AT LEAST TWO of `subject` / `path` / `members` (enforced at `thread_start`).
/// The `creator` may archive the thread and manage its membership; the system auto-archives
/// idle threads past a TTL. An archived thread (`active == false`) drops out of active listings
/// but its history remains readable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    /// Stable identifier — also the key in the `threads` keyspace.
    pub id: ThreadId,
    /// Topic string. Present when the thread is addressed by subject.
    #[serde(default)]
    pub subject: Option<String>,
    /// A path or GLOB pattern (matched with `globset`). Present when the thread is addressed by
    /// path — an agent whose cwd matches this glob discovers the thread.
    #[serde(default)]
    pub path: Option<String>,
    /// The explicit member set. An agent in this set sees the thread in its `thread_list` and
    /// inbox regardless of subject / path.
    #[serde(default)]
    pub members: Vec<AgentId>,
    /// The agent that created the thread — the only agent (besides a human via the CLI) that may
    /// archive it or manage its membership.
    pub creator: AgentId,
    /// `true` while the thread is active; `false` once archived (by the creator, a human, or the
    /// idle-TTL sweep). Archived threads drop out of active `thread_list` results.
    pub active: bool,
    /// Creation time in microseconds since the unix epoch.
    pub created_at: i64,
    /// Last-activity time in microseconds since the unix epoch; stamped on each post. Drives the
    /// idle-TTL auto-archive sweep and STALE surfacing.
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
    /// Thread this message was posted to.
    pub thread: ThreadId,
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
    /// Length of the separately-stored body in bytes.
    pub body_len: u32,
    /// Hex-encoded SHA-256 of the body for integrity / dedup.
    pub body_sha: String,
}

/// The message body, stored separately from [`MessageMeta`] so the front-matter scan stays
/// cheap. Fetched only by `get_body`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageBody(pub Vec<u8>);

/// A standing membership of an agent in a thread. Drives notification fan-out and is the
/// basis of the inbox (the union of an agent's joined threads).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Membership {
    /// Member agent.
    pub agent_id: AgentId,
    /// Thread joined.
    pub thread: ThreadId,
    /// Join time in microseconds since the unix epoch.
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
    /// Thread id, surfaced as the A2A `context_id`.
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
            context_id: meta.thread.as_str().to_string(),
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

    fn thread_id(s: &str) -> ThreadId {
        ThreadId::parse(s).expect("thread")
    }

    fn agent_id(s: &str) -> AgentId {
        AgentId::parse(s).expect("agent")
    }

    #[test]
    fn thread_round_trips_through_msgpack() {
        let thread = Thread {
            id: thread_id("th-1"),
            subject: Some("refactor".to_string()),
            path: Some("src/**".to_string()),
            members: vec![agent_id("alice"), agent_id("bob")],
            creator: agent_id("alice"),
            active: true,
            created_at: now_micros(),
            last_activity: 0,
        };
        let bytes = rmp_serde::to_vec_named(&thread).expect("encode");
        let back: Thread = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(thread, back);
    }

    #[test]
    fn message_meta_round_trips_and_is_small() {
        let meta = MessageMeta {
            id: "m-1".to_string(),
            thread: thread_id("th-1"),
            from: agent_id("agent-1"),
            ts_micros: 123,
            subject: "hello".to_string(),
            tags: vec!["t1".to_string()],
            reply_to: None,
            body_len: 5,
            body_sha: "abc".to_string(),
        };
        let bytes = rmp_serde::to_vec_named(&meta).expect("encode");
        let back: MessageMeta = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(meta, back);
    }
}
