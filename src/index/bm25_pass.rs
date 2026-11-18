//! A full pass's BM25 upkeep from what the pass changed, not from a walk of the posting list.
//!
//! [`IndexDb::sweep_bm25_postings`] reads every `code_bm25_postings` key (one per `(term, chunk)`
//! pair, ~8.6 M on a 4 000-file corpus) and [`IndexDb::recompute_bm25_stats`] every forward row of
//! the view. Both pull the keyspace through the block cache, which a group database opens at its
//! 256 MiB ceiling, so running them on every full pass filled the cache and took a minute in debug
//! builds, whatever had changed.
//!
//! While a pass is open ([`IndexDb::begin_bm25_pass`]), each committed batch hands the view's
//! ledger the postings its deleted forward rows named and the change it makes to `(N, total_len)`.
//! [`Bm25Pass::finish`] retires exactly those postings, under the same group gate and liveness
//! test as the sweep, and stamps the stats as the stored ones plus the delta.
//!
//! Two facts decide whether that is sound, and they have different scopes:
//!
//! - **The view's stats are exact** (per view). A `meta` marker says no forward row of this view
//!   changed outside a completed pass since the stats were written: a write outside a pass drops it
//!   in its own batch, and a pass drops it at its start and stamps it only once finished. A view
//!   with no forward row at all is exact at zero, which makes a new worktree's first scan cheap.
//! - **The group's posting list holds no garbage** (per group). A group `meta` row records that a
//!   sweep is owed. It is set in the same batch as any forward-row delete made outside a pass, by
//!   [`IndexDb::mark_bm25_sweep_owed`] for deletes made without the writer, and by a pass at its
//!   start (cleared at its finish), so a pass that dies leaves it set. Any view's next full pass
//!   that finds it sweeps, then clears it unless someone set it again meanwhile.
//!
//! A ledger that saw a staging, decode or commit error is poisoned; one whose retired keys outgrow
//! [`RETIRED_KEYS_CEILING_BYTES`] overflows. Either way the pass falls back to the sweep and the
//! recompute, which are exact whatever the failed batch did.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ahash::AHashMap;

use super::bm25_live::Bm25Candidates;
use super::keys::{self, Bm25Key};
use super::{IndexDb, IndexError, META_BM25_DOC_COUNT, META_BM25_TOTAL_LEN};

/// `meta` marker: the posting list holds no posting this view orphaned and the stored BM25 stats
/// are exact. Present only between a completed pass and the next forward-row write outside one.
const META_BM25_EXACT: &[u8] = b"code_bm25_exact";

/// Group `meta`, outside every view's prefix: a token present while the shared posting list may
/// hold postings no view names.
pub(crate) const META_BM25_SWEEP_OWED: &[u8] = b"\xffgroup/bm25_sweep_owed";

/// A token no earlier one in this process equals and no other process plausibly picks, so a row
/// holding it was set by this call.
pub(crate) fn fresh_token() -> u64 {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    let mixed = nanos ^ u64::from(std::process::id()).rotate_left(40);
    mixed.wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed)).max(1)
}

/// Bytes of retired posting keys one pass may hold for an exact retirement. A pass that replaces
/// more than this (a mass edit, a branch switch) forgets them and sweeps instead, so the ledger
/// never becomes a structure sized by the corpus.
pub(crate) const RETIRED_KEYS_CEILING_BYTES: u64 = 16 * 1024 * 1024;

/// Fixed cost of one retired posting key held in the ledger, beyond its bytes: the `Vec` header.
const RETIRED_KEY_OVERHEAD: u64 = 24;

/// What one writer's batch did to this view's BM25 forward rows. Applied to the ledger only once
/// the batch has committed.
#[derive(Default)]
pub(crate) struct Bm25Delta {
    docs: i64,
    total_len: i64,
    retired: AHashMap<Bm25Key, Vec<Vec<u8>>>,
    retired_bytes: u64,
    /// The writer saw the ledger already over its ceiling, so it stopped collecting keys.
    overflowed: bool,
    /// A row did not decode, so the tally cannot be trusted.
    poisoned: bool,
}

impl Bm25Delta {
    pub(crate) fn is_empty(&self) -> bool {
        self.docs == 0 && self.total_len == 0 && self.retired.is_empty() && !self.overflowed && !self.poisoned
    }

    /// Fold `other`, built for one file's forward rows, in once all of them were read and staged.
    pub(crate) fn merge(&mut self, other: Self) {
        self.docs += other.docs;
        self.total_len += other.total_len;
        self.retired_bytes += other.retired_bytes;
        self.overflowed |= other.overflowed;
        self.poisoned |= other.poisoned;
        for (content, postings) in other.retired {
            self.retired.entry(content).or_default().extend(postings);
        }
    }

