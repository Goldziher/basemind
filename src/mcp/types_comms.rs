//! Request / response shapes for the consolidated `agents` domain tool.
//!
//! [`AgentsParams`] is what crosses the wire: one flat parameter object with a required
//! [`AgentsMode`] selecting the operation and every per-mode field an optional sibling. The
//! per-operation structs below (`ThreadPostParams`, `InboxReadParams`, …) stay as the helpers'
//! internal shapes, so the bodies keep taking exactly the arguments they always did.
//!
//! Parameter structs derive `Deserialize + Serialize + JsonSchema` and use the validated
//! [`ThreadId`](crate::comms::ids::ThreadId) / [`AgentId`](crate::comms::ids::AgentId) newtypes
//! for identifier fields so a malformed id is rejected at the serde boundary rather than reaching
//! the broker. Response structs serialize the broker's [`MessageMeta`] front-matter directly —
//! modes `history` and `inbox` return front-matter ONLY; bodies come from mode `message`.

#![cfg(all(feature = "comms", any(unix, windows)))]

use serde::{Deserialize, Serialize};

use super::mode::AgentsMode;
use crate::comms::cursor::Cursor;
use crate::comms::ids::{AgentId, ThreadId};
use crate::comms::model::Thread;
use crate::comms::protocol::SeqMeta;

/// Wire parameters for the `agents` tool.
///
/// Only `mode` is required. Every other field belongs to one or more modes and is rejected — not
/// ignored — when passed to a mode that has no use for it (see
/// [`super::mode::reject_unsupported`]); a mode that cannot run without one names the exact
/// `mode`/field pair. `as_agent` is the single exception: every mode accepts it, because it selects
/// the identity the call runs as rather than what the call does.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct AgentsParams {
    /// Which operation to run.
    pub mode: AgentsMode,
    /// `join`, `leave`, `members`, `add_member`, `remove_member`, `archive`, `post`, `history` —
    /// the thread to act on, required. `list`, `ack`, `wait` — an optional filter narrowing the
    /// call to one thread.
    #[serde(default)]
    pub thread: Option<ThreadId>,
    /// `add_member`, `remove_member`. The agent id to add or remove. Required by both.
    #[serde(default)]
    pub member: Option<AgentId>,
    /// `thread_start` only. Additional member agent ids; you are added automatically.
    #[serde(default)]
    pub members: Option<Vec<AgentId>>,
    /// `message` only. Id of the message whose body to read — the `id` of a front-matter row
    /// returned by `history` or `inbox`. Required by that mode.
    #[serde(default, alias = "id")]
    pub message_id: Option<String>,
    /// `ack` only. Message ids to acknowledge.
    #[serde(default)]
    pub message_ids: Option<Vec<String>>,
    /// `ack` only. Advance `thread`'s read cursor straight to this per-thread seq. Requires
    /// `thread`.
    #[serde(default)]
    pub to_seq: Option<u64>,
    /// `thread_start` — the thread's topic string, one of the addressing dimensions. `post` — the
    /// message's subject line, required.
    #[serde(default)]
    pub subject: Option<String>,
    /// `thread_list` only. Case-sensitive substring filter over thread subjects.
    #[serde(default)]
    pub subject_contains: Option<String>,
    /// `thread_start` only. A path or globset glob (e.g. `src/**`) for path-based discovery, one of
    /// the addressing dimensions.
    #[serde(default)]
    pub path: Option<String>,
    /// `post` only. Message body (markdown). Stored separately from front-matter and read back only
    /// through mode `message`. Omit it entirely for a subject-only post; supplying an empty or
    /// whitespace-only body is refused, because it would be stored and read back later as
    /// `found: true, body: ""` — indistinguishable from a failed retrieval.
    #[serde(default)]
    pub body: Option<String>,
    /// `post` only. Free-form tags for filtering.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// `post` only. Id of the message this one replies to, for threading.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// `thread_list` only. Also return archived threads. Default false.
    #[serde(default)]
    pub include_archived: Option<bool>,
    /// `inbox` only. Advance read cursors past the returned messages. Default false.
    #[serde(default)]
    pub mark_read: Option<bool>,
    /// `history`, `inbox`, `wait`. Resume token from the previous page's `next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// `history`, `inbox`. Page size. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// `history`, `inbox`, `wait`. Only consider messages from the last N hours. Default 24; pass 0
    /// for ALL history.
    #[serde(default)]
    pub since_hours: Option<u32>,
    /// `wait` only. Seconds to block before returning `timed_out: true`. Default 30, max 300.
    #[serde(default)]
    pub timeout_secs: Option<u32>,
    /// `register` only. Human-readable agent name.
    #[serde(default)]
    pub name: Option<String>,
    /// `register` only. One-line description of this agent's purpose.
    #[serde(default)]
    pub description: Option<String>,
    /// `register` only. Agent version string (e.g. "1.0.0").
    #[serde(default)]
    pub version: Option<String>,
    /// `register` only. Skill labels advertised to peers.
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    /// Every mode. Sub-identity to act as; defaults to this server's own agent. Lets one
    /// orchestrator drive many named subagents, each with its own membership and inbox.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Params for mode `register`: announce or update this agent's A2A card.
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
    /// Optional sub-identity to act as; defaults to the server's own agent. Lets one orchestrator
    /// drive many named subagents.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Response for mode `register`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct AgentRegisterResponse {
    /// The agent id the card was registered under.
    pub agent_id: String,
    /// Always true on success.
    pub registered: bool,
}

