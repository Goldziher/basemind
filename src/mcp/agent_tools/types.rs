//! Wire arguments and replies of the three agent tools (ADR-0021).
//!
//! The argument structs follow the verb-tool contract of `types_tools.rs`: flat,
//! `deny_unknown_fields`, parsed through [`Strict`](crate::mcp::strict::Strict). Every field but
//! `agent_post`'s `body` is an `Option`, and none carries `#[serde(default)]`: serde already reads
//! a missing `Option` as `None`, and the attribute would add `"default":null` to each property of
//! a schema that has to fit, with the two others and the descriptions, in 5,000 bytes.
//!
//! Ids, paths and cursors are plain strings here. They are parsed by the `from_fields`
//! constructors of [`fields`](crate::comms::v2::fields), the one place a flat argument set becomes
//! a request, so a malformed id is refused with its grammar and an empty string is the field left
//! out.
//!
//! The replies leave out what says nothing (`created: false`, an empty `pending`, a `null`
//! cursor): an inbox is read on most turns, and every key costs tokens on each of them. `notices`
//! and an inbox's `more` are always written.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::comms::v2::api::{
    Actor, Cursor, InboxMessage, InboxReply, MessageId, PostReply, Principal, RepoKey, ThreadId, ThreadSummary,
    ThreadsReply, WaitOutcome,
};
use crate::mcp::strict::ToolArgs;

/// MCP name of the tool that posts a message.
pub const AGENT_POST: &str = "agent_post";

/// MCP name of the tool that reads messages.
pub const AGENT_INBOX: &str = "agent_inbox";

/// MCP name of the tool that lists and changes threads.
pub const AGENT_THREADS: &str = "agent_threads";

/// The three agent tools, in declaration order. Deferred, so not part of `VERB_TOOLS`.
pub const AGENT_TOOLS: &[&str] = &[AGENT_POST, AGENT_INBOX, AGENT_THREADS];

impl ToolArgs for AgentPostParams {
    const TOOL: &'static str = AGENT_POST;
}

impl ToolArgs for AgentInboxParams {
    const TOOL: &'static str = AGENT_INBOX;
}

impl ToolArgs for AgentThreadsParams {
    const TOOL: &'static str = AGENT_THREADS;
}

/// Arguments of `agent_post`.
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPostParams {
    /// The message, at most 16 KiB.
    pub body: String,
    /// Thread to post into, e.g. `t-abcdef`.
    pub thread: Option<String>,
    /// Message to answer, e.g. `t-abcdef.3`; posts into its thread.
    pub reply_to: Option<String>,
    /// Starts a new thread: its one-line topic, at most 200 bytes.
    pub subject: Option<String>,
    /// With `subject`: agent ids that become members; the thread is then invitation-only.
    pub to: Option<Vec<String>>,
    /// With `subject`: repo-relative path prefixes the thread is about.
    pub paths: Option<Vec<String>>,
    /// Sub-identity to act as, `[a-z0-9_-]{1,32}`. A subagent passes its own name.
    #[serde(rename = "as")]
    pub as_: Option<String>,
}

/// Arguments of `agent_inbox`.
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentInboxParams {
    /// Only this thread.
    pub thread: Option<String>,
    /// One message by id, with its whole body. Only `as` may accompany it.
    pub id: Option<String>,
    /// Default true. False reads history, your own posts included, and marks nothing.
    pub unread: Option<bool>,
    /// Return unread messages without marking them read.
    pub peek: Option<bool>,
    /// For hooks: list unread messages not announced yet, without bodies; marks nothing read.
    pub announce: Option<bool>,
    /// When nothing is unread, wait up to this many seconds (0-40) for a message.
    pub wait_secs: Option<u32>,
    /// Max messages (default 20, max 100).
    pub limit: Option<u32>,
    /// `next_cursor` of the previous `peek` or `unread:false` page.
    pub cursor: Option<String>,
    /// Sub-identity to read as; see `agent_post`.
    #[serde(rename = "as")]
    pub as_: Option<String>,
}

/// Arguments of `agent_threads`.
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentThreadsParams {
    /// List threads whose subject contains this (3+ characters).
    pub query: Option<String>,
    /// List threads about this repo-relative path.
    pub path: Option<String>,
    /// Thread to describe, or to change with one of `join`, `leave`, `invite`, `archive`.
    pub thread: Option<String>,
    /// Become a member of `thread`.
    pub join: Option<bool>,
    /// Stop being a member of `thread`.
    pub leave: Option<bool>,
    /// Agent ids to make members of `thread`.
    pub invite: Option<Vec<String>>,
    /// Close `thread` to new posts; only its creator may.
    pub archive: Option<bool>,
    /// Include archived threads in a listing.
    pub archived: Option<bool>,
    /// Max threads (default 20, max 100).
    pub limit: Option<u32>,
    /// `next_cursor` of the previous listing page.
    pub cursor: Option<String>,
    /// Sub-identity to act as; see `agent_post`.
    #[serde(rename = "as")]
    pub as_: Option<String>,
}

fn is_false(flag: &bool) -> bool {
    !*flag
}

fn did_not_wait(wait: &WaitOutcome) -> bool {
    *wait == WaitOutcome::NotWaited
}

