//! The refusals of agent comms v2 (ADR-0021), and how they cross the wire.
//!
//! A refusal is worded as what the caller should do next, and it travels as a map carrying a
//! stable `code`, the variant's own fields and the rendered `message`. The message is what lets
//! a binary that does not know a code still show the refusal: it decodes into
//! [`CommsError::Unknown`], so a new code does not need a protocol version of its own.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use super::ids::{InvalidId, MessageId, ThreadId, bounded_echo};
use crate::path::RelPath;

/// Wire key of the variant tag.
const CODE_KEY: &str = "code";

/// Wire key of the rendered message.
const MESSAGE_KEY: &str = "message";

/// A refusal, worded as what the caller should do next.
///
/// Serializable, so the daemon's refusal reaches a tool reply unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(remote = "Self", tag = "code", rename_all = "snake_case")]
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
    /// No message has this id: never posted, or removed by retention.
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
    /// described from here.
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
    /// The thread is archived: readable by naming it, but closed to posts, joins and invitations.
    #[error(
        "thread {thread} is archived: read it with agent_inbox `thread`, or start a new thread by posting with `subject`"
    )]
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
    InvalidPostTarget {
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
    /// A field was set without the field it depends on.
    #[error("`{needed_by}` needs `{field}`: pass `{field}` as well")]
    MissingField {
        /// The field that is missing.
        field: String,
        /// The field that needs it.
        needed_by: String,
    },
    /// A field that has no meaning beside another one. Refused, because a field that is ignored
    /// reads as a call that honoured it.
    #[error("`{field}` does not apply together with `{with}`: drop `{field}`, or make two calls")]
    FieldNotApplicable {
        /// The field that does not apply.
        field: String,
        /// The field it was combined with.
        with: String,
    },
    /// A cursor on an unread read that marks. Such a read continues from the read position.
    #[error(
        "`cursor` does not apply to an unread read that marks what it returns: it continues from your read position, so call again without `cursor`; only `peek` and `unread: false` reads page with one"
    )]
    CursorNotApplicable,
    /// A discovery query shorter than [`MIN_QUERY_CHARS`](super::api::MIN_QUERY_CHARS).
    #[error("`query` needs at least {min} characters: lengthen it, or search by `path`")]
    QueryTooShort {
        /// The minimum length, in characters.
        min: usize,
    },
    /// The thread already holds [`MAX_THREAD_MEMBERS`](super::api::MAX_THREAD_MEMBERS) members, or
    /// the invitation would take it past that.
    #[error(
        "thread {thread} cannot have more than {cap} members: invite fewer agents, ask a member that is done to leave, or start a new thread by posting with `subject`"
    )]
    MemberCapReached {
        /// The thread at its cap.
        thread: ThreadId,
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
    /// `wait_secs` above [`MAX_WAIT_SECS`](super::api::MAX_WAIT_SECS).
    #[error("`wait_secs` is {given}, above the maximum of {max}: wait at most {max} seconds and call again")]
    WaitOutOfRange {
        /// The requested wait.
        given: u32,
        /// The maximum wait.
        max: u32,
    },
    /// A cursor that is malformed, or was issued to another agent or for another request.
    #[error("`cursor` {cursor:?} is not a `next_cursor` of this request: repeat the call without `cursor`")]
    InvalidCursor {
        /// The refused cursor, cut to a bounded length.
        cursor: String,
    },
    /// A post without a body.
    #[error("`body` is empty: write the message to post")]
    EmptyBody,
    /// A body over [`MAX_POST_BODY_BYTES`](super::api::MAX_POST_BODY_BYTES).
    #[error("`body` is {len} bytes, above the maximum of {max}: shorten it, or split it across posts")]
    BodyTooLarge {
        /// The body's length in bytes.
        len: usize,
        /// The maximum length in bytes.
        max: usize,
    },
    /// A subject that is empty, multi-line or over
    /// [`MAX_SUBJECT_BYTES`](super::api::MAX_SUBJECT_BYTES).
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
        /// The refused path, cut to a bounded length.
        path: String,
    },
    /// More agents in `to` or `invite` than [`MAX_RECIPIENTS`](super::api::MAX_RECIPIENTS).
    #[error(
        "`{field}` names {given} agents, above the maximum of {max}: name fewer, and invite the rest in further calls"
    )]
    TooManyRecipients {
        /// The field over the cap: `to` or `invite`.
        field: String,
        /// How many agents it named.
        given: usize,
        /// The cap.
        max: usize,
    },
    /// More `paths` than [`MAX_THREAD_PATHS`](super::api::MAX_THREAD_PATHS).
    #[error("`paths` has {given} entries, above the maximum of {max}: name the common parent directories instead")]
    TooManyPaths {
        /// How many paths the post named.
        given: usize,
        /// The cap.
        max: usize,
    },
    /// A `to` that names nobody but the caller, which would make a thread only its creator can
    /// reach.
    #[error("`to` names only you: name another agent, or drop `to` to open the thread to the repository")]
    OnlySelfRecipient,
    /// An invitation naming nobody.
    #[error("`invite` is empty: name at least one agent id")]
    EmptyInvite,
    /// The store or the daemon failed; nothing about the request was wrong.
    #[error("the comms store failed: {detail}; retry, and run `basemind agent status` if it persists")]
    Backend {
        /// What failed.
        detail: String,
    },
    /// A refusal whose code this binary does not know: a newer daemon sent it. Carries the
    /// daemon's own wording, so the caller still reads what to do.
    #[error("{message}")]
    #[serde(skip)]
    Unknown {
        /// The code as sent.
        code: String,
        /// The message the sender rendered.
        message: String,
    },
}

