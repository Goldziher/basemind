//! The Fjall-backed store of agent comms v2 (ADR-0021): [`FjallAgents`], an
//! [`AgentOps`] that keeps threads, messages and read positions on disk.
//!
//! It is held to the same [conformance suite](crate::comms::v2::conformance) as the in-memory
//! reference, and takes every decision from [`rules`](crate::comms::v2::rules); what is here is
//! where the facts are kept and how a change is written.
//!
//! ## Layout
//!
//! One Fjall database in `<comms dir>/agents.fjall`, a directory of its own beside the legacy
//! `store.fjall` (see [`retire_legacy_store`]).
//!
//! | Keyspace | Key | Value | Serves |
//! |---|---|---|---|
//! | `meta` | `store_ver`, `posts`, `last_ts` | `u32`, `u64`, `i64` | layout version; post counter (arrivals); newest timestamp issued |
//! | `threads` | `thread:u32` | `ThreadRecord` | a thread by id |
//! | `messages` | `thread:u32 ‖ seq:u64` | `MessageMeta` | a message by id; a thread's messages in order |
//! | `bodies` | `thread:u32 ‖ seq:u64` | UTF-8 body | the text of a message that is returned |
//! | `post_order` | `order:u64` | `thread:u32 ‖ seq:u64 ‖ ts:u64` | messages in the order they were posted, which is the order they expire in |
//! | `members` | `thread:u32 ‖ agent` | `Position` | membership, `read_seq`, `notified_seq`; a thread's members |
//! | `member_threads` | `u16:len ‖ agent ‖ thread:u32` | empty | the threads an agent is in |
//! | `repo_threads` | `u16:len ‖ repo ‖ state:u8 ‖ thread:u32` | empty | discovery, and the active-thread cap of a repository |
//! | `replays` | `tag:u8 ‖ u16:len ‖ agent ‖ [thread:u32] ‖ key` | `Replay<PostReply>` or `Replay<Delivered>` | the first result of a relayed request |
//! | `seen` | `agent` | `i64` last call | whether a recipient is `pending` |
//! | `expiry` | `kind:u8 ‖ ts:u64 ‖ record key` | empty | threads, replay entries and seen records in the order they expire |
//!
//! A message id is `<thread>.<seq>`, so reading one is a point lookup in `messages`. The
//! `threads` row carries the head, the last activity and the newest message not posted by the
//! head's author, which is enough to tell whether a member has anything unread without reading
//! a message.
//!
//! ## Writing
//!
//! One writer at a time: every change takes the writer lock, reads a snapshot, stages all of its
//! rows (the caller's seen record included) in one Fjall batch and commits it, so a change is
//! whole or absent. Reads that change nothing run on a snapshot without the lock and take it only
//! afterwards, to record the caller as seen. A commit reaches the operating system before the
//! call returns, which survives a crash of the process; [`FjallAgents::flush`] forces it to disk.
//!
//! ## Retention
//!
//! Expiry is decided when a record is read, by comparing its time with the clock, so it holds
//! from the next operation whether or not anything was deleted. Deleting is
//! [`FjallAgents::sweep`], which walks `post_order` and `expiry` from their oldest entry and
//! stops at the first one still alive or at its budget; operations run a small one after
//! committing.
//!
//! ## Versions
//!
//! `meta.store_ver` is written when the directory is created. An older version is brought
//! forward at open by numbered steps, each committed with its version; a newer one opens
//! read-only in effect, refusing whatever would write with
//! [`CommsError::StoreNewer`]. Nothing here ever deletes or replaces a store: one that cannot
//! be opened, has no version, or has no migration path is reported as
//! [`CommsError::Backend`] and left as it is.

use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use fjall::{Database, Keyspace, OwnedWriteBatch, PersistMode};

use self::records::{Reader, backend};
use super::api::{
    Actor, AgentOps, Caller, CommsError, InboxBody, InboxReply, PostBody, PostReply, ThreadId, ThreadQuery,
    ThreadUpdate, ThreadsReply, check_body,
};
use super::rules::{self, Position};

