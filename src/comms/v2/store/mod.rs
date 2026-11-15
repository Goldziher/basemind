//! The Fjall-backed store of agent comms v2 (ADR-0021): [`FjallAgents`], an
//! [`AgentOps`] that keeps threads, messages and read positions on disk.
//!
//! It is held to the same [conformance suite](crate::comms::v2::conformance) as the in-memory
//! reference, and takes every decision from [`rules`](crate::comms::v2::rules); what is here is
//! where the facts are kept and how a change is written.
//!
//! ## Layout
//!
//! One Fjall database in `<dir>/agents.fjall`, `<dir>` being the directory the caller passes:
//! the protocol generation's (ADR-0024).
//!
//! | Keyspace | Key | Value | Serves |
//! |---|---|---|---|
//! | `meta` | `store_ver`, `posts` | `u32`, `u64` | layout version; post counter (arrivals, store-wide post order) |
//! | `threads` | `thread:u32` | `ThreadRecord` | a thread by id |
//! | `messages` | `thread:u32 ‖ seq:u64` | `MessageMeta` | a message by id; a thread's messages in order |
//! | `bodies` | `thread:u32 ‖ seq:u64` | UTF-8 body | the text of a message that is returned |
//! | `post_order` | `ts:u64 ‖ order:u64` | `thread:u32 ‖ seq:u64` | messages by timestamp, for the sweep |
//! | `members` | `thread:u32 ‖ principal` | `u64` | membership, and where the member's actors start reading |
//! | `member_threads` | `u16:len ‖ principal ‖ thread:u32` | empty | the threads a principal is in |
//! | `positions` | `thread:u32 ‖ actor` | `Position` | an actor's `read_seq` and `notified_seq` in a thread |
//! | `actor_threads` | `u16:len ‖ actor ‖ thread:u32` | empty | the threads an actor has marks in |
//! | `repo_threads` | `u16:len ‖ repo ‖ state:u8 ‖ thread:u32` | empty | discovery, and the active threads of a repository |
//! | `replays` | `tag:u8 ‖ u16:len ‖ actor ‖ [thread:u32] ‖ key` | `Replay<_>` | the first result of a relayed request |
//! | `seen` | `actor` | `i64`: when the record was written | whether a recipient is `pending` |
//! | `expiry` | `kind:u8 ‖ ts:u64 ‖ record key` | empty | what the sweep visits: threads, replay entries and seen records by time, and threads over their message cap |
//!
//! A thread id is drawn at random and drawn again while it is taken, 64 times at most
//! (`MAX_THREAD_ID_DRAWS`), after which the post fails as a [`CommsError::Backend`] and stores
//! nothing.
//!
//! ## Reading and writing
//!
//! One call at a time: every operation, a read included, takes the one lock, reads a snapshot,
//! stages its rows (the caller's seen record included, when it is due) in one Fjall batch and
//! commits it, so a change is whole or absent. An operation that stages nothing commits nothing.
//!
//! A commit is handed to the operating system before the call returns and is not synced: it
//! survives a crash of the process, not a loss of power. [`FjallAgents::flush`] syncs, and is for
//! the daemon to call on its periodic tick and at shutdown. What was committed after the last
//! flush can be lost with the machine, messages included, and a sequence number lost that way is
//! issued again to the next post in its thread.
//!
//! ## Time and retention
//!
//! A message is stamped [`rules::post_timestamp`]: the clock, or one microsecond past the newest
//! message of its thread. Timestamps are shown and read by the sweep; order within a thread is
//! `seq` and across threads the post counter. Nothing but [`FjallAgents::sweep`] removes a
//! record, and no read hides one for its age (see the `retention` module).
//!
//! ## Versions
//!
//! `meta.store_ver` is written when the directory is created. An older version is brought
//! forward at open by numbered steps, each committed with its version; a newer one opens
//! read-only in effect, refusing whatever would write with
//! [`CommsError::StoreNewer`]. Nothing here ever deletes or replaces a store: one that cannot
//! be opened, has rows but no version, lacks a keyspace, or has no migration path is reported
//! as [`CommsError::Backend`] and left as it is. See [`FjallAgents::open_with`] for what opening
//! may still do to the files of a store it does not write to.

use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use fjall::{Database, Keyspace, OwnedWriteBatch, PersistMode};

use self::records::{Reader, backend};
use super::api::{
    Actor, AgentOps, Caller, CommsError, InboxBody, InboxReply, PostBody, PostReply, Principal, ThreadId, ThreadQuery,
    ThreadUpdate, ThreadsReply, check_body,
};
use super::rules::{self, Position};

