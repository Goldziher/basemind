//! The transport-neutral contract of agent comms v2 (ADR-0021): what the daemon, the three MCP
//! tools, the CLI and the hooks all exchange.
//!
//! One set of types, so a rule is written once. A request is two parts. The body ([`PostBody`],
//! [`InboxBody`], [`ThreadQuery`], [`ThreadUpdate`]) is what a client sends. The [`Caller`] is
//! who sends it, and the daemon builds it from the session: a client cannot name the agent it
//! acts as, so [`Caller`] is deliberately not deserializable.
//!
//! Constraints a tool schema can only describe ("one of `thread`, `reply_to`, `subject`", "at
//! most one of `join`, `leave`, `invite`, `archive`") are enums here, and the `from_fields`
//! constructors in [`fields`](super::fields) are the single place a flat argument set is turned
//! into one. A field that does not apply is refused there, never ignored.

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use super::cursor::{Cursor, PagedRead, ThreadListing};
pub use super::error::CommsError;
pub use super::fields::{InboxCall, InboxFields, ThreadsCall, ThreadsFields};
pub use super::ids::{
    Actor, ID_SEPARATOR, IdKind, InvalidId, MAX_REPLAY_KEY_LEN, MAX_REPO_KEY_LEN, MAX_SUB_NAME_LEN, MessageId,
    PATH_REPO_PREFIX, Principal, ReplayKey, RepoKey, SubName, THREAD_ID_DIGITS, THREAD_ID_PREFIX, ThreadId,
    bounded_echo,
};
use crate::path::RelPath;

/// Longest body returned inline, in bytes. A longer one is returned cut and flagged; the whole
/// body is read by message id.
pub const MAX_INLINE_BODY_BYTES: usize = 4 * 1024;

/// Bytes one inbox reply may carry, counted per message by [`message_cost`]. The first message is
/// always returned; delivery stops at the first one after it that does not fit.
pub const RESPONSE_BUDGET_BYTES: usize = 16 * 1024;

/// Longest wait an inbox read may ask for, in seconds: under the hosts' tool-call timeout.
pub const MAX_WAIT_SECS: u32 = 40;

/// Page size when a request names none.
pub const DEFAULT_LIMIT: u32 = 20;

/// Largest page size honoured; a larger `limit` is clamped to it, and 0 means 1.
pub const MAX_LIMIT: u32 = 100;

/// How long a message is kept.
pub const MESSAGE_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How long an actor stays "seen" after its last call, at the least. A sweep removes its seen
/// record, and its marks in every thread with it, within [`SEEN_REFRESH_INTERVAL`] after that.
pub const SEEN_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How old a seen record grows before a call by its actor rewrites it, so that a read does not
/// write on every call. A record therefore trails the actor's last call by up to this much, and
/// is kept this much longer than [`SEEN_TTL`] to make up for it.
pub const SEEN_REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Messages kept per thread; a sweep drops the oldest past it.
pub const MAX_MESSAGES_PER_THREAD: u64 = 1_000;

/// Active (not archived) threads one repository holds. Creating one more archives the least
/// recently active.
pub const MAX_ACTIVE_THREADS_PER_REPO: usize = 256;

/// Most agents one `to` or one `invite` may name.
pub const MAX_RECIPIENTS: usize = 32;

/// Most members, which are principals, one thread may have. A join or an invitation that would
/// pass it is refused whole.
pub const MAX_THREAD_MEMBERS: usize = 256;

/// Most path prefixes a thread may be about.
pub const MAX_THREAD_PATHS: usize = 16;

/// Shortest discovery query, in characters, so a query cannot enumerate every subject.
pub const MIN_QUERY_CHARS: usize = 3;

/// How long a replay key keeps returning its first result, at the least: a sweep removes the
/// entry after it.
pub const REPLAY_TTL: Duration = Duration::from_secs(60 * 60);

/// Version of the comms store layout. Not tied to the release, so an upgrade keeps history.
pub const COMMS_STORE_VER: u32 = 1000;

/// Largest body a post may carry, in bytes. What is returned inline is bounded separately by
/// [`MAX_INLINE_BODY_BYTES`].
pub const MAX_POST_BODY_BYTES: usize = 16 * 1024;

/// Longest thread subject, in bytes: a subject is a one-line topic.
pub const MAX_SUBJECT_BYTES: usize = 200;