mod inbox;
mod keys;
mod legacy;
mod open;
mod post;
mod records;
mod retention;
mod threads;

pub use legacy::{LEGACY_STORE_DIR, retire_legacy_store};
pub use retention::{SWEEP_BUDGET, SweepReport};

/// Name of the store's directory inside the comms directory.
pub const STORE_DIR: &str = "agents.fjall";

/// `meta` key of the layout version.
const META_STORE_VER: &[u8] = b"store_ver";

/// `meta` key of the post counter.
const META_POSTS: &[u8] = b"posts";

/// `meta` key of the newest timestamp the store issued.
const META_LAST_TS: &[u8] = b"last_ts";

/// Draws after which a new thread id is given up on. Ids are 30 random bits and a store holds a
/// few thousand threads, so a second draw is already rare.
const MAX_THREAD_ID_DRAWS: usize = 64;

/// Bytes of randomness behind one thread id.
const THREAD_ID_BYTES: usize = 4;

/// Where the store takes the time from, in microseconds since the unix epoch.
pub trait Clock: Send + Sync {
    /// The current time. It may stand still or go back: the store never issues a timestamp that
    /// is not later than the previous one.
    fn now_micros(&self) -> i64;
}

/// The system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_micros(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_micros()).unwrap_or(i64::MAX))
    }
}

/// Where the store takes candidate thread ids from. The store checks each for collision and
/// draws again, so a source need not be collision-free.
pub trait ThreadIds: Send {
    /// The bits of the next candidate; only the low 30 are used.
    fn draw(&mut self) -> Result<u32, CommsError>;
}

/// Thread ids from the operating system's random source.
#[derive(Clone, Copy, Debug, Default)]
pub struct RandomThreadIds;

impl ThreadIds for RandomThreadIds {
    fn draw(&mut self) -> Result<u32, CommsError> {
        let mut bytes = [0u8; THREAD_ID_BYTES];
        getrandom::fill(&mut bytes).map_err(|error| backend("draw a thread id", error))?;
        Ok(u32::from_be_bytes(bytes))
    }
}

/// What a store is opened with beside its directory.
pub struct StoreOptions {
    /// The clock retention and timestamps follow.
    pub clock: Arc<dyn Clock>,
    /// The source of new thread ids.
    pub thread_ids: Box<dyn ThreadIds>,
}

impl Default for StoreOptions {
    /// The system clock and random thread ids.
    fn default() -> Self {
        Self {
            clock: Arc::new(SystemClock),
            thread_ids: Box::new(RandomThreadIds),
        }
    }
}

/// A handle to every keyspace of the store.
pub(super) struct Keyspaces {
    meta: Keyspace,
    threads: Keyspace,
    messages: Keyspace,
    bodies: Keyspace,
    post_order: Keyspace,
    members: Keyspace,
    member_threads: Keyspace,
    repo_threads: Keyspace,
    replays: Keyspace,
    seen: Keyspace,
    expiry: Keyspace,
}

/// What only the one writer touches.
struct Writer {
    /// Posts stored so far: the store-wide post order.
    posts: u64,
    /// The newest timestamp issued.
    last_ts: i64,
    thread_ids: Box<dyn ThreadIds>,
    /// When an operation last ran its sweep.
    swept_at: i64,
}

/// Agent coordination with ADR-0021 semantics, kept in a Fjall database.
pub struct FjallAgents {
    db: Database,
    ks: Keyspaces,
    clock: Arc<dyn Clock>,
    writer: Mutex<Writer>,
    /// Mirror of the persisted post counter, for [`AgentOps::arrivals`].
    arrivals: AtomicU64,
    /// Mirror of the newest timestamp issued, for reads that do not take the writer lock.
    last_ts: AtomicI64,
    store_ver: AtomicU32,
}

/// One change being staged: the batch it commits, and the instant it happens at.
struct Txn<'a> {
    writer: &'a mut Writer,
    batch: OwnedWriteBatch,
    /// The instant the change is judged at. A post moves it to the message's timestamp.
    now: i64,
    posted: bool,
}

