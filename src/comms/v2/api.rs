//! The transport-neutral contract of agent comms v2 (ADR-0021): what the daemon, the three MCP
//! tools, the CLI and the hooks all exchange.
//!
//! One set of types, so a rule is written once. Constraints the tool schema can only describe
//! ("one of `thread`, `reply_to`, `subject`", "at most one of `join`, `leave`, `invite`,
//! `archive`") are enums here, and [`PostTarget::from_fields`] / [`ThreadAction::from_fields`]
//! are the single place a flat argument set is turned into one. Every type serializes as a named
//! map with defaulted optional fields, the shape `protocol.rs` uses, so it goes on the wire as is.

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use super::ids::{
    Actor, ID_SEPARATOR, IdKind, InvalidId, MAX_REPLAY_KEY_LEN, MAX_REPO_KEY_LEN, MAX_SUB_NAME_LEN, MessageId,
    PATH_REPO_PREFIX, Principal, ReplayKey, RepoKey, SubName, THREAD_ID_DIGITS, THREAD_ID_PREFIX, ThreadId,
};
use crate::path::RelPath;

/// Longest body returned inline, in bytes. A longer one is returned cut and flagged; the whole
/// body is read by message id.
pub const MAX_INLINE_BODY_BYTES: usize = 4 * 1024;

/// Inline body bytes one inbox reply may carry. Messages past it are left unread for the next call.
pub const RESPONSE_BUDGET_BYTES: usize = 16 * 1024;

/// Longest wait an inbox read may ask for, in seconds: under the hosts' tool-call timeout.
pub const MAX_WAIT_SECS: u32 = 40;

/// Page size when a request names none.
pub const DEFAULT_LIMIT: u32 = 20;

/// Largest page size honoured; a larger `limit` is clamped to it. Not set by ADR-0021.
pub const MAX_LIMIT: u32 = 100;

/// How long a message is kept.
pub const MESSAGE_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Messages kept per thread; the oldest are dropped past it.
pub const MAX_MESSAGES_PER_THREAD: u64 = 1_000;

/// Active (not archived) threads one repository may hold.
pub const MAX_ACTIVE_THREADS_PER_REPO: usize = 256;

/// Shortest discovery query, in characters, so a query cannot enumerate every subject.
pub const MIN_QUERY_CHARS: usize = 3;

/// How long a replay key keeps returning its first result.
pub const REPLAY_TTL: Duration = Duration::from_secs(60 * 60);

/// Version of the comms store layout. Not tied to the release, so an upgrade keeps history.
pub const COMMS_STORE_VER: u32 = 1000;

/// Largest body a post may carry, in bytes. Not set by ADR-0021, which bounds only what is
/// returned inline; a stored body still needs a ceiling.
pub const MAX_POST_BODY_BYTES: usize = 64 * 1024;

/// Longest thread subject, in bytes. Not set by ADR-0021; a subject is a one-line topic.
pub const MAX_SUBJECT_BYTES: usize = 200;

