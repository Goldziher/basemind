//! Values of the comms v2 store and the consistent view operations read them through.
//!
//! Records are msgpack maps (`rmp_serde::to_vec_named`), so a field can be added with
//! `#[serde(default)]` without a migration step. Index values that are a fixed tuple of numbers
//! are raw big-endian bytes, encoded in [`keys`](super::keys).

use std::fmt::Display;
use std::ops::RangeInclusive;

use fjall::{Keyspace, Readable, Snapshot, UserValue};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::Keyspaces;
use super::keys;
use crate::comms::v2::api::{Actor, Caller, CommsError, InboxMessage, MessageId, Principal, RepoKey, ThreadId};
use crate::comms::v2::rules::{self, Access, BodyForm, Position};
use crate::path::RelPath;

/// Bytes of a raw `i64` or `u64` value.
const NUMBER_LEN: usize = 8;

/// One row of `threads`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ThreadRecord {
    pub subject: String,
    pub repo: RepoKey,
    pub paths: Vec<RelPath>,
    pub open: bool,
    pub archived: bool,
    pub creator: Actor,
    /// Sequence number of the newest message.
    pub head: u64,
    /// Timestamp of the newest message: the thread's last activity, which the next post is
    /// stamped after and the sweep removes the thread by.
    pub last_activity_micros: i64,
    /// Position of the newest message in the store-wide post order: what listings sort by.
    pub last_order: u64,
}

impl ThreadRecord {
    /// The facts the rules decide on, for `caller`.
    pub fn access(&self, caller: &Caller, member: bool) -> Access {
        Access::of(caller, &self.repo, &self.creator, self.open, self.archived, member)
    }
}

/// One row of `messages`: everything about a message but its body, which is in `bodies` so that
/// counting and skipping messages never loads one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct MessageMeta {
    pub from: Actor,
    pub ts_micros: i64,
    /// Position in the store-wide post order: what an inbox across threads sorts by.
    pub order: u64,
    /// Sequence number, in the same thread, of the message this one replies to.
    #[serde(default)]
    pub reply_to: Option<u64>,
}

/// One row of `replays`: the first result of a request, and when it was stored.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Replay<T> {
    pub written_micros: i64,
    pub value: T,
}

/// A store failure, as the refusal that reaches the caller.
pub(super) fn backend(what: impl Display, error: impl Display) -> CommsError {
    CommsError::Backend {
        detail: format!("{what}: {error}"),
    }
}

/// A stored key or value that does not decode: the store holds something this binary did not
/// write.
pub(super) fn corrupt(what: &str) -> CommsError {
    CommsError::Backend {
        detail: format!("the comms store holds an unreadable {what}"),
    }
}

pub(super) fn encode<T: Serialize>(what: &str, value: &T) -> Result<Vec<u8>, CommsError> {
    rmp_serde::to_vec_named(value).map_err(|error| backend(format_args!("encode {what}"), error))
}

pub(super) fn decode<T: DeserializeOwned>(what: &str, bytes: &[u8]) -> Result<T, CommsError> {
    rmp_serde::from_slice(bytes).map_err(|error| backend(format_args!("decode {what}"), error))
}

pub(super) fn decode_i64(what: &str, bytes: &[u8]) -> Result<i64, CommsError> {
    let bytes: [u8; NUMBER_LEN] = bytes.try_into().map_err(|_| corrupt(what))?;
    Ok(i64::from_be_bytes(bytes))
}

pub(super) fn decode_u64(what: &str, bytes: &[u8]) -> Result<u64, CommsError> {
    let bytes: [u8; NUMBER_LEN] = bytes.try_into().map_err(|_| corrupt(what))?;
    Ok(u64::from_be_bytes(bytes))
}

/// One consistent view of the store. It returns what is stored: nothing is left out for its age.
pub(super) struct Reader<'a> {
    pub ks: &'a Keyspaces,
    snapshot: Snapshot,
}

impl<'a> Reader<'a> {
    pub fn new(ks: &'a Keyspaces, snapshot: Snapshot) -> Self {
        Self { ks, snapshot }
    }

    pub fn get(&self, keyspace: &Keyspace, key: impl AsRef<[u8]>) -> Result<Option<UserValue>, CommsError> {
        self.snapshot
            .get(keyspace, key)
            .map_err(|error| backend("read the comms store", error))
    }

    /// The keys of `keyspace` that start with `prefix`.
    pub fn keys(
        &self,
        keyspace: &Keyspace,
        prefix: &[u8],
    ) -> impl Iterator<Item = Result<fjall::UserKey, CommsError>> + use<> {
        self.snapshot
            .prefix(keyspace, prefix)
            .map(|guard| guard.key().map_err(|error| backend("read the comms store", error)))
    }

    /// The rows of `keyspace` whose keys start with `prefix`.
    pub fn rows(
        &self,
        keyspace: &Keyspace,
        prefix: &[u8],
    ) -> impl Iterator<Item = Result<(fjall::UserKey, UserValue), CommsError>> + use<> {
        self.snapshot.prefix(keyspace, prefix).map(|guard| {
            guard
                .into_inner()
                .map_err(|error| backend("read the comms store", error))
        })
    }

    pub fn thread(&self, id: &ThreadId) -> Result<Option<ThreadRecord>, CommsError> {
        self.get(&self.ks.threads, keys::thread(id))?
            .map(|bytes| decode("a thread", &bytes))
            .transpose()
    }

    /// Where the actors of `principal` start reading `thread`, when it is a member.
    pub fn member(&self, thread: &ThreadId, principal: &Principal) -> Result<Option<u64>, CommsError> {
        self.get(&self.ks.members, keys::member(thread, principal))?
            .map(|bytes| decode_u64("member row", &bytes))
            .transpose()
    }