impl Txn<'_> {
    /// Take the next timestamp and post position, and stage both counters. A timestamp is
    /// always later than the one before it, so post order and time order agree.
    fn stamp_post(&mut self, ks: &Keyspaces) -> (i64, u64) {
        let ts_micros = self.now.max(self.writer.last_ts.saturating_add(1));
        let order = self.writer.posts + 1;
        self.batch.insert(&ks.meta, META_POSTS, order.to_be_bytes());
        self.batch.insert(&ks.meta, META_LAST_TS, ts_micros.to_be_bytes());
        self.now = ts_micros;
        self.posted = true;
        (ts_micros, order)
    }

    /// A thread id the store does not hold.
    fn fresh_thread_id(&mut self, reader: &Reader<'_>) -> Result<ThreadId, CommsError> {
        for _ in 0..MAX_THREAD_ID_DRAWS {
            let id = ThreadId::from_bits(self.writer.thread_ids.draw()? % ThreadId::SPACE);
            if reader.stored_thread(&id)?.is_none() {
                return Ok(id);
            }
        }
        Err(CommsError::Backend {
            detail: format!("no free thread id in {MAX_THREAD_ID_DRAWS} draws"),
        })
    }

    /// Stage `actor`'s membership of `thread` at `position`.
    fn add_member(
        &mut self,
        ks: &Keyspaces,
        thread: &ThreadId,
        actor: &Actor,
        position: Position,
    ) -> Result<(), CommsError> {
        self.set_position(ks, thread, actor, position)?;
        self.batch
            .insert(&ks.member_threads, keys::member_thread(actor, thread), []);
        Ok(())
    }

    fn set_position(
        &mut self,
        ks: &Keyspaces,
        thread: &ThreadId,
        actor: &Actor,
        position: Position,
    ) -> Result<(), CommsError> {
        let row = records::encode("a member row", &position)?;
        self.batch.insert(&ks.members, keys::member(thread, actor), row);
        Ok(())
    }

    /// Stage a replay entry for `key`, replacing an expired one.
    fn remember<T: serde::Serialize>(&mut self, reader: &Reader<'_>, key: Vec<u8>, value: T) -> Result<(), CommsError> {
        if let Some(expired) = reader.stored_replay::<serde::de::IgnoredAny>(&key)? {
            self.batch.remove(
                &reader.ks.expiry,
                keys::expiry(keys::Expiry::Replay, expired.expires_micros, &key),
            );
        }
        let entry = records::Replay {
            expires_micros: rules::replay_expiry(self.now),
            value,
        };
        self.batch.insert(
            &reader.ks.expiry,
            keys::expiry(keys::Expiry::Replay, entry.expires_micros, &key),
            [],
        );
        self.batch
            .insert(&reader.ks.replays, key, records::encode("a replay entry", &entry)?);
        Ok(())
    }

    /// Stage `actor`'s seen record at this change's instant.
    fn touch_seen(&mut self, reader: &Reader<'_>, actor: &Actor) -> Result<(), CommsError> {
        let key = keys::seen(actor);
        let previous = reader.stored_seen(actor)?;
        if previous == Some(self.now) {
            return Ok(());
        }
        if let Some(previous) = previous {
            self.batch
                .remove(&reader.ks.expiry, keys::expiry(keys::Expiry::Seen, previous, key));
        }
        self.batch
            .insert(&reader.ks.expiry, keys::expiry(keys::Expiry::Seen, self.now, key), []);
        self.batch.insert(&reader.ks.seen, key, self.now.to_be_bytes());
        Ok(())
    }
}

impl FjallAgents {
    /// Open the store in `comms_dir`, creating it at the current version when the directory is
    /// new, with the system clock and random thread ids.
    pub fn open(comms_dir: &Path) -> Result<Self, CommsError> {
        Self::open_with(comms_dir, StoreOptions::default())
    }

    /// The layout version found on disk. Above
    /// [`COMMS_STORE_VER`](crate::comms::v2::api::COMMS_STORE_VER), the store refuses writes.
    pub fn store_version(&self) -> u32 {
        self.store_ver.load(Ordering::Acquire)
    }