/// Params for mode `list`: enumerate known agents, optionally restricted to one thread.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct AgentListParams {
    /// Restrict to members of this thread when set.
    #[serde(default)]
    pub thread: Option<ThreadId>,
    /// Optional sub-identity to act as; defaults to the server's own agent. Lets one orchestrator
    /// drive many named subagents.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// One agent row in a mode `list` response (front-matter view of an `AgentRecord`).
#[derive(Debug, Serialize, schemars::JsonSchema)]
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

/// Response for mode `list`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct AgentListResponse {
    /// Number of agents returned.
    pub total: usize,
    /// The agent rows.
    pub agents: Vec<AgentSummary>,
}

/// A thread front-matter view shared by modes `thread_start` and `thread_list`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ThreadSummary {
    /// Stable thread id.
    pub id: String,
    /// Topic string, when addressed by subject.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Path or GLOB pattern, when addressed by path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The member agent ids.
    pub members: Vec<String>,
    /// The creating agent.
    pub creator: String,
    /// True while active; false once archived.
    pub active: bool,
    /// Creation time, microseconds since the unix epoch.
    pub created_at: i64,
    /// Last-activity time, microseconds since the unix epoch. `0` when the thread has no posts.
    pub last_activity_micros: i64,
    /// True when the thread is STALE: never had a post, or its last post is older than 7 days.
    pub stale: bool,
}

/// The staleness window for a thread, in hours. 168h = 7 days.
pub(super) const STALE_AFTER_HOURS: i64 = 168;

impl ThreadSummary {
    /// Build a thread summary, computing `stale` against `now_micros`.
    pub(super) fn from_thread(thread: &Thread, now_micros: i64) -> Self {
        let last = thread.last_activity;
        let window_micros = STALE_AFTER_HOURS * 3_600_000_000;
        let stale = last == 0 || (now_micros - last) > window_micros;
        Self {
            id: thread.id.as_str().to_string(),
            subject: thread.subject.clone(),
            path: thread.path.clone(),
            members: thread.members.iter().map(|m| m.as_str().to_string()).collect(),
            creator: thread.creator.as_str().to_string(),
            active: thread.active,
            created_at: thread.created_at,
            last_activity_micros: last,
            stale,
        }
    }
}

/// Params for mode `thread_start`: open a conversation addressed by AT LEAST TWO of `subject` /
/// `path` / `members`. Fewer than two is rejected. The caller becomes the creator + a member.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ThreadStartParams {
    /// Topic string.
    #[serde(default)]
    pub subject: Option<String>,
    /// A path or GLOB pattern (globset syntax, e.g. `src/**`) for path-based discovery.
    #[serde(default)]
    pub path: Option<String>,
    /// Explicit additional member agent ids (the caller is added automatically).
    #[serde(default)]
    pub members: Vec<AgentId>,
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Response for mode `thread_start`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ThreadStartResponse {
    /// The created thread.
    pub thread: ThreadSummary,
}