/// Who makes a request, as the daemon established it.
///
/// The principal comes from the session's `Hello`, the sub-identity from the call's `sub`, the
/// repository from the session's root, and the replay key from the relay. None of it is taken
/// from a request body, which is why this type cannot be deserialized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    /// The agent the request acts as.
    pub actor: Actor,
    /// The repository the session works in: the owner of a thread it creates, and the only
    /// repository whose open threads it can discover or join.
    pub repo: RepoKey,
    /// The relay's key for this request, when it came through a relay. A replay with the same key
    /// returns the first result.
    pub replay_key: Option<ReplayKey>,
}

impl Caller {
    /// A caller without a replay key: a CLI or hook call, or a test.
    pub fn new(actor: Actor, repo: RepoKey) -> Self {
        Self {
            actor,
            repo,
            replay_key: None,
        }
    }

    /// The same caller making the request the relay stamped with `key`.
    pub fn with_replay_key(mut self, key: ReplayKey) -> Self {
        self.replay_key = Some(key);
        self
    }
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
        /// Members beside the creator, at most [`MAX_RECIPIENTS`]. Any makes the thread
        /// invitation-only; none leaves it open to its repository.
        #[serde(default)]
        to: Vec<Actor>,
        /// Repository-relative path prefixes the thread is about, at most [`MAX_THREAD_PATHS`],
        /// fixed here.
        #[serde(default)]
        paths: Vec<RelPath>,
    },
}

/// `agent_post`: post a message, creating the thread when the target is a new one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostBody {
    /// Where the post goes.
    pub target: PostTarget,
    /// The message text.
    pub body: String,
}

/// What an unread inbox read does to the caller's positions.
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
    /// Every active thread the caller's principal is a member of, in every repository. An
    /// archived thread is read by naming it.
    #[default]
    All,
    /// One thread.
    Thread(ThreadId),
    /// One message, with its whole body. A point lookup that writes nothing, so it is allowed on a
    /// store newer than the binary.
    Message(MessageId),
}

/// `agent_inbox`: read messages, bodies inline. Never blocks: the wait of `wait_secs` belongs to
/// the daemon, which watches [`AgentOps::arrivals`] and reads again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxBody {
    /// Which messages to read.
    #[serde(default)]
    pub select: InboxSelect,
    /// Only unread messages from other agents (the default). `false` reads history, oldest first,
    /// the caller's own posts included, and moves no position.
    #[serde(default = "default_true")]
    pub unread: bool,
    /// What an unread read does to the caller's positions.
    #[serde(default)]
    pub mark: Mark,
    /// Most messages to return; [`DEFAULT_LIMIT`] when absent, clamped to [`MAX_LIMIT`].
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page. Only a read that marks nothing takes one.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

fn default_true() -> bool {
    true
}

impl Default for InboxBody {
    /// The tool's defaults: every thread, unread only, marking as read.
    fn default() -> Self {
        Self {
            select: InboxSelect::All,
            unread: true,
            mark: Mark::Read,
            limit: None,
            cursor: None,
        }
    }
}

/// What an [`InboxBody`] asks for, once its fields are checked against each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadKind {
    /// One message by id. Writes nothing.
    ById,
    /// Unread messages, advancing `read_seq` (and `notified_seq` with it) to the last one
    /// returned in each thread. Takes no cursor.
    Marking,
    /// Unread messages not announced yet, advancing only `notified_seq`. Takes no cursor.
    Announcing,
    /// Unread messages, moving nothing. Pages with a cursor.
    Peek,
    /// Every message, moving nothing. Pages with a cursor.
    History,
}

impl ReadKind {
    /// Whether the read moves a position, and so needs a writable store and is the kind a replay
    /// key protects.
    pub fn writes(self) -> bool {
        matches!(self, Self::Marking | Self::Announcing)
    }

    /// The cursor family of a read that pages with one.
    pub fn paged(self) -> Option<PagedRead> {
        match self {
            Self::Peek => Some(PagedRead::Peek),
            Self::History => Some(PagedRead::History),
            Self::ById | Self::Marking | Self::Announcing => None,
        }
    }
}

