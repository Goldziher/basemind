//! Deleting what has expired.
//!
//! Nothing depends on this for correctness: an expired record is already invisible to every
//! read. A sweep only gives the space back. It never scans: each kind of record has an index in
//! the order it expires (`post_order` for messages, `expiry` for the rest), keyed by the time
//! expiry is judged by, which the sweep walks from the oldest entry until the first one still
//! alive or until its budget is spent.
//!
//! A message goes when its own stamp has expired. A thread expires by its last activity, which
//! no stamp of its messages is later than (see the module's "Time" section), so by then the walk
//! of `post_order` has reached every message of it that has an index row. The thread sweep
//! removes whatever messages a thread still has before it removes the thread, which are those
//! without one.
//!
//! An index row the sweep cannot decode (a `post_order` row, an `expiry` key) would sit at the
//! head of its range and stop every later sweep there. Such a row is removed and counted
//! instead, and the sweep goes on. Nothing says what such a row named, so it may have been a
//! record that is still alive. That record stays stored and is read until it expires like any
//! other, but no sweep finds it through this index again: a message goes when its thread does or
//! when the thread's cap pushes it out, and a thread, replay entry or seen record stays until it
//! is next written, which gives it a new index row.

use fjall::{Keyspace, OwnedWriteBatch, UserKey};
use serde::de::IgnoredAny;

use super::records::{MessageMeta, Reader, backend, decode, decode_i64};
use super::{FjallAgents, Writer, keys};
use crate::comms::v2::api::{CommsError, ThreadId};
use crate::comms::v2::rules;

/// Records one [`FjallAgents::sweep`] removes at most. A thread counts once for itself and once
/// per member, and is removed whole, so a sweep can pass the budget by one thread and its
/// members.
pub const SWEEP_BUDGET: usize = 1024;

/// Records the sweep after a change removes at most: enough to keep up with ordinary traffic,
/// small enough not to be felt by the call that pays for it.
const CHANGE_SWEEP_BUDGET: usize = 64;

/// Least time between two sweeps run by changes, in microseconds.
const CHANGE_SWEEP_INTERVAL_MICROS: i64 = 1_000_000;

/// What one sweep removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Messages past their retention, and those removed with their thread.
    pub messages: usize,
    /// Threads whose last activity expired, with their member rows.
    pub threads: usize,
    /// Replay entries past their hour.
    pub replays: usize,
    /// Seen records that expired ([`rules::seen_expired`]).
    pub seen: usize,
    /// Index rows that could not be decoded and were removed without the record they named.
    pub skipped_unreadable: usize,
    /// Whether the budget ran out with expired records left: sweep again.
    pub more: bool,
}

/// The budget of one sweep.
struct Budget {
    left: usize,
    exhausted: bool,
}

impl Budget {
    /// Whether another record may be removed; remembers a refusal.
    fn allows(&mut self) -> bool {
        if self.left == 0 {
            self.exhausted = true;
        }
        self.left > 0
    }

    fn spend(&mut self, records: usize) {
        self.left = self.left.saturating_sub(records);
    }
}

impl FjallAgents {
    /// Remove up to [`SWEEP_BUDGET`] expired records. For the daemon to call periodically;
    /// refused on a store newer than the binary, where nothing is written.
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

    /// One sweep, as one batch. `_writer` is the proof that the caller holds the writer lock.
    fn sweep_locked(&self, _writer: &Writer, now: i64, budget: usize) -> Result<SweepReport, CommsError> {
        let reader = self.reader(now);
        let mut batch = self.db.batch();
        let mut budget = Budget {
            left: budget,
            exhausted: false,
        };
        let mut report = SweepReport::default();
        let swept = sweep_messages(&reader, &mut batch, &mut budget, &mut report)?;
        sweep_threads(&reader, &mut batch, &mut budget, &mut report, &swept)?;
        sweep_replays(&reader, &mut batch, &mut budget, &mut report)?;
        sweep_seen(&reader, &mut batch, &mut budget, &mut report)?;
        report.more = budget.exhausted;
        batch
            .commit()
            .map_err(|error| backend("sweep the comms store", error))?;
        if report.skipped_unreadable > 0 {
            tracing::warn!(
                rows = report.skipped_unreadable,
                "comms: the sweep removed index rows it could not decode; the records they named are not swept"
            );
        }
        Ok(report)
    }
}