/// Params for mode `thread_list`: list threads DISCOVERABLE to this agent. No global listing — a
/// thread surfaces only when the caller is a member, its cwd matches the thread's path glob, or
/// `subject_contains` matches. Scope context (remote + cwd) is injected by the server.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ThreadListParams {
    /// Optional case-sensitive substring filter over thread subjects.
    #[serde(default)]
    pub subject_contains: Option<String>,
    /// When true, also return archived threads.
    #[serde(default)]
    pub include_archived: bool,
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Response for mode `thread_list`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ThreadListResponse {
    /// Number of threads returned.
    pub total: usize,
    /// The thread rows.
    pub threads: Vec<ThreadSummary>,
}

/// Params for mode `join`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ThreadJoinParams {
    /// The thread to join.
    pub thread: ThreadId,
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Params for mode `leave`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ThreadLeaveParams {
    /// The thread to leave.
    pub thread: ThreadId,
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Response for modes `join` / `leave`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ThreadMembershipResponse {
    /// The thread acted on.
    pub thread: String,
    /// True after a successful join.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub joined: bool,
    /// True after a successful leave.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub left: bool,
}

/// Params for mode `members`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ThreadMembersParams {
    /// The thread whose members to list.
    pub thread: ThreadId,
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Response for mode `members`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ThreadMembersResponse {
    /// The thread queried.
    pub thread: String,
    /// The member agent ids.
    pub members: Vec<String>,
}

/// Params for modes `add_member` / `remove_member` (creator only).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ThreadMemberParams {
    /// The thread to modify.
    pub thread: ThreadId,
    /// The member agent id to add / remove.
    pub member: AgentId,
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Response for modes `add_member` / `remove_member`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ThreadMemberChangeResponse {
    /// The thread acted on.
    pub thread: String,
    /// The member agent id.
    pub member: String,
    /// True after a successful add.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub added: bool,
    /// True after a successful remove.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
}

/// Params for mode `archive` (creator only).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ThreadArchiveParams {
    /// The thread to archive.
    pub thread: ThreadId,
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Response for mode `archive`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ThreadArchiveResponse {
    /// The thread archived.
    pub thread: String,
    /// Always true on success.
    pub archived: bool,
}

/// Params for mode `post`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ThreadPostParams {
    /// Target thread.
    pub thread: ThreadId,
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
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Response for mode `post`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ThreadPostResponse {
    /// The id of the message just stored.
    pub message_id: String,
}

/// Params for mode `history`: read a thread's front-matter, oldest-first, paginated.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ThreadHistoryParams {
    /// The thread to read.
    pub thread: ThreadId,
    /// Resume token from a previous page's `next_cursor` (opaque string).
    #[serde(default)]
    pub cursor: Option<String>,
    /// Maximum messages to return (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Only return messages from the last N hours; defaults to 24. Pass 0 for ALL history.
    #[serde(default)]
    pub since_hours: Option<u32>,
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Front-matter view of a message. Surfaces [`MessageMeta`] front-matter plus its per-thread
/// `seq` — NO body. Fetch the body with mode `message`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct MessageFrontMatter {
    /// Globally unique message id (pass to mode `message` or mode `ack`).
    pub id: String,
    /// Thread the message was posted to.
    pub thread: String,
    /// Authoring agent.
    pub from: String,
    /// Short human subject line.
    pub subject: String,
    /// Post time, microseconds since the unix epoch.
    pub ts_micros: i64,
    /// Age of the message in whole seconds at read time (`now - ts`, floored at 0).
    pub age_secs: i64,
    /// Free-form tags.
    pub tags: Vec<String>,
    /// Id of the message this one replies to, if any.
    pub reply_to: Option<String>,
    /// Per-thread sequence number — pass as mode `ack`'s `to_seq` to bulk-ack up to here.
    pub seq: u64,
    /// Length of the separately-stored body in bytes.
    pub body_len: u32,
    /// Hex SHA-256 of the body for integrity.
    pub body_sha: String,
}

impl MessageFrontMatter {
    /// Build a front-matter row from a [`SeqMeta`], stamping `age_secs` against `now_micros`.
    pub(super) fn from_seq_meta(sm: &SeqMeta, now_micros: i64) -> Self {
        let meta = &sm.meta;
        Self {
            id: meta.id.clone(),
            thread: meta.thread.as_str().to_string(),
            from: meta.from.as_str().to_string(),
            subject: meta.subject.clone(),
            ts_micros: meta.ts_micros,
            age_secs: ((now_micros - meta.ts_micros) / 1_000_000).max(0),
            tags: meta.tags.clone(),
            reply_to: meta.reply_to.clone(),
            seq: sm.seq,
            body_len: meta.body_len,
            body_sha: meta.body_sha.clone(),
        }
    }
}