impl InboxBody {
    /// Classify the read, refusing every field combination that would leave a field ignored: a
    /// cursor on a read that marks, and `unread`, `peek`, `notify`, `limit` or `cursor` beside `id`.
    /// A field at its default value counts as left out.
    ///
    /// Every [`AgentOps::inbox`] calls this first, so a body that did not come through
    /// [`InboxCall::from_fields`] is held to the same rules.
    pub fn kind(&self) -> Result<ReadKind, CommsError> {
        if matches!(self.select, InboxSelect::Message(_)) {
            let extra = [
                ("cursor", self.cursor.is_some()),
                ("limit", given_limit(self.limit).is_some()),
                ("unread", !self.unread),
                ("peek", self.mark == Mark::Peek),
                ("notify", self.mark == Mark::Notify),
            ];
            return match extra.into_iter().find(|(_, set)| *set) {
                Some((field, _)) => Err(CommsError::not_applicable(field, "id")),
                None => Ok(ReadKind::ById),
            };
        }
        let kind = match (self.unread, self.mark) {
            (true, Mark::Read) => ReadKind::Marking,
            (true, Mark::Notify) => ReadKind::Announcing,
            (true, Mark::Peek) => ReadKind::Peek,
            (false, Mark::Read | Mark::Peek) => ReadKind::History,
            (false, Mark::Notify) => return Err(CommsError::not_applicable("unread", "notify")),
        };
        if kind.writes() && self.cursor.is_some() {
            return Err(CommsError::CursorNotApplicable);
        }
        Ok(kind)
    }

    /// The thread the read is limited to, when it is.
    pub fn thread(&self) -> Option<&ThreadId> {
        match &self.select {
            InboxSelect::Thread(thread) => Some(thread),
            InboxSelect::All | InboxSelect::Message(_) => None,
        }
    }
}

/// Which threads a listing returns.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadFilter {
    /// Case-insensitive substring of the subject, at least [`MIN_QUERY_CHARS`] characters.
    #[serde(default)]
    pub query: Option<String>,
    /// A repository-relative path; matches a thread whose `paths` contain a prefix of it or
    /// something under it. With `query`, both must match.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Include archived threads.
    #[serde(default)]
    pub archived: bool,
    /// Most threads to return; [`DEFAULT_LIMIT`] when absent, clamped to [`MAX_LIMIT`].
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

/// `agent_threads` without an update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadQuery {
    /// List threads, newest activity first. With an empty filter: the caller's own active
    /// threads, from every repository.
    List(ThreadFilter),
    /// Describe one thread: allowed to its members, and to anyone in its repository while it is
    /// open.
    Describe(ThreadId),
}

impl Default for ThreadQuery {
    /// The tool with no arguments.
    fn default() -> Self {
        Self::List(ThreadFilter::default())
    }
}

/// The one change an `agent_threads` call makes. At most one, by construction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadAction {
    /// Make the caller's principal a member, its actors reading from the head.
    Join,
    /// End the membership of the caller's principal. The last member to leave archives the
    /// thread.
    Leave,
    /// Make the principals of these agents members, their actors reading from the head as of
    /// now. At most [`MAX_RECIPIENTS`], from any repository.
    Invite(Vec<Actor>),
    /// Close the thread to posts, joins and invitations. Creator only, member or not.
    Archive,
}

/// `agent_threads` with an update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadUpdate {
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
    /// Recipients the store has not seen make a call: not yet started, long idle, or mistyped.
    #[serde(default)]
    pub pending: Vec<Actor>,
    /// Things the agent should know that are not about this request. An [`AgentOps`]
    /// implementation puts [`cap_archive_notice`] here and nothing else; the daemon and the tool
    /// layer append theirs.
    #[serde(default)]
    pub notices: Vec<String>,
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
    /// The body: cut to [`MAX_INLINE_BODY_BYTES`] unless the message was read by id, and empty
    /// in an announcing read, which returns front matter only.
    #[serde(default)]
    pub body: String,
    /// Size of the whole body, in bytes.
    #[serde(default)]
    pub body_bytes: u64,
    /// Whether `body` was cut. The message still counts as read; the rest is read by id.
    #[serde(default)]
    pub truncated: bool,
}