/// Reply of `agent_post`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AgentPostResponse {
    /// The posted message.
    pub id: MessageId,
    /// The thread it went into.
    pub thread: ThreadId,
    /// Whether this post created the thread.
    #[serde(skip_serializing_if = "is_false")]
    pub created: bool,
    /// Recipients that have made no call yet: not started, long idle, or mistyped.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<Actor>,
    /// Sentences that are not about this request.
    pub notices: Vec<String>,
}

impl AgentPostResponse {
    /// Shape the store's reply, with the tool layer's notices after the daemon's.
    pub(super) fn from_reply(reply: PostReply, notices: Vec<String>) -> Self {
        Self {
            id: reply.id,
            thread: reply.thread,
            created: reply.created,
            pending: reply.pending,
            notices,
        }
    }
}

/// One message in an `agent_inbox` reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AgentMessage {
    /// The message's id; pass it as `reply_to` to answer, or as `id` to read it whole.
    pub id: MessageId,
    /// Subject of its thread.
    pub subject: String,
    /// Who posted it.
    pub from: Actor,
    /// Post time in microseconds since the unix epoch.
    pub ts_micros: i64,
    /// The message it replies to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageId>,
    /// The body, cut to 4 KiB unless the message was read by `id`. Left out by an announcing
    /// read, which returns front matter only.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// Size of the whole body, in bytes.
    pub body_bytes: u64,
    /// Whether `body` was cut.
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
}

impl From<InboxMessage> for AgentMessage {
    /// The store's message as it is. The store cut the body and counted it against the response
    /// budget; cutting or dropping here would lose what a marking read already marked.
    fn from(message: InboxMessage) -> Self {
        Self {
            id: message.id,
            subject: message.subject,
            from: message.from,
            ts_micros: message.ts_micros,
            reply_to: message.reply_to,
            body: message.body,
            body_bytes: message.body_bytes,
            truncated: message.truncated,
        }
    }
}

/// Reply of `agent_inbox`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AgentInboxResponse {
    /// The messages, oldest first.
    pub messages: Vec<AgentMessage>,
    /// The last message now read in each thread this read advanced.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub read_to: Vec<MessageId>,
    /// Whether the limit or the 16 KiB response budget left messages out. After a read that marks,
    /// call again; otherwise continue with `next_cursor`.
    pub more: bool,
    /// Pass back as `cursor` to continue a `peek` or `unread:false` read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// How the wait ended, when the read waited.
    #[serde(skip_serializing_if = "did_not_wait")]
    pub wait: WaitOutcome,
    /// Sentences that are not about this request.
    pub notices: Vec<String>,
}

impl AgentInboxResponse {
    /// Shape the store's reply, with the tool layer's notices after the daemon's.
    pub(super) fn from_reply(reply: InboxReply, notices: Vec<String>) -> Self {
        Self {
            messages: reply.messages.into_iter().map(AgentMessage::from).collect(),
            read_to: reply.read_to,
            more: reply.more,
            next_cursor: reply.next_cursor,
            wait: reply.wait,
            notices,
        }
    }
}

/// One thread in an `agent_threads` reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AgentThread {
    /// The thread's id.
    pub id: ThreadId,
    /// Its subject.
    pub subject: String,
    /// The repository it belongs to.
    pub repo: RepoKey,
    /// The path prefixes it is about.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Whether any agent of its repository may join.
    pub open: bool,
    /// Whether it is archived.
    #[serde(skip_serializing_if = "is_false")]
    pub archived: bool,
    /// Who created it, and may archive it.
    pub creator: Actor,
    /// Whether the caller is a member.
    pub member: bool,
    /// Messages from other agents the caller has not read.
    pub unread: u64,
    /// Sequence number of the newest message.
    pub head: u64,
    /// Time of the last post, in microseconds since the unix epoch.
    pub last_activity_micros: i64,
    /// The members other than the caller's principal.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<Principal>,
}

impl From<ThreadSummary> for AgentThread {
    fn from(summary: ThreadSummary) -> Self {
        Self {
            id: summary.id,
            subject: summary.subject,
            repo: summary.repo,
            paths: summary
                .paths
                .iter()
                .map(|path| path.to_str_lossy().into_owned())
                .collect(),
            open: summary.open,
            archived: summary.archived,
            creator: summary.creator,
            member: summary.member,
            unread: summary.unread,
            head: summary.head,
            last_activity_micros: summary.last_activity_micros,
            peers: summary.peers,
        }
    }
}

/// Reply of `agent_threads`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AgentThreadsResponse {
    /// The threads, newest activity first; after a change, the one thread changed.
    pub threads: Vec<AgentThread>,
    /// Invited agents that have made no call yet: not started, long idle, or mistyped.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<Actor>,
    /// Pass back as `cursor` to continue a listing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// Sentences that are not about this request.
    pub notices: Vec<String>,
}

impl AgentThreadsResponse {
    /// Shape the store's reply, with the tool layer's notices after the daemon's.
    pub(super) fn from_reply(reply: ThreadsReply, notices: Vec<String>) -> Self {
        Self {
            threads: reply.threads.into_iter().map(AgentThread::from).collect(),
            pending: reply.pending,
            next_cursor: reply.next_cursor,
            notices,
        }
    }
}