/// Response for mode `history`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ThreadHistoryResponse {
    /// Number of messages in this page.
    pub total: usize,
    /// Front-matter rows, oldest-first.
    pub messages: Vec<MessageFrontMatter>,
    /// Opaque cursor for the next page; absent means no more results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
}

/// Params for mode `message`: fetch a single message body by id.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MessageGetParams {
    /// The message id (the `id` of a front-matter record).
    pub message_id: String,
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Response for mode `message`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct MessageGetResponse {
    /// The message id queried.
    pub message_id: String,
    /// True when a body was found for the id.
    pub found: bool,
    /// The body decoded as UTF-8 (lossy — bodies are markdown). `None` when the id is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// Params for mode `inbox`: read new front-matter across JOINED threads.
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
    /// Only return messages from the last N hours; defaults to 24. Pass 0 for ALL history.
    #[serde(default)]
    pub since_hours: Option<u32>,
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Response for mode `inbox`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct InboxReadResponse {
    /// Number of messages in this page.
    pub total: usize,
    /// Count of unread messages remaining after this page.
    pub unread: u32,
    /// Front-matter rows across joined threads.
    pub messages: Vec<MessageFrontMatter>,
    /// Opaque cursor for the next page; absent means no more results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
}

/// Params for mode `ack`: advance this agent's per-thread read cursors past acked messages.
///
/// Two modes, combinable:
/// * `message_ids` — resolve each id to its `(thread, seq)`, then advance each thread's cursor.
/// * `thread` + `to_seq` — advance that one thread's cursor straight to `to_seq`.
///
/// At least one mode must be supplied; an empty request is rejected.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct InboxAckParams {
    /// Message ids to ack (mode a).
    #[serde(default)]
    pub message_ids: Vec<String>,
    /// Target thread for the bulk `to_seq` mode (mode b).
    #[serde(default)]
    pub thread: Option<ThreadId>,
    /// Advance `thread`'s cursor straight to this seq (mode b). Requires `thread`.
    #[serde(default)]
    pub to_seq: Option<u64>,
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// One `(thread, new_seq)` cursor advance recorded by mode `ack`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct CursorAdvance {
    /// The thread whose per-agent read cursor advanced.
    pub thread: String,
    /// The cursor's new seq after the advance.
    pub seq: u64,
}

/// Response for mode `ack`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct InboxAckResponse {
    /// Number of message ids that resolved and were acked.
    pub acked: usize,
    /// The `(thread, new_seq)` cursor advances this call produced.
    pub cursors_advanced: Vec<CursorAdvance>,
}

/// Params for mode `wait`: block until a peer posts to a joined thread (or the one thread named
/// in `thread`), or until `timeout_secs` elapses. NEVER marks read — there is no `mark_read`
/// param; follow up with `inbox_read(mark_read: true)` or mode `ack` once you've handled the
/// page. A long-poll replacement for a caller looping mode `inbox` / mode `thread_list`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct InboxWaitParams {
    /// Maximum seconds to block before returning `timed_out: true` (default 30, max 300).
    #[serde(default)]
    pub timeout_secs: Option<u32>,
    /// Restrict the wake to this thread only. `None` wakes on ANY joined thread.
    #[serde(default)]
    pub thread: Option<ThreadId>,
    /// Only consider messages from the last N hours; defaults to 24. Pass 0 for ALL history.
    #[serde(default)]
    pub since_hours: Option<u32>,
    /// Resume token from a previous page's `next_cursor` (opaque string).
    #[serde(default)]
    pub cursor: Option<String>,
    /// Optional sub-identity to act as; defaults to the server's own agent.
    #[serde(default)]
    pub as_agent: Option<String>,
}

/// Response for mode `wait`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct InboxWaitResponse {
    /// True when the call returned because `timeout_secs` elapsed with nothing new to report.
    pub timed_out: bool,
    /// Number of messages in this page.
    pub total: usize,
    /// Count of unread messages remaining after this page.
    pub unread: u32,
    /// Front-matter rows across joined threads (or the one filtered thread). NOT marked read.
    pub messages: Vec<MessageFrontMatter>,
    /// Opaque cursor for the next page; absent means no more results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
}
