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
use crate::comms::v2::api::{Actor, Caller, CommsError, InboxMessage, MessageId, RepoKey, ThreadId};
use crate::comms::v2::rules::{self, Access, Position};
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
    /// Time of the newest message: the thread expires with it.
    pub last_activity_micros: i64,
    /// Who posted the newest message.
    pub head_from: Actor,
    /// Sequence number of the newest message not posted by `head_from`, or 0. With `head` it
    /// gives, for any agent, the newest message from somebody else without reading a message.
    pub prev_other_seq: u64,
}

impl ThreadRecord {
    /// Sequence number of the newest message `actor` did not post, or 0: nothing past it can be
    /// unread to `actor`.
    pub fn newest_from_others(&self, actor: &Actor) -> u64 {
        if self.head_from == *actor {
            self.prev_other_seq
        } else {
            self.head
        }
    }

    /// The record after `actor` posts message `seq` at `ts_micros`.
    pub fn posted(mut self, actor: &Actor, seq: u64, ts_micros: i64) -> Self {
        if self.head_from != *actor {
            self.prev_other_seq = self.head;
            self.head_from = actor.clone();
        }
        self.head = seq;
        self.last_activity_micros = ts_micros;
        self
    }

    /// The facts the rules decide on, for `caller`.
    pub fn access(&self, caller: &Caller, member: bool) -> Access {
        Access {
            member,
            open: self.open,
            same_repo: self.repo == caller.repo,
            archived: self.archived,
            creator: self.creator == caller.actor,
        }
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

/// One row of `replays`: the first result of a request, and when it stops answering.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Replay<T> {
    pub expires_micros: i64,
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

/// One consistent view of the store at one instant of its clock.
///
/// Retention is applied here, on every read, by comparing a record's time with `now`: an expired
/// thread, message, replay entry or seen record is not returned whether or not a sweep has removed
/// it yet. The methods named `stored_*` are the exception and return what is physically there,
/// for the writes that must replace or remove it.
pub(super) struct Reader<'a> {
    pub ks: &'a Keyspaces,
    snapshot: Snapshot,
    /// The instant retention is judged at, in microseconds since the unix epoch.
    pub now: i64,
}

impl<'a> Reader<'a> {
    pub fn new(ks: &'a Keyspaces, snapshot: Snapshot, now: i64) -> Self {
        Self { ks, snapshot, now }
    }

    pub fn get(&self, keyspace: &Keyspace, key: impl AsRef<[u8]>) -> Result<Option<UserValue>, CommsError> {
        self.snapshot
            .get(keyspace, key)
            .map_err(|error| backend("read the comms store", error))
    }

    pub fn prefix(&self, keyspace: &Keyspace, prefix: impl AsRef<[u8]>) -> fjall::Iter {
        self.snapshot.prefix(keyspace, prefix)
    }

    pub fn stored_thread(&self, id: &ThreadId) -> Result<Option<ThreadRecord>, CommsError> {
        self.get(&self.ks.threads, keys::thread(id))?
            .map(|bytes| decode("a thread", &bytes))
            .transpose()
    }

    /// The thread `id`, unless it has expired with its newest message.
    pub fn thread(&self, id: &ThreadId) -> Result<Option<ThreadRecord>, CommsError> {
        Ok(self
            .stored_thread(id)?
            .filter(|thread| !rules::message_expired(thread.last_activity_micros, self.now)))
    }

    pub fn position(&self, thread: &ThreadId, actor: &Actor) -> Result<Option<Position>, CommsError> {
        self.get(&self.ks.members, keys::member(thread, actor))?
            .map(|bytes| decode("a member row", &bytes))
            .transpose()
    }

    pub fn stored_meta(&self, thread: &ThreadId, seq: u64) -> Result<Option<MessageMeta>, CommsError> {
        self.get(&self.ks.messages, keys::message(thread, seq))?
            .map(|bytes| decode("a message", &bytes))
            .transpose()
    }

    /// Message `seq` of `thread`, unless it has expired.
    pub fn meta(&self, thread: &ThreadId, seq: u64) -> Result<Option<MessageMeta>, CommsError> {
        Ok(self
            .stored_meta(thread, seq)?
            .filter(|meta| !rules::message_expired(meta.ts_micros, self.now)))
    }