    /// A forward row the batch deletes: its chunk leaves `N`, and its postings may now be dead. A
    /// row that does not decode still leaves `N`, uncounted, so it poisons the pass.
    pub(crate) fn remove_row(&mut self, ledger: &Bm25PassLedger, key: &[u8], value: &[u8]) {
        let (Some((_, content, ordinal)), Some((doclen, _, terms))) = (
            keys::parse_code_bm25_by_path(key),
            keys::parse_code_bm25_by_path_value(value),
        ) else {
            self.poisoned = true;
            return;
        };
        self.docs -= 1;
        self.total_len -= i64::from(doclen);
        if self.overflowed || ledger.overflowed() {
            self.overflowed = true;
            return;
        }
        let Ok(terms) = rmp_serde::from_slice::<Vec<String>>(terms) else {
            self.poisoned = true;
            return;
        };
        let postings = self.retired.entry(content).or_default();
        for term in terms {
            if let Some(posting) = keys::code_bm25_posting(&term, &content, ordinal) {
                self.retired_bytes += posting.len() as u64 + RETIRED_KEY_OVERHEAD;
                postings.push(posting);
            }
        }
    }

    /// A forward row the batch inserts.
    pub(crate) fn add_row(&mut self, doclen: u32) {
        self.docs += 1;
        self.total_len += i64::from(doclen);
    }
}

#[derive(Default)]
struct PassState {
    docs: i64,
    total_len: i64,
    retired: AHashMap<Bm25Key, Vec<Vec<u8>>>,
    retired_bytes: u64,
    overflowed: bool,
    /// A staging, decode or commit error happened while the pass was open.
    poisoned: bool,
}

/// The open pass's tally, shared by every clone of one view's [`IndexDb`].
#[derive(Default)]
pub(crate) struct Bm25PassLedger {
    open: AtomicBool,
    full: AtomicBool,
    state: Mutex<PassState>,
}

impl Bm25PassLedger {
    /// Whether a pass is collecting: writers then record their deltas instead of dropping the
    /// exact marker.
    pub(crate) fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    fn overflowed(&self) -> bool {
        self.full.load(Ordering::Relaxed)
    }

    /// Mark the open pass unable to vouch for its tally (a staging or commit error): it then
    /// recomputes and sweeps, which is exact whatever the failed batch did.
    pub(crate) fn poison(&self) {
        if self.is_open() {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .poisoned = true;
        }
    }

    /// Fold one committed batch's delta in.
    pub(crate) fn absorb(&self, delta: Bm25Delta) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.docs += delta.docs;
        state.total_len += delta.total_len;
        state.poisoned |= delta.poisoned;
        if state.overflowed || delta.overflowed {
            state.overflowed = true;
            return;
        }
        state.retired_bytes += delta.retired_bytes;
        if state.retired_bytes > RETIRED_KEYS_CEILING_BYTES {
            state.overflowed = true;
            state.retired = AHashMap::new();
            self.full.store(true, Ordering::Relaxed);
            return;
        }
        for (content, postings) in delta.retired {
            state.retired.entry(content).or_default().extend(postings);
        }
    }

    fn reset(&self) -> PassState {
        self.open.store(false, Ordering::Release);
        self.full.store(false, Ordering::Relaxed);
        std::mem::take(&mut *self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }
}

/// How a finished pass brought the BM25 state up to date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PassOutcome {
    /// The stats came from the stored base plus the ledger, not a walk of the view's forward rows.
    pub(crate) stats_from_ledger: bool,
    /// The whole posting list was swept.
    pub(crate) swept: bool,
}

/// An open full pass; see the module docs. Dropping it unfinished (a cancelled or failed pass)
/// closes the ledger and leaves the view's marker unset and the group's sweep owed.
pub(crate) struct Bm25Pass {
    db: IndexDb,
    /// Stored `(N, total_len)` when the pass began, when they were exact; `None` means recompute.
    base: Option<(u64, u64)>,
    /// The group's owed token as the pass found it: a debt this pass pays with a sweep.
    owed_before: Option<u64>,
    /// The owed token this pass set at its begin when none was owed, cleared by its finish.
    owed_own: Option<u64>,
}