/// A refusal, worded as what the caller should do next.
///
/// Serializable, so the daemon's refusal reaches a tool reply unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum CommsError {
    /// An identifier failed validation.
    #[error(transparent)]
    InvalidId(#[from] InvalidId),
    /// No thread has this id.
    #[error("thread {thread} not found: list your threads with agent_threads, or search with `query` or `path`")]
    ThreadNotFound {
        /// The id that was looked up.
        thread: ThreadId,
    },
    /// No message has this id: never posted, or expired.
    #[error("message {id} not found: it was never posted or has expired; read the thread with agent_inbox `thread`")]
    MessageNotFound {
        /// The id that was looked up.
        id: MessageId,
    },
    /// The caller reads from, posts to, leaves or invites into a thread it is not a member of.
    #[error(
        "not a member of thread {thread}: join it with agent_threads {{thread, join: true}}, or ask a member to invite you"
    )]
    NotMember {
        /// The thread the caller is not in.
        thread: ThreadId,
    },
    /// The thread is invitation-only or belongs to another repository, so it cannot be joined or
    /// looked up from here.
    #[error(
        "thread {thread} is closed to this repository (invitation-only, or owned by another repository): ask a member to invite you"
    )]
    ThreadClosed {
        /// The thread that is not open to the caller's repository.
        thread: ThreadId,
    },
    /// Only the creator may archive a thread.
    #[error(
        "only the creator of thread {thread} may archive it: leave it with agent_threads {{thread, leave: true}} instead"
    )]
    NotCreator {
        /// The thread the caller did not create.
        thread: ThreadId,
    },
    /// The thread is archived: readable, but closed to posts, joins and invitations.
    #[error("thread {thread} is archived: read it with agent_inbox, or start a new thread by posting with `subject`")]
    ThreadArchived {
        /// The archived thread.
        thread: ThreadId,
    },
    /// More than one of `join`, `leave`, `invite`, `archive` in one call.
    #[error("pass at most one of join, leave, invite, archive; this call set [{}]: make one call per change", .fields.join(", "))]
    MultipleUpdates {
        /// The update fields the call set.
        fields: Vec<String>,
    },
    /// A post named none, or more than one, of `thread`, `reply_to`, `subject`.
    #[error("pass exactly one of thread, reply_to, subject; this call set [{}]: `subject` starts a thread, the others post into one", .given.join(", "))]
    PostTarget {
        /// The target fields the call set.
        given: Vec<String>,
    },
    /// `to` or `paths` on a post that does not create a thread.
    #[error(
        "`{field}` is fixed when a thread is created and needs `subject`: to add members to an existing thread use agent_threads {{thread, invite}}"
    )]
    CreationOnly {
        /// The creation-only field the call set.
        field: String,
    },
    /// A discovery query shorter than [`MIN_QUERY_CHARS`].
    #[error("`query` needs at least {min} characters: lengthen it, or search by `path`")]
    QueryTooShort {
        /// The minimum length, in characters.
        min: usize,
    },
    /// The repository already holds [`MAX_ACTIVE_THREADS_PER_REPO`] active threads.
    #[error(
        "repository {repo} already has {cap} active threads: post into an existing one, or archive one you created"
    )]
    ThreadCapReached {
        /// The repository at its cap.
        repo: RepoKey,
        /// The cap.
        cap: usize,
    },
    /// The store was written by a newer basemind; this binary must not write to it.
    #[error(
        "the comms store is version {store}, newer than this basemind understands ({binary}): upgrade basemind, and restart the daemon so every session runs the same version"
    )]
    StoreNewer {
        /// The version found on disk.
        store: u32,
        /// The newest version this binary writes.
        binary: u32,
    },
    /// `wait_secs` above [`MAX_WAIT_SECS`].
    #[error("`wait_secs` is {given}, above the maximum of {max}: wait at most {max} seconds and call again")]
    WaitOutOfRange {
        /// The requested wait.
        given: u32,
        /// The maximum wait.
        max: u32,
    },
    /// A cursor this store did not issue, or issued for a different request.
    #[error("`cursor` {cursor:?} is not a `next_cursor` of this request: repeat the call without `cursor`")]
    InvalidCursor {
        /// The refused cursor.
        cursor: String,
    },
    /// A post without a body.
    #[error("`body` is empty: write the message to post")]
    EmptyBody,
    /// A body over [`MAX_POST_BODY_BYTES`].
    #[error("`body` is {len} bytes, above the maximum of {max}: shorten it, or split it across posts")]
    BodyTooLarge {
        /// The body's length in bytes.
        len: usize,
        /// The maximum length in bytes.
        max: usize,
    },
    /// A subject that is empty, multi-line or over [`MAX_SUBJECT_BYTES`].
    #[error("`subject` must be one non-empty line of at most {max} bytes: shorten it and put the detail in `body`")]
    InvalidSubject {
        /// The maximum length in bytes.
        max: usize,
    },
    /// A thread path that is not a repository-relative prefix.
    #[error(
        "path {path:?} is not a repository-relative path prefix: pass it relative to the repository root, without `..`"
    )]
    InvalidPath {
        /// The refused path.
        path: String,
    },
    /// An invitation naming nobody.
    #[error("`invite` is empty: name at least one agent id")]
    EmptyInvite,
    /// The store or the daemon failed; nothing about the request was wrong.
    #[error("the comms store failed: {message}; retry, and run `basemind agents status` if it persists")]
    Backend {
        /// What failed.
        message: String,
    },
}