/// Stage the removal of an index row that does not decode, and count it against the budget.
fn skip_unreadable(
    batch: &mut OwnedWriteBatch,
    index: &Keyspace,
    key: &[u8],
    budget: &mut Budget,
    report: &mut SweepReport,
) {
    batch.remove(index, key);
    budget.spend(1);
    report.skipped_unreadable += 1;
}

/// How far one walk of `post_order` got: the key of the first row it left, or `None` when it
/// left none. Every message whose row sorts before that key is staged for removal in the batch,
/// which the snapshot the sweep reads from does not show.
struct Swept(Option<UserKey>);

impl Swept {
    /// Whether the message with this `post_order` key is already staged for removal.
    fn covers(&self, order_key: &[u8]) -> bool {
        self.0.as_ref().is_none_or(|first_left| order_key < &first_left[..])
    }
}

/// Remove messages whose own stamp has expired, oldest stamp first, and say how far that got.
fn sweep_messages(
    reader: &Reader<'_>,
    batch: &mut OwnedWriteBatch,
    budget: &mut Budget,
    report: &mut SweepReport,
) -> Result<Swept, CommsError> {
    let ks = reader.ks;
    for guard in reader.prefix(&ks.post_order, []) {
        let (key, value) = guard
            .into_inner()
            .map_err(|error| backend("read the comms store", error))?;
        let Some(entry) = keys::parse_order_row(&key, &value) else {
            if !budget.allows() {
                return Ok(Swept(Some(key)));
            }
            skip_unreadable(batch, &ks.post_order, &key, budget, report);
            continue;
        };
        if !rules::message_expired(entry.ts_micros, reader.now) || !budget.allows() {
            return Ok(Swept(Some(key)));
        }
        let message = keys::message(&entry.thread, entry.seq);
        batch.remove(&ks.messages, message);
        batch.remove(&ks.bodies, message);
        batch.remove(&ks.post_order, key);
        budget.spend(1);
        report.messages += 1;
    }
    Ok(Swept(None))
}

/// Remove expired threads with their messages, member rows and index entries.
///
/// A thread goes only in the batch that leaves no message of it behind. That is what keeps a
/// thread id from being drawn again while rows of its previous owner remain. A thread the budget
/// does not cover whole loses the messages it does cover and stays for the next sweep.
fn sweep_threads(
    reader: &Reader<'_>,
    batch: &mut OwnedWriteBatch,
    budget: &mut Budget,
    report: &mut SweepReport,
    swept: &Swept,
) -> Result<(), CommsError> {
    let ks = reader.ks;
    for guard in reader.prefix(&ks.expiry, keys::expiry_prefix(keys::Expiry::Thread)) {
        let key = guard.key().map_err(|error| backend("read the comms store", error))?;
        let parsed = keys::parse_expiry(&key).and_then(|(last_activity, payload)| {
            let id = keys::parse_thread(payload)?;
            Some((last_activity, id))
        });
        let Some((last_activity, id)) = parsed else {
            if !budget.allows() {
                break;
            }
            skip_unreadable(batch, &ks.expiry, &key, budget, report);
            continue;
        };
        if !rules::message_expired(last_activity, reader.now) || !budget.allows() {
            break;
        }
        let record = reader
            .stored_thread(&id)?
            .filter(|record| record.last_activity_micros == last_activity);
        if let Some(record) = record {
            if !sweep_thread_messages(reader, batch, budget, report, &id, swept)? {
                break;
            }
            let members = reader.members(&id)?;
            for member in &members {
                batch.remove(&ks.members, keys::member(&id, member));
                batch.remove(&ks.member_threads, keys::member_thread(member, &id));
            }
            batch.remove(&ks.repo_threads, keys::repo_thread(&record.repo, record.archived, &id));
            batch.remove(&ks.threads, keys::thread(&id));
            budget.spend(members.len());
            report.threads += 1;
        }
        batch.remove(&ks.expiry, key);
        budget.spend(1);
    }
    Ok(())
}