    /// The unexpired messages of `thread` with a sequence number in `seqs`, oldest first.
    pub fn metas(
        &self,
        thread: &ThreadId,
        seqs: RangeInclusive<u64>,
    ) -> impl Iterator<Item = Result<(u64, MessageMeta), CommsError>> + use<> {
        let range = keys::message(thread, *seqs.start())..=keys::message(thread, *seqs.end());
        let now = self.now;
        self.snapshot.range(&self.ks.messages, range).filter_map(move |guard| {
            let decoded = guard
                .into_inner()
                .map_err(|error| backend("read the comms store", error))
                .and_then(|(key, value)| {
                    let seq = keys::parse_message_seq(&key).ok_or_else(|| corrupt("message key"))?;
                    Ok((seq, decode::<MessageMeta>("a message", &value)?))
                });
            match decoded {
                Ok((_, meta)) if rules::message_expired(meta.ts_micros, now) => None,
                other => Some(other),
            }
        })
    }

    /// The reply form of message `seq` of `thread`. `None` when its body is gone, which a
    /// concurrent sweep cannot cause within one view.
    pub fn render(
        &self,
        thread: &ThreadId,
        record: &ThreadRecord,
        seq: u64,
        meta: &MessageMeta,
        whole: bool,
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
            whole,
        ))
    }

    /// The members of `thread`, in id order.
    pub fn members(&self, thread: &ThreadId) -> Result<Vec<Actor>, CommsError> {
        let mut members = Vec::new();
        for guard in self.prefix(&self.ks.members, keys::thread(thread)) {
            let key = guard.key().map_err(|error| backend("read the comms store", error))?;
            members.push(keys::parse_member(&key).ok_or_else(|| corrupt("member key"))?);
        }
        members.sort();
        Ok(members)
    }

    /// The threads `actor` has a member row in, expired ones included.
    pub fn member_threads(&self, actor: &Actor) -> Result<Vec<ThreadId>, CommsError> {
        self.trailing_threads(&self.ks.member_threads, keys::member_threads_prefix(actor))
    }

    /// The threads of `repo` in one state, expired ones included.
    pub fn repo_threads(&self, repo: &RepoKey, archived: bool) -> Result<Vec<ThreadId>, CommsError> {
        self.trailing_threads(&self.ks.repo_threads, keys::repo_threads_prefix(repo, archived))
    }

    fn trailing_threads(&self, keyspace: &Keyspace, prefix: Vec<u8>) -> Result<Vec<ThreadId>, CommsError> {
        let mut threads = Vec::new();
        for guard in self.prefix(keyspace, prefix) {
            let key = guard.key().map_err(|error| backend("read the comms store", error))?;
            threads.push(keys::parse_trailing_thread(&key).ok_or_else(|| corrupt("thread index key"))?);
        }
        Ok(threads)
    }

    pub fn stored_seen(&self, actor: &Actor) -> Result<Option<i64>, CommsError> {
        self.get(&self.ks.seen, keys::seen(actor))?
            .map(|bytes| decode_i64("seen record", &bytes))
            .transpose()
    }

    /// Whether `actor` made a successful call within the seen window.
    pub fn seen(&self, actor: &Actor) -> Result<bool, CommsError> {
        Ok(self
            .stored_seen(actor)?
            .is_some_and(|last_call| !rules::seen_expired(last_call, self.now)))
    }

    pub fn stored_replay<T: DeserializeOwned>(&self, key: &[u8]) -> Result<Option<Replay<T>>, CommsError> {
        self.get(&self.ks.replays, key)?
            .map(|bytes| decode("a replay entry", &bytes))
            .transpose()
    }

    /// The first result stored under `key`, while it still answers.
    pub fn replay<T: DeserializeOwned>(&self, key: &[u8]) -> Result<Option<T>, CommsError> {
        Ok(self
            .stored_replay(key)?
            .filter(|replay: &Replay<T>| !rules::replay_expired(replay.expires_micros, self.now))
            .map(|replay| replay.value))
    }

    /// The `recipients` that have not made a call, the caller aside, in id order.
    pub fn pending(&self, actor: &Actor, recipients: &[Actor]) -> Result<Vec<Actor>, CommsError> {
        let mut pending = Vec::new();
        for recipient in rules::addressed(actor, recipients) {
            if !self.seen(&recipient)? {
                pending.push(recipient);
            }
        }
        Ok(pending)
    }
}