/// How the wait of an inbox read ended. Set by the daemon, which owns the wait.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitOutcome {
    /// The read did not wait: it asked for none, or had something to return at once. What an
    /// [`AgentOps`] implementation always reports.
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
    /// The messages, in the store-wide order they were posted.
    #[serde(default)]
    pub messages: Vec<InboxMessage>,
    /// The last message now read in each thread this read advanced. Empty unless marking as read.
    #[serde(default)]
    pub read_to: Vec<MessageId>,
    /// Whether the limit or the response budget left messages out. After a read that marks, call
    /// again; otherwise continue with `next_cursor`.
    #[serde(default)]
    pub more: bool,
    /// Set with `more` on a read that marks nothing; pass it back as `cursor` to continue.
    #[serde(default)]
    pub next_cursor: Option<Cursor>,
    /// Things the agent should know that are not about this request. Appended by the daemon and
    /// the tool layer; an [`AgentOps`] implementation leaves it empty.
    #[serde(default)]
    pub notices: Vec<String>,
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
    /// The repository it belongs to. Not always the caller's: an invitation crosses repositories.
    pub repo: RepoKey,
    /// The path prefixes it is about.
    #[serde(default)]
    pub paths: Vec<RelPath>,
    /// Whether any agent of its repository may join; `false` for an invitation-only thread.
    pub open: bool,
    /// Whether it is archived.
    #[serde(default)]
    pub archived: bool,
    /// Who created it. Any actor of the same principal may archive it.
    pub creator: Actor,
    /// Whether the caller's principal is a member.
    pub member: bool,
    /// Stored messages from other actors the caller has not read. Zero for a non-member.
    #[serde(default)]
    pub unread: u64,
    /// Sequence number of the newest message.
    pub head: u64,
    /// The newest message's timestamp, in microseconds since the unix epoch.
    pub last_activity_micros: i64,
    /// The members other than the caller's principal.
    #[serde(default)]
    pub peers: Vec<Principal>,
}

/// Reply to a thread query or update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadsReply {
    /// The threads, newest activity first; after an update, the one thread changed.
    #[serde(default)]
    pub threads: Vec<ThreadSummary>,
    /// Invited agents the store has not seen make a call: not yet started, long idle, or mistyped.
    #[serde(default)]
    pub pending: Vec<Actor>,
    /// Set when the limit left threads out; pass it back as `cursor` to continue.
    #[serde(default)]
    pub next_cursor: Option<Cursor>,
    /// Things the agent should know that are not about this request. Appended by the daemon and
    /// the tool layer; an [`AgentOps`] implementation leaves it empty.
    #[serde(default)]
    pub notices: Vec<String>,
}

/// The agent-coordination operations, independent of transport.
///
/// Synchronous and never blocking, so a caller needs no runtime: the one wait in the design, the
/// inbox's `wait_secs`, is the daemon's, built on [`AgentOps::arrivals`]. `Send + Sync`, because
/// the daemon serves every session from one instance.
///
/// Every operation takes the [`Caller`] the daemon derived. An actor counts as seen once any of
/// these returns `Ok` for it; nothing is recorded, seen included, on a store newer than the
/// binary. No operation fills `notices`, except a post with [`cap_archive_notice`].
///
/// No operation removes a record or hides one for its age: that is an implementation's sweep.
pub trait AgentOps: Send + Sync {
    /// Post a message. Requires membership, except that a [`PostTarget::NewThread`] creates the
    /// thread with the principals of the caller and of every `to` recipient as members, their
    /// actors reading from the start.
    fn post(&self, caller: &Caller, body: PostBody) -> Result<PostReply, CommsError>;

    /// Read messages from the caller's threads without waiting. What the read moves is decided by
    /// [`InboxBody::kind`].
    fn inbox(&self, caller: &Caller, body: InboxBody) -> Result<InboxReply, CommsError>;

    /// List the caller's threads, discover open threads of its repository by query or path, or
    /// describe one thread.
    fn threads_query(&self, caller: &Caller, query: ThreadQuery) -> Result<ThreadsReply, CommsError>;

    /// Join, leave, invite into or archive one thread.
    fn threads_update(&self, caller: &Caller, update: ThreadUpdate) -> Result<ThreadsReply, CommsError>;

    /// A counter that increases every time a post is stored, in any thread, and at no other time.
    /// Cheap, and all a waiter needs: when it has changed, an inbox read may return something
    /// new. A store already keeps such a counter, because an inbox across threads is sorted by
    /// the store-wide order of posts.
    fn arrivals(&self) -> u64;
}

