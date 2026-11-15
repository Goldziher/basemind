//! Deleting what has expired.
//!
//! Nothing depends on this for correctness: an expired record is already invisible to every
//! read. A sweep only gives the space back. It never scans: each kind of record has an index in
//! the order it expires (`post_order` for messages, `expiry` for the rest), which the sweep
//! walks from the oldest entry until the first one still alive or until its budget is spent.
//!
//! An index row the sweep cannot decode (a `post_order` value, an `expiry` key) would sit at the
//! head of its range and stop every later sweep there. Such a row is removed and counted
//! instead, and the sweep goes on. What it named is then no longer found by a sweep. It stays
//! stored and, being expired, hidden from every read: a message until a post takes its key
//! again, a thread, replay entry or seen record until it is next written, which gives it a new
//! index row.

use fjall::{Keyspace, OwnedWriteBatch};
use serde::de::IgnoredAny;

use super::records::{Reader, backend, decode_i64};
use super::{FjallAgents, Writer, keys};
use crate::comms::v2::api::CommsError;
use crate::comms::v2::rules;

/// Records one [`FjallAgents::sweep`] removes at most. A thread counts once for itself and once
/// per member, and is removed whole, so a sweep can pass the budget by one thread's members.
pub const SWEEP_BUDGET: usize = 1024;

/// Records the sweep after a change removes at most: enough to keep up with ordinary traffic,
/// small enough not to be felt by the call that pays for it.
const CHANGE_SWEEP_BUDGET: usize = 64;

/// Least time between two sweeps run by changes, in microseconds.
const CHANGE_SWEEP_INTERVAL_MICROS: i64 = 1_000_000;

/// What one sweep removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Messages past their retention.
    pub messages: usize,
    /// Threads whose last message expired, with their member rows.
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
        let oldest_message = sweep_messages(&reader, &mut batch, &mut budget, &mut report)?;
        sweep_threads(&reader, &mut batch, &mut budget, &mut report, oldest_message)?;
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

/// Remove expired messages, oldest first. Returns the timestamp of the oldest message left in
/// the store, or `i64::MAX` when none is; `i64::MIN`, which lets no thread go, when the budget
/// ran out on a row that could not be decoded.
fn sweep_messages(
    reader: &Reader<'_>,
    batch: &mut OwnedWriteBatch,
    budget: &mut Budget,
    report: &mut SweepReport,
) -> Result<i64, CommsError> {
    let ks = reader.ks;
    for guard in reader.prefix(&ks.post_order, []) {
        let (key, value) = guard
            .into_inner()
            .map_err(|error| backend("read the comms store", error))?;
        let Some(entry) = keys::parse_order_entry(&value) else {
            if !budget.allows() {
                return Ok(i64::MIN);
            }
            skip_unreadable(batch, &ks.post_order, &key, budget, report);
            continue;
        };
        if !rules::message_expired(entry.ts_micros, reader.now) || !budget.allows() {
            return Ok(entry.ts_micros);
        }
        let message = keys::message(&entry.thread, entry.seq);
        batch.remove(&ks.messages, message);
        batch.remove(&ks.bodies, message);
        batch.remove(&ks.post_order, key);
        budget.spend(1);
        report.messages += 1;
    }
    Ok(i64::MAX)
}

/// Remove expired threads with their member rows and index entries.
///
/// A thread goes only once every message of it is gone, which `oldest_message` proves: a thread
/// is as old as its newest message, and messages are removed oldest first. That order is what
/// keeps a thread id from being drawn again while rows of its previous owner remain.
fn sweep_threads(
    reader: &Reader<'_>,
    batch: &mut OwnedWriteBatch,
    budget: &mut Budget,
    report: &mut SweepReport,
    oldest_message: i64,
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
        if !rules::message_expired(last_activity, reader.now) {
            break;
        }
        if last_activity >= oldest_message || !budget.allows() {
            budget.exhausted = true;
            break;
        }
        let record = reader
            .stored_thread(&id)?
            .filter(|record| record.last_activity_micros == last_activity);
        if let Some(record) = record {
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