mod inbox;
mod keys;
mod open;
mod post;
mod records;
mod retention;
mod threads;

pub use retention::{SWEEP_BUDGET, SweepReport};

/// Name of the store's directory inside the directory it is opened in.
pub const STORE_DIR: &str = "agents.fjall";

/// `meta` key of the layout version.
const META_STORE_VER: &[u8] = b"store_ver";

/// `meta` key of the post counter.
const META_POSTS: &[u8] = b"posts";

/// Draws after which a new thread id is given up on. Ids are 30 random bits and a store holds a
/// few thousand threads, so a second draw is already rare.
const MAX_THREAD_ID_DRAWS: usize = 64;

/// Bytes of randomness behind one thread id.
const THREAD_ID_BYTES: usize = 4;

/// Where the store takes the time from, in microseconds since the unix epoch.
pub trait Clock: Send + Sync {
    /// The current time. It may stand still or go back: a post is stamped as
    /// [`rules::post_timestamp`] says, and a sweep judges age against this reading alone.
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
    /// The clock timestamps and the sweep follow.
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
    positions: Keyspace,
    actor_threads: Keyspace,
    repo_threads: Keyspace,
    replays: Keyspace,
    seen: Keyspace,
    expiry: Keyspace,
}

/// What only the call holding the lock touches.
struct Writer {
    /// Posts stored so far: the store-wide post order.
    posts: u64,
    thread_ids: Box<dyn ThreadIds>,
    /// When a change last ran its sweep.
    swept_at: i64,
}

/// Agent coordination with ADR-0021 semantics, kept in a Fjall database.
pub struct FjallAgents {
    db: Database,
    ks: Keyspaces,
    clock: Arc<dyn Clock>,
    writer: Mutex<Writer>,
    /// Mirror of the persisted post counter, so [`AgentOps::arrivals`] takes no lock.
    arrivals: AtomicU64,
    store_ver: AtomicU32,
}

/// One operation's change being staged: the batch it commits, and the instant it happens at.
struct Txn<'a> {
    writer: &'a mut Writer,
    batch: OwnedWriteBatch,
    /// What the clock read when the operation began.
    now: i64,
    /// Whether the change stores a post, which takes the next place in the post order.
    posted: bool,
}