/// Where a post goes. Exactly one, by construction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostTarget {
    /// Into an existing thread.
    Thread(ThreadId),
    /// Into the thread of an existing message, as a reply to it.
    ReplyTo(MessageId),
    /// Into a new thread. The subject is a topic, not a key: two such posts make two threads.
    NewThread {
        /// The thread's subject.
        subject: String,
        /// Members beside the creator. Any makes the thread invitation-only; none leaves it open
        /// to its repository.
        #[serde(default)]
        to: Vec<Actor>,
        /// Repository-relative path prefixes the thread is about, fixed here.
        #[serde(default)]
        paths: Vec<RelPath>,
    },
}

impl PostTarget {
    /// Build the target from a tool's flat fields, refusing zero or several targets and refusing
    /// `to` / `paths` without `subject` rather than dropping them.
    pub fn from_fields(
        thread: Option<ThreadId>,
        reply_to: Option<MessageId>,
        subject: Option<String>,
        to: Vec<Actor>,
        paths: Vec<RelPath>,
    ) -> Result<Self, CommsError> {
        let given: Vec<String> = [
            ("thread", thread.is_some()),
            ("reply_to", reply_to.is_some()),
            ("subject", subject.is_some()),
        ]
        .into_iter()
        .filter(|(_, set)| *set)
        .map(|(field, _)| field.to_string())
        .collect();
        match (thread, reply_to, subject) {
            (None, None, Some(subject)) => Ok(Self::NewThread { subject, to, paths }),
            (Some(_), None, None) | (None, Some(_), None) if !to.is_empty() => Err(CommsError::CreationOnly {
                field: "to".to_string(),
            }),
            (Some(_), None, None) | (None, Some(_), None) if !paths.is_empty() => Err(CommsError::CreationOnly {
                field: "paths".to_string(),
            }),
            (Some(thread), None, None) => Ok(Self::Thread(thread)),
            (None, Some(reply_to), None) => Ok(Self::ReplyTo(reply_to)),
            _ => Err(CommsError::PostTarget { given }),
        }
    }
}

/// `agent_post`: post a message, creating the thread when the target is a new one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostRequest {
    /// Who posts.
    pub actor: Actor,
    /// The caller's repository: the owner of a thread this post creates.
    pub repo: RepoKey,
    /// Where the post goes.
    pub target: PostTarget,
    /// The message text.
    pub body: String,
    /// The relay's key for this request; a replay returns the first reply and posts nothing.
    #[serde(default)]
    pub replay_key: Option<ReplayKey>,
}

/// What an inbox read does to the caller's positions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mark {
    /// Mark what is returned as read.
    #[default]
    Read,
    /// Change nothing: the tool's `peek`.
    Peek,
    /// The hooks' read: return only what has not been announced yet and record it as announced,
    /// leaving it unread. A hook has no replay key, so it must never consume messages.
    Notify,
}

/// Which messages an inbox read covers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxSelect {
    /// Every thread the caller is a member of.
    #[default]
    All,
    /// One thread.
    Thread(ThreadId),
    /// One message, with its whole body. A point lookup: it moves no read position and ignores
    /// `unread`, `limit`, `cursor` and `wait_secs`.
    Message(MessageId),
}

/// `agent_inbox`: read messages, bodies inline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxRequest {
    /// Who reads.
    pub actor: Actor,
    /// The caller's repository. Not a filter: membership alone decides what is read.
    pub repo: RepoKey,
    /// Which messages to read.
    #[serde(default)]
    pub select: InboxSelect,
    /// Only unread messages from other agents (the default); `false` reads history, oldest first,
    /// the caller's own posts included.
    #[serde(default = "default_true")]
    pub unread: bool,
    /// What the read does to the caller's positions.
    #[serde(default)]
    pub mark: Mark,
    /// With nothing unread, block up to this many seconds for a message. At most [`MAX_WAIT_SECS`].
    #[serde(default)]
    pub wait_secs: u32,
    /// Most messages to return; [`DEFAULT_LIMIT`] when absent, clamped to [`MAX_LIMIT`].
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// The relay's key for this request; a replay returns the range first delivered instead of an
    /// empty page.
    #[serde(default)]
    pub replay_key: Option<ReplayKey>,
}