impl IndexDb {
    /// The meta marker drop a forward-row write outside a pass stages into its own batch.
    pub(crate) fn bm25_exact_marker() -> &'static [u8] {
        META_BM25_EXACT
    }

    /// Record that the shared posting list may hold garbage, for forward rows deleted without the
    /// writer (a rebuild clearing a view's keyspaces).
    pub(crate) fn mark_bm25_sweep_owed(&self) -> Result<(), IndexError> {
        Ok(self
            .meta
            .raw()
            .insert(META_BM25_SWEEP_OWED, fresh_token().to_be_bytes())?)
    }

    fn sweep_owed_token(&self) -> Result<Option<u64>, IndexError> {
        Ok(self
            .meta
            .raw()
            .get(META_BM25_SWEEP_OWED)?
            .and_then(|bytes| <[u8; 8]>::try_from(&bytes[..]).ok())
            .map(u64::from_be_bytes))
    }

    /// Clear the owed row if it still holds `token`. Under the gate every owed-setting commit holds
    /// shared, so a debt incurred after `token` was read is never cleared.
    fn clear_sweep_owed_if(&self, token: u64) -> Result<(), IndexError> {
        let _exclusive = self.bm25_gate.write();
        if self.sweep_owed_token()? == Some(token) {
            self.meta.raw().remove(META_BM25_SWEEP_OWED)?;
        }
        Ok(())
    }

    /// Open a full pass: read whether the view's stats are exact, drop the view's marker, owe the
    /// group a sweep until the pass finishes, and start collecting.
    pub(crate) fn begin_bm25_pass(&self) -> Result<Bm25Pass, IndexError> {
        self.bm25_pass.reset();
        let stat = |key: &[u8]| -> Result<Option<u64>, IndexError> {
            Ok(self
                .meta
                .get(key)?
                .and_then(|bytes| <[u8; 8]>::try_from(&bytes[..]).ok())
                .map(u64::from_be_bytes))
        };
        let base = if self.code_bm25_by_path.keys().next().transpose()?.is_none() {
            Some((0, 0))
        } else if self.meta.get(META_BM25_EXACT)?.is_some() {
            match (stat(META_BM25_DOC_COUNT)?, stat(META_BM25_TOTAL_LEN)?) {
                (Some(n), Some(len)) => Some((n, len)),
                _ => None,
            }
        } else {
            None
        };
        self.meta.remove(META_BM25_EXACT)?;
        let owed_before = self.sweep_owed_token()?;
        let owed_own = match owed_before {
            Some(_) => None,
            None => {
                let own = fresh_token();
                self.meta.raw().insert(META_BM25_SWEEP_OWED, own.to_be_bytes())?;
                Some(own)
            }
        };
        self.bm25_pass.open.store(true, Ordering::Release);
        Ok(Bm25Pass {
            db: self.clone(),
            base,
            owed_before,
            owed_own,
        })
    }
}

impl Bm25Pass {
    /// Whether this pass will retire from its ledger rather than sweep. For a test to pin which
    /// path ran.
    #[cfg(test)]
    pub(crate) fn is_exact(&self) -> bool {
        self.base.is_some()
    }

    /// Retire the postings the pass's deleted rows named and stamp the stats, then mark the state
    /// exact. Falls back to the full sweep and recompute when the pass began inexact or its
    /// ledger overflowed. On an error the marker stays unset.
    pub(crate) fn finish(self) -> Result<PassOutcome, IndexError> {
        let db = &self.db;
        let state = db.bm25_pass.reset();
        let exact = self
            .base
            .filter(|_| !state.overflowed && !state.poisoned)
            .and_then(|(n, len)| {
                let n = i64::try_from(n).ok()?.checked_add(state.docs)?;
                let len = i64::try_from(len).ok()?.checked_add(state.total_len)?;
                Some((u64::try_from(n).ok()?, u64::try_from(len).ok()?))
            });
        // Stats and postings fail over separately: inexact stats need only a recompute of this ~keep
        // view; the postings need the sweep only when a debt is owed or the tally is incomplete. ~keep
        let sweep = self.owed_before.is_some() || state.overflowed || state.poisoned;
        let outcome = PassOutcome {
            stats_from_ledger: exact.is_some(),
            swept: sweep,
        };
        if sweep {
            let owed = db.sweep_owed_token()?;
            drop(state);
            db.sweep_bm25_postings()?;
            if let Some(owed) = owed {
                db.clear_sweep_owed_if(owed)?;
            }
        } else {
            db.retire_bm25_postings(Bm25Candidates(state.retired))?;
            if let Some(own) = self.owed_own {
                db.clear_sweep_owed_if(own)?;
            }
        }
        match exact {
            Some((n, len)) => {
                db.meta.insert(META_BM25_DOC_COUNT, n.to_be_bytes())?;
                db.meta.insert(META_BM25_TOTAL_LEN, len.to_be_bytes())?;
            }
            None => db.recompute_bm25_stats()?,
        }
        db.meta.insert(META_BM25_EXACT, [1u8])?;
        Ok(outcome)
    }
}

impl Drop for Bm25Pass {
    fn drop(&mut self) {
        if self.db.bm25_pass.is_open() {
            self.db.bm25_pass.reset();
        }
    }
}

#[cfg(test)]
#[path = "bm25_pass_tests.rs"]
mod tests;
