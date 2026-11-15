//! The sweep: the one place records are removed.
//!
//! No read hides a record for its age; a record is there until a sweep removes it, and a sweep
//! judges age against the clock as it reads when the sweep runs. A clock that reads more than
//! the retention ahead therefore makes a sweep remove every message and thread (ADR-0021 accepts
//! that). A sweep never scans the store: `post_order` lists messages by timestamp and `expiry`
//! lists threads, replay entries and seen records by time and the threads over their message
//! cap, and each is walked from its oldest entry to the first one that stays or until the budget
//! is spent. Each kind of record is removed in a batch of its own.
//!
//! An index row that does not decode would stop every later sweep where it sits, so it is
//! removed alone and counted; the record it named stays until its thread goes. The same holds
//! for a `post_order` row that does not name the message stored under its id, and for the
//! rows of a removed thread that do not decode. A thread whose own row does not decode is left
//! stored without its `expiry` entry: the row is what names its other index entries.

use fjall::OwnedWriteBatch;

use super::records::{MessageMeta, Reader, ThreadRecord, backend, decode};
use super::{FjallAgents, Writer, keys};
use crate::comms::v2::api::{Actor, CommsError, MAX_MESSAGES_PER_THREAD, MESSAGE_TTL, REPLAY_TTL, ThreadId};
use crate::comms::v2::rules;

/// Records one [`FjallAgents::sweep`] removes at most. A thread counts once for itself and once
/// per member, and is removed whole, so a sweep can pass the budget by one thread.
pub const SWEEP_BUDGET: usize = 1024;

/// Records the sweep after a change removes at most: enough to keep up with ordinary traffic,
/// small enough not to be felt by the call that pays for it.
const CHANGE_SWEEP_BUDGET: usize = 64;

/// Least time between two sweeps run by changes, in microseconds.
const CHANGE_SWEEP_INTERVAL_MICROS: i64 = 1_000_000;

/// What one sweep removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Messages past their retention or outside the newest of their thread, and those removed
    /// with their thread.
    pub messages: usize,
    /// Threads idle past the retention, removed with their members and positions.
    pub threads: usize,
    /// Replay entries past their hour.
    pub replays: usize,
    /// Seen records past their retention, removed with the actor's marks in every thread.
    pub seen: usize,
    /// Rows removed because they could not be decoded or named nothing stored.
    pub skipped_unreadable: usize,
    /// Whether the budget ran out with records left to remove: sweep again.
    pub more: bool,
}

/// One sweep in progress: what it may still remove and what it has.
struct Sweep<'a> {
    store: &'a FjallAgents,
    now: i64,
    left: usize,
    report: SweepReport,
}

impl FjallAgents {
    /// Remove up to [`SWEEP_BUDGET`] records retention no longer keeps. For the daemon to call
    /// periodically; refused on a store newer than the binary, where nothing is written.
    pub fn sweep(&self) -> Result<SweepReport, CommsError> {
        self.sweep_bounded(SWEEP_BUDGET)
    }

    /// [`FjallAgents::sweep`] with a budget of `budget` records.
    pub fn sweep_bounded(&self, budget: usize) -> Result<SweepReport, CommsError> {
        self.check_writable()?;
        let writer = self.lock();
        self.sweep_locked(&writer, self.clock.now_micros(), budget)
    }

    /// The small sweep a change runs after it committed, at most once per interval. The change
    /// has succeeded by then, so a failure here is logged and left for the next sweep. A clock
    /// that went back since the last sweep does not hold the next one off.
    pub(super) fn sweep_after_change(&self, writer: &mut Writer, now: i64) {
        if (0..CHANGE_SWEEP_INTERVAL_MICROS).contains(&now.saturating_sub(writer.swept_at)) {
            return;
        }
        writer.swept_at = now;
        if let Err(error) = self.sweep_locked(writer, now, CHANGE_SWEEP_BUDGET) {
            tracing::warn!(%error, "comms: sweep after a change failed");
        }
    }

    /// One sweep. `_writer` is the proof that the caller holds the lock.
    fn sweep_locked(&self, _writer: &Writer, now: i64, budget: usize) -> Result<SweepReport, CommsError> {
        let mut sweep = Sweep {
            store: self,
            now,
            left: budget,
            report: SweepReport::default(),
        };
        sweep.phase(Sweep::old_messages)?;
        sweep.phase(Sweep::over_cap)?;
        sweep.phase(Sweep::idle_threads)?;
        sweep.phase(Sweep::replays)?;
        sweep.phase(Sweep::seen)?;
        if sweep.report.skipped_unreadable > 0 {
            tracing::warn!(
                unreadable = sweep.report.skipped_unreadable,
                "comms: the sweep removed rows it could not decode or that named nothing stored"
            );
        }
        Ok(sweep.report)
    }
}