    /// The marks of every actor of `principal` in `thread`.
    pub fn positions(&self, thread: &ThreadId, principal: &str) -> Result<Vec<(Actor, Position)>, CommsError> {
        let mut positions = Vec::new();
        for row in self.rows(&self.ks.positions, &keys::positions_prefix(thread, principal)) {
            let (key, value) = row?;
            let actor = keys::parse_position(&key).ok_or_else(|| corrupt("position key"))?;
            // ~keep The prefix also matches a longer principal that starts with this one.
            if actor.principal() == principal {
                positions.push((actor, decode("a position", &value)?));
            }
        }
        Ok(positions)
    }

    /// Where `actor` stands in `thread`, and whether those are marks it has stored or where it
    /// starts ([`Position::start`]). `None` when its principal is not a member.
    pub fn position(&self, thread: &ThreadId, actor: &Actor) -> Result<Option<(Position, bool)>, CommsError> {
        let Some(member_start) = self.member(thread, &actor.principal_id())? else {
            return Ok(None);
        };
        let positions = self.positions(thread, actor.principal())?;
        if let Some((_, own)) = positions.iter().find(|(sibling, _)| sibling == actor) {
            return Ok(Some((*own, true)));
        }
        let sibling_read_seq = positions.iter().map(|(_, position)| position.read_seq).max();
        Ok(Some((Position::start(sibling_read_seq, member_start), false)))
    }

    pub fn meta(&self, thread: &ThreadId, seq: u64) -> Result<Option<MessageMeta>, CommsError> {
        self.get(&self.ks.messages, keys::message(thread, seq))?
            .map(|bytes| decode("a message", &bytes))
            .transpose()
    }

    /// The messages of `thread` with a sequence number in `seqs`, oldest first.
    pub fn metas(
        &self,
        thread: &ThreadId,
        seqs: RangeInclusive<u64>,
    ) -> impl Iterator<Item = Result<(u64, MessageMeta), CommsError>> + use<> {
        let range = keys::message(thread, *seqs.start())..=keys::message(thread, *seqs.end());
        self.snapshot.range(&self.ks.messages, range).map(move |guard| {
            let (key, value) = guard
                .into_inner()
                .map_err(|error| backend("read the comms store", error))?;
            let seq = keys::parse_message_seq(&key).ok_or_else(|| corrupt("message key"))?;
            Ok((seq, decode::<MessageMeta>("a message", &value)?))
        })
    }

    /// The reply form of message `seq` of `thread`, its body in `form`.
    pub fn render(
        &self,
        thread: &ThreadId,
        record: &ThreadRecord,
        seq: u64,
        meta: &MessageMeta,
        form: BodyForm,
    ) -> Result<InboxMessage, CommsError> {
        let body = self
            .get(&self.ks.bodies, keys::message(thread, seq))?
            .ok_or_else(|| corrupt("message without a body"))?;
        let body = std::str::from_utf8(&body).map_err(|_| corrupt("message body"))?;
        let reply_to = meta
            .reply_to
            .map(|seq| MessageId::new(thread.clone(), seq))
            .transpose()?;
        Ok(rules::render(
            MessageId::new(thread.clone(), seq)?,
            &record.subject,
            &meta.from,
            meta.ts_micros,
            reply_to,
            body,
            form,
        ))
    }

    /// The members of `thread`, in id order.
    pub fn members(&self, thread: &ThreadId) -> Result<Vec<Principal>, CommsError> {
        self.keys(&self.ks.members, &keys::thread(thread))
            .map(|key| keys::parse_member(&key?).ok_or_else(|| corrupt("member key")))
            .collect()
    }

    /// The threads `principal` is a member of.
    pub fn member_threads(&self, principal: &str) -> Result<Vec<ThreadId>, CommsError> {
        self.trailing_threads(&self.ks.member_threads, keys::threads_of_prefix(principal))
    }

    /// The threads of `repo` in one state.
    pub fn repo_threads(&self, repo: &RepoKey, archived: bool) -> Result<Vec<ThreadId>, CommsError> {
        self.trailing_threads(&self.ks.repo_threads, keys::repo_threads_prefix(repo, archived))
    }

    fn trailing_threads(&self, keyspace: &Keyspace, prefix: Vec<u8>) -> Result<Vec<ThreadId>, CommsError> {
        self.keys(keyspace, &prefix)
            .map(|key| keys::parse_trailing_thread(&key?).ok_or_else(|| corrupt("thread index key")))
            .collect()
    }

    /// When `actor`'s seen record was written, when it has one.
    pub fn seen(&self, actor: &Actor) -> Result<Option<i64>, CommsError> {
        self.get(&self.ks.seen, keys::seen(actor))?
            .map(|bytes| decode_i64("seen record", &bytes))
            .transpose()
    }

    /// The first result stored under `key`, until a sweep removes it.
    pub fn replay<T: DeserializeOwned>(&self, key: Option<&[u8]>) -> Result<Option<T>, CommsError> {
        let Some(key) = key else {
            return Ok(None);
        };
        Ok(self
            .get(&self.ks.replays, key)?
            .map(|bytes| decode::<Replay<T>>("a replay entry", &bytes))
            .transpose()?
            .map(|replay| replay.value))
    }

    /// The `recipients` without a seen record, the caller aside, in id order.
    pub fn pending(&self, actor: &Actor, recipients: &[Actor]) -> Result<Vec<Actor>, CommsError> {
        let mut pending = Vec::new();
        for recipient in rules::addressed(actor, recipients) {
            if self.seen(&recipient)?.is_none() {
                pending.push(recipient);
            }
        }
        Ok(pending)
    }
}