    /// Force every committed change to disk. A commit is otherwise only handed to the operating
    /// system, which loses nothing when the process dies but can when the machine does.
    pub fn flush(&self) -> Result<(), CommsError> {
        self.db
            .persist(PersistMode::SyncAll)
            .map_err(|error| backend("flush the comms store", error))
    }

    /// A panic in another session's call must not take the store down with a poison error: the
    /// writer state is only changed after a commit, so it is consistent whenever the lock is free.
    fn lock(&self) -> MutexGuard<'_, Writer> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn check_writable(&self) -> Result<(), CommsError> {
        rules::check_writable(self.store_version())
    }

    fn reader(&self, now: i64) -> Reader<'_> {
        Reader::new(&self.ks, self.db.snapshot(), now)
    }

    /// Run a change under the writer lock and commit it with the caller's seen record. `change`
    /// reads from a snapshot taken under the lock, so it sees every earlier change and no later
    /// one; when it fails nothing is written.
    fn write<T>(
        &self,
        caller: &Caller,
        change: impl FnOnce(&mut Txn<'_>, &Reader<'_>) -> Result<T, CommsError>,
    ) -> Result<T, CommsError> {
        let mut writer = self.lock();
        let now = self.clock.now_micros().max(writer.last_ts);
        let reader = self.reader(now);
        let mut txn = Txn {
            writer: &mut writer,
            batch: self.db.batch(),
            now,
            posted: false,
        };
        let value = change(&mut txn, &reader)?;
        txn.touch_seen(&reader, &caller.actor)?;
        let Txn { batch, now, posted, .. } = txn;
        batch
            .commit()
            .map_err(|error| backend("write to the comms store", error))?;
        if posted {
            writer.posts += 1;
            writer.last_ts = now;
            self.last_ts.store(now, Ordering::Release);
            self.arrivals.store(writer.posts, Ordering::Release);
        }
        drop(reader);
        self.sweep_after_change(&mut writer, now);
        Ok(value)
    }

    /// Run a read that changes nothing on a snapshot, then record the caller as seen. On a store
    /// newer than the binary nothing is recorded. The read has succeeded by then, so a failure to
    /// record is logged and the reply still returned.
    fn read<T>(
        &self,
        caller: &Caller,
        read: impl FnOnce(&Reader<'_>) -> Result<T, CommsError>,
    ) -> Result<T, CommsError> {
        let now = self.clock.now_micros().max(self.last_ts.load(Ordering::Acquire));
        let value = read(&self.reader(now))?;
        if self.check_writable().is_ok()
            && let Err(error) = self.write(caller, |_, _| Ok(()))
        {
            tracing::warn!(%error, agent = %caller.actor, "comms: recording a caller as seen failed");
        }
        Ok(value)
    }
}

impl AgentOps for FjallAgents {
    fn post(&self, caller: &Caller, body: PostBody) -> Result<PostReply, CommsError> {
        check_body(&body.body)?;
        self.check_writable()?;
        self.write(caller, |txn, reader| post::post(txn, reader, caller, body))
    }

    fn inbox(&self, caller: &Caller, body: InboxBody) -> Result<InboxReply, CommsError> {
        let kind = body.kind()?;
        if !kind.writes() {
            return self.read(caller, |reader| inbox::read(reader, caller, &body, kind));
        }
        self.check_writable()?;
        self.write(caller, |txn, reader| {
            inbox::read_and_mark(txn, reader, caller, &body, kind)
        })
    }

    fn threads_query(&self, caller: &Caller, query: ThreadQuery) -> Result<ThreadsReply, CommsError> {
        self.read(caller, |reader| match &query {
            ThreadQuery::Describe(id) => threads::describe(reader, caller, id),
            ThreadQuery::List(filter) => threads::list(reader, caller, filter),
        })
    }

    fn threads_update(&self, caller: &Caller, update: ThreadUpdate) -> Result<ThreadsReply, CommsError> {
        self.check_writable()?;
        self.write(caller, |txn, reader| threads::update(txn, reader, caller, &update))
    }

    fn arrivals(&self) -> u64 {
        self.arrivals.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests;