fn default_true() -> bool {
    true
}

impl InboxRequest {
    /// The tool's defaults: every thread, unread only, marking as read, no wait.
    pub fn new(actor: Actor, repo: RepoKey) -> Self {
        Self {
            actor,
            repo,
            select: InboxSelect::All,
            unread: true,
            mark: Mark::Read,
            wait_secs: 0,
            limit: None,
            cursor: None,
            replay_key: None,
        }
    }
}

/// `agent_threads` without an update: list or discover threads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadQuery {
    /// Who asks.
    pub actor: Actor,
    /// The caller's repository: the only one whose open threads can be discovered.
    pub repo: RepoKey,
    /// Case-insensitive substring of the subject, at least [`MIN_QUERY_CHARS`] characters.
    #[serde(default)]
    pub query: Option<String>,
    /// A repository-relative path; matches a thread whose `paths` contain a prefix of it or
    /// something under it.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Describe this one thread.
    #[serde(default)]
    pub thread: Option<ThreadId>,
    /// Include archived threads.
    #[serde(default)]
    pub archived: bool,
    /// Most threads to return; [`DEFAULT_LIMIT`] when absent, clamped to [`MAX_LIMIT`].
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

impl ThreadQuery {
    /// The tool with no arguments: the caller's own active threads.
    pub fn new(actor: Actor, repo: RepoKey) -> Self {
        Self {
            actor,
            repo,
            query: None,
            path: None,
            thread: None,
            archived: false,
            limit: None,
            cursor: None,
        }
    }
}

/// The one change an `agent_threads` call makes. At most one, by construction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadAction {
    /// Become a member, reading from the head.
    Join,
    /// Stop being a member.
    Leave,
    /// Make these agents members, each reading from the head as of now.
    Invite(Vec<Actor>),
    /// Close the thread to posts, joins and invitations. Creator only.
    Archive,
}

impl ThreadAction {
    /// Build the action from a tool's flat fields: `None` when the call is a query, a refusal
    /// naming the fields when it sets more than one.
    pub fn from_fields(
        join: bool,
        leave: bool,
        invite: Option<Vec<Actor>>,
        archive: bool,
    ) -> Result<Option<Self>, CommsError> {
        let fields: Vec<String> = [
            ("join", join),
            ("leave", leave),
            ("invite", invite.is_some()),
            ("archive", archive),
        ]
        .into_iter()
        .filter(|(_, set)| *set)
        .map(|(field, _)| field.to_string())
        .collect();
        if fields.len() > 1 {
            return Err(CommsError::MultipleUpdates { fields });
        }
        Ok(match invite {
            Some(invited) if invited.is_empty() => return Err(CommsError::EmptyInvite),
            Some(invited) => Some(Self::Invite(invited)),
            None if join => Some(Self::Join),
            None if leave => Some(Self::Leave),
            None if archive => Some(Self::Archive),
            None => None,
        })
    }
}

/// `agent_threads` with an update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadUpdate {
    /// Who acts.
    pub actor: Actor,
    /// The caller's repository: a thread can be joined only from the repository that owns it.
    pub repo: RepoKey,
    /// The thread to change.
    pub thread: ThreadId,
    /// The change.
    pub action: ThreadAction,
}

/// Reply to a post.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostReply {
    /// The posted message.
    pub id: MessageId,
    /// The thread it went into.
    pub thread: ThreadId,
    /// Whether this post created the thread.
    #[serde(default)]
    pub created: bool,
    /// Recipients the store has never seen: not yet started, or mistyped.
    #[serde(default)]
    pub pending: Vec<Actor>,
    /// Something the agent should know that is not about this request, such as a shared identity.
    #[serde(default)]
    pub notice: Option<String>,
}

/// One message in an inbox reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxMessage {
    /// The message's id.
    pub id: MessageId,
    /// Subject of the thread it is in.
    pub subject: String,
    /// Who posted it.
    pub from: Actor,
    /// Post time in microseconds since the unix epoch.
    pub ts_micros: i64,
    /// The message it replies to, when it is a reply.
    #[serde(default)]
    pub reply_to: Option<MessageId>,
    /// The body, cut to [`MAX_INLINE_BODY_BYTES`] unless the message was read by id.
    pub body: String,
    /// Whether `body` was cut. The message still counts as read; the rest is read by id.
    #[serde(default)]
    pub truncated: bool,
}