impl Sweep<'_> {
    /// Run one phase on a fresh view and commit what it staged, so the next phase sees it.
    fn phase(
        &mut self,
        stage: fn(&mut Self, &Reader<'_>, &mut OwnedWriteBatch) -> Result<(), CommsError>,
    ) -> Result<(), CommsError> {
        let store = self.store;
        let reader = Reader::new(&store.ks, store.db.snapshot());
        let mut batch = store.db.batch();
        stage(self, &reader, &mut batch)?;
        if batch.is_empty() {
            return Ok(());
        }
        batch.commit().map_err(|error| backend("sweep the comms store", error))
    }

    /// Whether another record may be removed; remembers a refusal as more to do.
    fn allows(&mut self) -> bool {
        self.report.more |= self.left == 0;
        self.left > 0
    }

    fn spend(&mut self, records: usize) {
        self.left = self.left.saturating_sub(records);
    }

    /// Count a row that was removed because it does not decode or names nothing stored.
    fn skip(&mut self) {
        self.spend(1);
        self.report.skipped_unreadable += 1;
    }

    /// Stage the removal of message `key` with its body and its `post_order` row, which is
    /// found through the message's own row when that decodes.
    fn remove_message(&mut self, reader: &Reader<'_>, batch: &mut OwnedWriteBatch, key: &[u8], row: &[u8]) {
        let ks = reader.ks;
        match decode::<MessageMeta>("a message", row) {
            Ok(meta) => {
                batch.remove(&ks.post_order, keys::order(meta.ts_micros, meta.order));
                self.spend(1);
                self.report.messages += 1;
            }
            Err(_) => self.skip(),
        }
        batch.remove(&ks.messages, key);
        batch.remove(&ks.bodies, key);
    }

    /// Messages stamped more than [`MESSAGE_TTL`] ago, oldest stamp first.
    fn old_messages(&mut self, reader: &Reader<'_>, batch: &mut OwnedWriteBatch) -> Result<(), CommsError> {
        let ks = reader.ks;
        for row in reader.rows(&ks.post_order, &[]) {
            let (key, value) = row?;
            let entry = keys::parse_order_row(&key, &value);
            if entry
                .as_ref()
                .is_some_and(|entry| !rules::older_than(entry.ts_micros, self.now, MESSAGE_TTL))
                || !self.allows()
            {
                break;
            }
            // ~keep A row is believed only when the message it names carries the row's stamp
            // and post position. Anything else would remove whatever now has that id.
            let named = match &entry {
                Some(entry) => reader.get(&ks.messages, keys::message(&entry.thread, entry.seq))?,
                None => None,
            }
            .filter(|row| {
                decode::<MessageMeta>("a message", row)
                    .is_ok_and(|meta| keys::order(meta.ts_micros, meta.order)[..] == key[..])
            });
            match (entry, named) {
                (Some(entry), Some(row)) => {
                    self.remove_message(reader, batch, &keys::message(&entry.thread, entry.seq), &row);
                }
                _ => {
                    batch.remove(&ks.post_order, key);
                    self.skip();
                }
            }
        }
        Ok(())
    }

    /// Messages outside the newest [`MAX_MESSAGES_PER_THREAD`] of a thread a post marked as
    /// over its cap. The mark goes once the thread is within it.
    fn over_cap(&mut self, reader: &Reader<'_>, batch: &mut OwnedWriteBatch) -> Result<(), CommsError> {
        let ks = reader.ks;
        'threads: for key in reader.keys(&ks.expiry, &keys::expiry_prefix(keys::Expiry::OverCap)) {
            let key = key?;
            let id = keys::parse_expiry(&key).and_then(|(_, payload)| keys::parse_thread(payload));
            if !self.allows() {
                break;
            }
            let record = match &id {
                Some(id) => reader.thread(id).ok().flatten(),
                None => None,
            };
            if let (Some(id), Some(record)) = (&id, record) {
                let newest_dropped = record.head.saturating_sub(MAX_MESSAGES_PER_THREAD);
                for row in reader.rows(&ks.messages, &keys::thread(id)) {
                    let (message, row) = row?;
                    if keys::parse_message_seq(&message).is_none_or(|seq| seq > newest_dropped) {
                        break;
                    }
                    if !self.allows() {
                        break 'threads;
                    }
                    self.remove_message(reader, batch, &message, &row);
                }
            } else {
                self.skip();
            }
            batch.remove(&ks.expiry, key);
        }
        Ok(())
    }

    /// Threads whose last activity is more than [`MESSAGE_TTL`] ago, each with whatever messages
    /// it still has, its members and their positions. A thread goes only in the batch that leaves
    /// no row of it behind, which is what lets its id be drawn again.
    fn idle_threads(&mut self, reader: &Reader<'_>, batch: &mut OwnedWriteBatch) -> Result<(), CommsError> {
        let ks = reader.ks;
        for key in reader.keys(&ks.expiry, &keys::expiry_prefix(keys::Expiry::Thread)) {
            let key = key?;
            let parsed = keys::parse_expiry(&key)
                .and_then(|(last_activity, payload)| Some((last_activity, keys::parse_thread(payload)?)));
            if parsed
                .as_ref()
                .is_some_and(|(last_activity, _)| !rules::older_than(*last_activity, self.now, MESSAGE_TTL))
                || !self.allows()
            {
                break;
            }
            batch.remove(&ks.expiry, key);
            let Some((last_activity, id)) = parsed else {
                self.skip();
                continue;
            };
            // ~keep An entry is believed only when the thread it names still carries its time:
            // anything else would remove a thread that is not idle.
            match reader
                .get(&ks.threads, keys::thread(&id))?
                .map(|row| decode::<ThreadRecord>("a thread", &row))
            {
                Some(Ok(record)) if record.last_activity_micros == last_activity => {
                    self.remove_thread(reader, batch, &id, &record)?;
                }
                _ => self.skip(),
            }
        }
        Ok(())
    }

    fn remove_thread(
        &mut self,
        reader: &Reader<'_>,
        batch: &mut OwnedWriteBatch,
        id: &ThreadId,
        record: &ThreadRecord,
    ) -> Result<(), CommsError> {
        let ks = reader.ks;
        let prefix = keys::thread(id);
        for row in reader.rows(&ks.messages, &prefix) {
            let (message, row) = row?;
            self.remove_message(reader, batch, &message, &row);
        }
        for key in reader.keys(&ks.members, &prefix) {
            let key = key?;
            if let Some(member) = keys::parse_member(&key) {
                batch.remove(&ks.member_threads, keys::thread_of(member.as_str(), id));
            }
            batch.remove(&ks.members, key);
            self.spend(1);
        }
        for key in reader.keys(&ks.positions, &prefix) {
            let key = key?;
            if let Some(actor) = keys::parse_position(&key) {
                batch.remove(&ks.actor_threads, keys::thread_of(actor.as_str(), id));
            }
            batch.remove(&ks.positions, key);
        }
        batch.remove(&ks.expiry, keys::over_cap(id));
        batch.remove(&ks.repo_threads, keys::repo_thread(&record.repo, record.archived, id));
        batch.remove(&ks.threads, prefix);
        self.spend(1);
        self.report.threads += 1;
        Ok(())
    }

    /// Replay entries written more than [`REPLAY_TTL`] ago.
    fn replays(&mut self, reader: &Reader<'_>, batch: &mut OwnedWriteBatch) -> Result<(), CommsError> {
        let ks = reader.ks;
        for key in reader.keys(&ks.expiry, &keys::expiry_prefix(keys::Expiry::Replay)) {
            let key = key?;
            let parsed = keys::parse_expiry(&key);
            if parsed.is_some_and(|(written, _)| !rules::older_than(written, self.now, REPLAY_TTL)) || !self.allows() {
                break;
            }
            match parsed {
                Some((_, replay)) => {
                    batch.remove(&ks.replays, replay);
                    self.spend(1);
                    self.report.replays += 1;
                }
                None => self.skip(),
            }
            batch.remove(&ks.expiry, key);
        }
        Ok(())
    }

    /// Seen records written more than [`rules::SEEN_RETENTION`] ago, each with the actor's marks
    /// in every thread.
    fn seen(&mut self, reader: &Reader<'_>, batch: &mut OwnedWriteBatch) -> Result<(), CommsError> {
        let ks = reader.ks;
        for key in reader.keys(&ks.expiry, &keys::expiry_prefix(keys::Expiry::Seen)) {
            let key = key?;
            let parsed = keys::parse_expiry(&key).and_then(|(written, actor)| {
                let actor = Actor::parse(std::str::from_utf8(actor).ok()?).ok()?;
                Some((written, actor))
            });
            if parsed
                .as_ref()
                .is_some_and(|(written, _)| !rules::older_than(*written, self.now, rules::SEEN_RETENTION))
                || !self.allows()
            {
                break;
            }
            batch.remove(&ks.expiry, key);
            // ~keep An entry is believed only when the record it names still carries its time:
            // anything else would remove an actor that has called since.
            let current =
                parsed.filter(|(written, actor)| reader.seen(actor).is_ok_and(|recorded| recorded == Some(*written)));
            let Some((_, actor)) = current else {
                self.skip();
                continue;
            };
            batch.remove(&ks.seen, keys::seen(&actor));
            self.spend(1);
            self.report.seen += 1;
            for mark in reader.keys(&ks.actor_threads, &keys::threads_of_prefix(actor.as_str())) {
                let mark = mark?;
                if let Some(thread) = keys::parse_trailing_thread(&mark) {
                    batch.remove(&ks.positions, keys::position(&thread, &actor));
                }
                batch.remove(&ks.actor_threads, mark);
            }
        }
        Ok(())
    }
}