/// Remove the messages an expired thread still has: those the walk of `post_order` did not
/// reach. Returns whether none is left, which is false when the budget ran out first.
///
/// A message is staged already when its index row is there and sorts before where the walk
/// stopped. One whose index row is gone (see the module doc) was never reached by the walk and
/// is removed here. Reading is bounded with the budget too: a message is either staged already,
/// and a sweep stages no more than its budget, or costs the budget one.
fn sweep_thread_messages(
    reader: &Reader<'_>,
    batch: &mut OwnedWriteBatch,
    budget: &mut Budget,
    report: &mut SweepReport,
    thread: &ThreadId,
    swept: &Swept,
) -> Result<bool, CommsError> {
    let ks = reader.ks;
    for guard in reader.prefix(&ks.messages, keys::thread(thread)) {
        let (key, value) = guard
            .into_inner()
            .map_err(|error| backend("read the comms store", error))?;
        let meta: MessageMeta = decode("a message", &value)?;
        let order_key = keys::order(meta.ts_micros, meta.order);
        if swept.covers(&order_key) && reader.get(&ks.post_order, order_key)?.is_some() {
            continue;
        }
        if !budget.allows() {
            return Ok(false);
        }
        batch.remove(&ks.messages, key.clone());
        batch.remove(&ks.bodies, key);
        batch.remove(&ks.post_order, order_key);
        budget.spend(1);
        report.messages += 1;
    }
    Ok(true)
}

/// Remove replay entries that no longer answer.
fn sweep_replays(
    reader: &Reader<'_>,
    batch: &mut OwnedWriteBatch,
    budget: &mut Budget,
    report: &mut SweepReport,
) -> Result<(), CommsError> {
    let ks = reader.ks;
    for guard in reader.prefix(&ks.expiry, keys::expiry_prefix(keys::Expiry::Replay)) {
        let key = guard.key().map_err(|error| backend("read the comms store", error))?;
        let Some((expires, payload)) = keys::parse_expiry(&key) else {
            if !budget.allows() {
                break;
            }
            skip_unreadable(batch, &ks.expiry, &key, budget, report);
            continue;
        };
        if !rules::replay_expired(expires, reader.now) || !budget.allows() {
            break;
        }
        let current = reader
            .stored_replay::<IgnoredAny>(payload)?
            .is_some_and(|replay| replay.expires_micros == expires);
        if current {
            batch.remove(&ks.replays, payload);
            report.replays += 1;
        }
        batch.remove(&ks.expiry, key);
        budget.spend(1);
    }
    Ok(())
}

/// Remove the seen records of agents that made no call for the seen window.
fn sweep_seen(
    reader: &Reader<'_>,
    batch: &mut OwnedWriteBatch,
    budget: &mut Budget,
    report: &mut SweepReport,
) -> Result<(), CommsError> {
    let ks = reader.ks;
    for guard in reader.prefix(&ks.expiry, keys::expiry_prefix(keys::Expiry::Seen)) {
        let key = guard.key().map_err(|error| backend("read the comms store", error))?;
        let Some((last_call, payload)) = keys::parse_expiry(&key) else {
            if !budget.allows() {
                break;
            }
            skip_unreadable(batch, &ks.expiry, &key, budget, report);
            continue;
        };
        if !rules::seen_expired(last_call, reader.now) || !budget.allows() {
            break;
        }
        let stored = reader
            .get(&ks.seen, payload)?
            .map(|bytes| decode_i64("seen record", &bytes))
            .transpose()?;
        if stored == Some(last_call) {
            batch.remove(&ks.seen, payload);
            report.seen += 1;
        }
        batch.remove(&ks.expiry, key);
        budget.spend(1);
    }
    Ok(())
}