/// How the wait of an inbox read ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitOutcome {
    /// The read did not wait: it asked for none, or had something to return at once.
    #[default]
    NotWaited,
    /// A message arrived during the wait.
    Arrived,
    /// The wait ran out with nothing to return.
    TimedOut,
}

/// Reply to an inbox read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxReply {
    /// The messages, oldest first.
    #[serde(default)]
    pub messages: Vec<InboxMessage>,
    /// The last message now read in each thread this read advanced. Empty unless marking as read.
    #[serde(default)]
    pub read_to: Vec<MessageId>,
    /// Set when the limit or the response budget left messages out; pass it back to continue.
    #[serde(default)]
    pub next_cursor: Option<String>,
    /// Something the agent should know that is not about this request, such as a shared identity.
    #[serde(default)]
    pub notice: Option<String>,
    /// How the wait ended.
    #[serde(default)]
    pub wait: WaitOutcome,
}

/// One thread as the caller sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummary {
    /// The thread's id.
    pub id: ThreadId,
    /// Its subject.
    pub subject: String,
    /// The repository it belongs to.
    pub repo: RepoKey,
    /// The path prefixes it is about.
    #[serde(default)]
    pub paths: Vec<RelPath>,
    /// Whether any agent of its repository may join; `false` for an invitation-only thread.
    pub open: bool,
    /// Whether it is archived.
    #[serde(default)]
    pub archived: bool,
    /// Who created it, and may archive it.
    pub creator: Actor,
    /// Whether the caller is a member.
    pub member: bool,
    /// Messages from other agents the caller has not read. Zero for a non-member.
    #[serde(default)]
    pub unread: u64,
    /// Sequence number of the newest message.
    pub head: u64,
    /// Time of the newest message in microseconds since the unix epoch.
    pub last_activity_micros: i64,
    /// The members other than the caller.
    #[serde(default)]
    pub peers: Vec<Actor>,
}

/// Reply to a thread query or update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadsReply {
    /// The threads, in ascending id order; after an update, the one thread changed.
    #[serde(default)]
    pub threads: Vec<ThreadSummary>,
    /// Invited agents the store has never seen: not yet started, or mistyped.
    #[serde(default)]
    pub pending: Vec<Actor>,
    /// Set when the limit left threads out; pass it back to continue.
    #[serde(default)]
    pub next_cursor: Option<String>,
    /// Something the agent should know that is not about this request, such as a shared identity.
    #[serde(default)]
    pub notice: Option<String>,
}

/// The agent-coordination operations, independent of transport.
///
/// Synchronous on purpose: the one blocking operation, the inbox wait, is a field of its request
/// and runs inside the implementation, so a caller needs no runtime to use it.
pub trait AgentOps {
    /// Post a message. Requires membership, except that a [`PostTarget::NewThread`] creates the
    /// thread with the caller and every `to` recipient as members at read position 0.
    fn post(&self, request: PostRequest) -> Result<PostReply, CommsError>;

    /// Read messages from the caller's threads. Marks what it returns as read unless the request's
    /// [`Mark`] says otherwise.
    fn inbox(&self, request: InboxRequest) -> Result<InboxReply, CommsError>;

    /// List the caller's threads, or discover open threads of its repository by query or path.
    fn threads_query(&self, request: ThreadQuery) -> Result<ThreadsReply, CommsError>;

    /// Join, leave, invite into or archive one thread.
    fn threads_update(&self, request: ThreadUpdate) -> Result<ThreadsReply, CommsError>;
}

/// The page size a request's `limit` means.
pub fn effective_limit(limit: Option<u32>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize
}

/// The wait a request's `wait_secs` means, refused above [`MAX_WAIT_SECS`] rather than clamped so
/// a caller is never left waiting less than it believes.
pub fn checked_wait(wait_secs: u32) -> Result<Duration, CommsError> {
    if wait_secs > MAX_WAIT_SECS {
        return Err(CommsError::WaitOutOfRange {
            given: wait_secs,
            max: MAX_WAIT_SECS,
        });
    }
    Ok(Duration::from_secs(u64::from(wait_secs)))
}