impl Txn<'_> {
    /// Take the timestamp and the post position of a post into a thread whose newest message is
    /// stamped `thread_last_micros`, and stage the counter.
    fn stamp_post(&mut self, ks: &Keyspaces, thread_last_micros: Option<i64>) -> (i64, u64) {
        let order = self.writer.posts + 1;
        self.batch.insert(&ks.meta, META_POSTS, order.to_be_bytes());
        self.posted = true;
        (rules::post_timestamp(self.now, thread_last_micros), order)
    }

    /// A thread id the store does not hold.
    fn fresh_thread_id(&mut self, reader: &Reader<'_>) -> Result<ThreadId, CommsError> {
        for _ in 0..MAX_THREAD_ID_DRAWS {
            let id = ThreadId::from_bits(self.writer.thread_ids.draw()? % ThreadId::SPACE);
            if reader.thread(&id)?.is_none() {
                return Ok(id);
            }
        }
        Err(CommsError::Backend {
            detail: format!("no free thread id in {MAX_THREAD_ID_DRAWS} draws"),
        })
    }

    /// Stage `principal`'s membership of `thread`, its actors starting after message `start`.
    fn add_member(&mut self, ks: &Keyspaces, thread: &ThreadId, principal: &Principal, start: u64) {
        self.batch
            .insert(&ks.members, keys::member(thread, principal), start.to_be_bytes());
        self.batch
            .insert(&ks.member_threads, keys::thread_of(principal.as_str(), thread), []);
    }

    /// Stage `actor`'s marks in `thread`.
    fn set_position(
        &mut self,
        ks: &Keyspaces,
        thread: &ThreadId,
        actor: &Actor,
        position: Position,
    ) -> Result<(), CommsError> {
        let row = records::encode("a position", &position)?;
        self.batch.insert(&ks.positions, keys::position(thread, actor), row);
        self.batch
            .insert(&ks.actor_threads, keys::thread_of(actor.as_str(), thread), []);
        Ok(())
    }

    /// Stage the archiving of a thread: its row, and its place in the repository's index, which
    /// keeps active and archived threads apart.
    fn archive(&mut self, ks: &Keyspaces, id: &ThreadId, record: &mut records::ThreadRecord) -> Result<(), CommsError> {
        record.archived = true;
        self.batch
            .insert(&ks.threads, keys::thread(id), records::encode("a thread", record)?);
        self.batch
            .remove(&ks.repo_threads, keys::repo_thread(&record.repo, false, id));
        self.batch
            .insert(&ks.repo_threads, keys::repo_thread(&record.repo, true, id), []);
        Ok(())
    }

    /// Stage the first result of a relayed request under `key`. The first write for a key wins:
    /// callers look the key up before they do the work.
    fn remember<T: serde::Serialize>(&mut self, ks: &Keyspaces, key: Vec<u8>, value: T) -> Result<(), CommsError> {
        let entry = records::Replay {
            written_micros: self.now,
            value,
        };
        self.batch
            .insert(&ks.expiry, keys::expiry(keys::Expiry::Replay, self.now, &key), []);
        self.batch
            .insert(&ks.replays, key, records::encode("a replay entry", &entry)?);
        Ok(())
    }

    /// Stage `actor`'s seen record at this operation's instant when it is due
    /// ([`rules::seen_due`]).
    fn touch_seen(&mut self, reader: &Reader<'_>, actor: &Actor) -> Result<(), CommsError> {
        let key = keys::seen(actor);
        let previous = reader.seen(actor)?;
        if !rules::seen_due(previous, self.now) {
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
    /// Open the store in `dir`, creating it at the current version when the directory is new,
    /// with the system clock and random thread ids.
    pub fn open(dir: &Path) -> Result<Self, CommsError> {
        Self::open_with(dir, StoreOptions::default())
    }

    /// The layout version found on disk. Above
    /// [`COMMS_STORE_VER`](crate::comms::v2::api::COMMS_STORE_VER), the store refuses writes.
    pub fn store_version(&self) -> u32 {
        self.store_ver.load(Ordering::Acquire)
    }

    /// Force every committed change to disk. A commit is otherwise only handed to the operating
    /// system, which loses nothing when the process dies but can when the machine does. For the
    /// daemon to call on its periodic tick and at shutdown.
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

    /// Run one operation under the lock and commit what it staged, with the caller's seen record
    /// when that is due. `operation` reads from a snapshot taken under the lock, so it sees
    /// every earlier call and no later one; when it fails nothing is written. An operation that
    /// `writes` is refused on a store newer than the binary, where nothing is recorded for any
    /// call, seen included.
    fn call<T>(
        &self,
        caller: &Caller,
        writes: bool,
        operation: impl FnOnce(&mut Txn<'_>, &Reader<'_>) -> Result<T, CommsError>,
    ) -> Result<T, CommsError> {
        let mut writer = self.lock();
        let writable = self.check_writable();
        if writes {
            writable.clone()?;
        }
        let now = self.clock.now_micros();
        let reader = Reader::new(&self.ks, self.db.snapshot());
        let mut txn = Txn {
            writer: &mut writer,
            batch: self.db.batch(),
            now,
            posted: false,
        };
        let value = operation(&mut txn, &reader)?;
        if writable.is_ok() {
            txn.touch_seen(&reader, &caller.actor)?;
        }
        let Txn { batch, posted, .. } = txn;
        drop(reader);
        if batch.is_empty() {
            return Ok(value);
        }
        batch
            .commit()
            .map_err(|error| backend("write to the comms store", error))?;
        if posted {
            writer.posts += 1;
            self.arrivals.store(writer.posts, Ordering::Release);
        }
        self.sweep_after_change(&mut writer, now);
        Ok(value)
    }
}

impl AgentOps for FjallAgents {
    fn post(&self, caller: &Caller, body: PostBody) -> Result<PostReply, CommsError> {
        check_body(&body.body)?;
        self.call(caller, true, |txn, reader| post::post(txn, reader, caller, body))
    }

    fn inbox(&self, caller: &Caller, body: InboxBody) -> Result<InboxReply, CommsError> {
        let kind = body.kind()?;
        self.call(caller, kind.writes(), |txn, reader| {
            inbox::read(txn, reader, caller, &body, kind)
        })
    }

    fn threads_query(&self, caller: &Caller, query: ThreadQuery) -> Result<ThreadsReply, CommsError> {
        self.call(caller, false, |_, reader| match &query {
            ThreadQuery::Describe(id) => threads::describe(reader, caller, id),
            ThreadQuery::List(filter) => threads::list(reader, caller, filter),
        })
    }

    fn threads_update(&self, caller: &Caller, update: ThreadUpdate) -> Result<ThreadsReply, CommsError> {
        self.call(caller, true, |txn, reader| {
            threads::update(txn, reader, caller, &update)
        })
    }

    fn arrivals(&self) -> u64 {
        self.arrivals.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests;