/// A request's `limit` as given: [`DEFAULT_LIMIT`] counts as left out, like every field passed
/// with its default value, so it is not refused where a `limit` does not apply.
pub fn given_limit(limit: Option<u32>) -> Option<u32> {
    limit.filter(|limit| *limit != DEFAULT_LIMIT)
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

/// What `message` takes from [`RESPONSE_BUDGET_BYTES`]: the bytes of its body as returned, its
/// subject, its sender and its id. The subject counts every time, because every message repeats
/// it.
pub fn message_cost(message: &InboxMessage) -> usize {
    message.body.len() + message.subject.len() + message.from.as_str().len() + message.id.to_string().len()
}

/// How many of `costs`, in order, one reply carries: the first always, then each next one while
/// the running total stays within [`RESPONSE_BUDGET_BYTES`] and the count within `limit`. It
/// stops at the first message that does not fit and never skips ahead to a smaller one, so what
/// is delivered is a prefix.
pub fn deliverable(costs: impl IntoIterator<Item = usize>, limit: usize) -> usize {
    let mut spent = 0usize;
    let mut count = 0usize;
    for cost in costs {
        if count == limit.max(1) || (count > 0 && spent.saturating_add(cost) > RESPONSE_BUDGET_BYTES) {
            break;
        }
        spent = spent.saturating_add(cost);
        count += 1;
    }
    count
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

/// Refuse a `to` or an `invite` (named by `field`) over [`MAX_RECIPIENTS`].
pub fn check_recipient_count(field: &str, recipients: &[Actor]) -> Result<(), CommsError> {
    if recipients.len() > MAX_RECIPIENTS {
        return Err(CommsError::TooManyRecipients {
            field: field.to_string(),
            given: recipients.len(),
            max: MAX_RECIPIENTS,
        });
    }
    Ok(())
}

/// The actors a new thread is addressed to beside its `creator`: `to` without duplicates and
/// without the creator, in id order. Their principals become members. Refused over
/// [`MAX_RECIPIENTS`], and when `to` names only the creator, which would make an invitation-only
/// thread no other actor was told about.
pub fn new_thread_recipients(creator: &Actor, to: &[Actor]) -> Result<Vec<Actor>, CommsError> {
    check_recipient_count("to", to)?;
    let mut others: Vec<Actor> = to.iter().filter(|recipient| *recipient != creator).cloned().collect();
    others.sort();
    others.dedup();
    if others.is_empty() && !to.is_empty() {
        return Err(CommsError::OnlySelfRecipient);
    }
    Ok(others)
}

/// The paths a new thread stores: each one [`normalize_thread_path`] output, sorted, without
/// duplicates. Refused over [`MAX_THREAD_PATHS`].
pub fn normalize_thread_paths(paths: &[RelPath]) -> Result<Vec<RelPath>, CommsError> {
    if paths.len() > MAX_THREAD_PATHS {
        return Err(CommsError::TooManyPaths {
            given: paths.len(),
            max: MAX_THREAD_PATHS,
        });
    }
    let mut paths = paths.iter().map(normalize_thread_path).collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    paths.dedup();
    Ok(paths)
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
    let invalid = || CommsError::invalid_path(path);
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

/// The notice the daemon appends to an agent-tool reply while `sessions` live `serve` sessions
/// share the caller's principal. `None` when there is one session, and when the call carries
/// `as`: such a caller has already separated itself.
pub fn shared_identity_notice(caller: &Actor, sessions: usize) -> Option<String> {
    if sessions < 2 || caller.sub().is_some() {
        return None;
    }
    Some(format!(
        "{sessions} live sessions share the identity {}: they share one inbox and read position. \
         Set BASEMIND_AGENT_ID in one of them, or pass `as`, to separate them.",
        caller.principal()
    ))
}

/// The notice a creating post carries when it archived `thread` to stay within
/// [`MAX_ACTIVE_THREADS_PER_REPO`]. Only the poster is told.
pub fn cap_archive_notice(thread: &ThreadId) -> String {
    format!(
        "this repository was at its limit of {MAX_ACTIVE_THREADS_PER_REPO} active threads, so the least recently \
         active one, {thread}, was archived: it stays readable with agent_inbox `thread` and takes no more posts."
    )
}

/// The notice the tool layer appends to a session's first agent-tool reply when the id it was
/// configured with (`BASEMIND_AGENT_ID` or `comms.agent_id`) failed validation. `rejected` is
/// caller input, so only a bounded part of it is repeated.
pub fn rejected_id_notice(rejected: &str, in_use: &str) -> String {
    format!(
        "the configured agent id {:?} is not a valid id and is not used: this session acts as {in_use}. \
         Fix BASEMIND_AGENT_ID or `comms.agent_id` to choose the id.",
        bounded_echo(rejected)
    )
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