/// The part of `body` returned inline, and whether it was cut. Cut at a character boundary at or
/// below [`MAX_INLINE_BODY_BYTES`], so the result is always valid text.
pub fn inline_body(body: &str) -> (&str, bool) {
    if body.len() <= MAX_INLINE_BODY_BYTES {
        return (body, false);
    }
    let mut end = MAX_INLINE_BODY_BYTES;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    (&body[..end], true)
}

/// Check a post body: not blank, at most [`MAX_POST_BODY_BYTES`].
pub fn check_body(body: &str) -> Result<(), CommsError> {
    if body.trim().is_empty() {
        return Err(CommsError::EmptyBody);
    }
    if body.len() > MAX_POST_BODY_BYTES {
        return Err(CommsError::BodyTooLarge {
            len: body.len(),
            max: MAX_POST_BODY_BYTES,
        });
    }
    Ok(())
}

/// The subject a new thread stores: trimmed, one non-empty line of at most [`MAX_SUBJECT_BYTES`].
pub fn normalize_subject(subject: &str) -> Result<String, CommsError> {
    let subject = subject.trim();
    if subject.is_empty() || subject.len() > MAX_SUBJECT_BYTES || subject.chars().any(char::is_control) {
        return Err(CommsError::InvalidSubject { max: MAX_SUBJECT_BYTES });
    }
    Ok(subject.to_string())
}

/// The form a discovery query is matched in: trimmed and lowercased, refused below
/// [`MIN_QUERY_CHARS`] characters.
pub fn normalize_query(query: &str) -> Result<String, CommsError> {
    let query = query.trim().to_lowercase();
    if query.chars().count() < MIN_QUERY_CHARS {
        return Err(CommsError::QueryTooShort { min: MIN_QUERY_CHARS });
    }
    Ok(query)
}

/// Whether `subject` contains `normalized_query` (the output of [`normalize_query`]), ignoring case.
pub fn subject_matches(subject: &str, normalized_query: &str) -> bool {
    subject.to_lowercase().contains(normalized_query)
}

/// The form a thread path is stored and matched in: trailing slashes removed. Refused when it is
/// empty, absolute, or has an empty, `.` or `..` component, so a stored prefix always names a
/// place inside the repository.
pub fn normalize_thread_path(path: &RelPath) -> Result<RelPath, CommsError> {
    let invalid = || CommsError::InvalidPath { path: path.to_string() };
    let mut bytes = path.as_bytes();
    while let [rest @ .., b'/'] = bytes {
        bytes = rest;
    }
    if bytes.is_empty() || path.is_external() || bytes.contains(&0) {
        return Err(invalid());
    }
    if bytes
        .split(|byte| *byte == b'/')
        .any(|component| component.is_empty() || component == b"." || component == b"..")
    {
        return Err(invalid());
    }
    Ok(RelPath::from(bytes))
}

/// Whether `prefix` is `path` or a directory above it. Whole components only: `src/comms` is a
/// prefix of `src/comms/store.rs`, not of `src/comms2`.
pub fn is_path_prefix(prefix: &RelPath, path: &RelPath) -> bool {
    match path.as_bytes().strip_prefix(prefix.as_bytes()) {
        Some(rest) => rest.is_empty() || rest.first() == Some(&b'/'),
        None => false,
    }
}

/// Whether a discovery `path` selects a thread about `thread_paths`: it is a prefix of one of
/// them, or one of them is a prefix of it. Both sides are [`normalize_thread_path`] output.
pub fn path_selects(path: &RelPath, thread_paths: &[RelPath]) -> bool {
    thread_paths
        .iter()
        .any(|thread_path| is_path_prefix(path, thread_path) || is_path_prefix(thread_path, path))
}

/// The notice every agent-tool reply carries while more than one live session shares `principal`.
pub fn shared_identity_notice(principal: &str, sessions: usize) -> String {
    format!(
        "{sessions} live sessions share the identity {principal}: they share one inbox and read position. \
         Set BASEMIND_AGENT_ID in one of them, or pass `as`, to separate them."
    )
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