impl CommsError {
    /// Refuse `cursor`, echoing at most a bounded part of it.
    pub fn invalid_cursor(cursor: &str) -> Self {
        Self::InvalidCursor {
            cursor: bounded_echo(cursor),
        }
    }

    /// Refuse `path`, echoing at most a bounded part of it.
    pub fn invalid_path(path: &RelPath) -> Self {
        Self::InvalidPath {
            path: bounded_echo(&path.to_str_lossy()),
        }
    }

    /// Refuse `field` because it has no meaning beside `with`.
    pub fn not_applicable(field: &str, with: &str) -> Self {
        Self::FieldNotApplicable {
            field: field.to_string(),
            with: with.to_string(),
        }
    }

    /// The stable wire code of this refusal: the variant name in snake case, or the code a newer
    /// sender used.
    pub fn code(&self) -> String {
        match self.to_wire() {
            Ok(Value::Object(fields)) => match fields.get(CODE_KEY) {
                Some(Value::String(code)) => code.clone(),
                _ => String::new(),
            },
            _ => String::new(),
        }
    }

    /// The wire map: the variant's tagged fields plus the rendered message.
    fn to_wire(&self) -> Result<Value, serde_json::Error> {
        let mut fields = match self {
            Self::Unknown { code, .. } => {
                let mut fields = Map::new();
                fields.insert(CODE_KEY.to_string(), Value::String(code.clone()));
                fields
            }
            known => match serde_json::to_value(Known(known))? {
                Value::Object(fields) => fields,
                _ => Map::new(),
            },
        };
        fields.insert(MESSAGE_KEY.to_string(), Value::String(self.to_string()));
        Ok(Value::Object(fields))
    }
}

/// Routes the derived, tag-only representation through `Serialize`, which the remote derive
/// leaves to the hand-written impls below.
struct Known<'a>(&'a CommsError);

impl Serialize for Known<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        CommsError::serialize(self.0, serializer)
    }
}

impl Serialize for CommsError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_wire().map_err(serde::ser::Error::custom)?.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CommsError {
    /// Decodes a known code into its variant. A code this binary does not know, or known fields it
    /// cannot read, become [`CommsError::Unknown`] with the sender's message.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let Value::Object(mut fields) = Value::deserialize(deserializer)? else {
            return Err(serde::de::Error::custom("a comms refusal must be a map"));
        };
        let message = fields.remove(MESSAGE_KEY);
        let Some(Value::String(code)) = fields.get(CODE_KEY).cloned() else {
            return Err(serde::de::Error::custom("a comms refusal must carry a string `code`"));
        };
        if let Ok(known) = CommsError::deserialize(Value::Object(fields)) {
            return Ok(known);
        }
        let message = match message {
            Some(Value::String(message)) => message,
            _ => format!(
                "the comms daemon refused the request with code `{code}`, which this basemind does not know: upgrade basemind"
            ),
        };
        Ok(Self::Unknown { code, message })
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
