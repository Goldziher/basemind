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
//! - **The view's stats are exact** (per view). `meta` holds a dirty token, replaced in the same
//!   batch by every forward-row write made outside a pass and by every pass when it begins, and an
//!   exact stamp, the token a finished pass still found in place. The stored stats are a valid base
//!   only while the two are equal, so a pass that dies, or a write the ledger did not see, leaves
//!   them unequal. A view with no forward row at all is exact at zero, which makes a new
//!   worktree's first scan cheap.
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ahash::AHashMap;

use super::bm25_live::Bm25Candidates;
use super::keys::{self, Bm25Key};
use super::{IndexDb, IndexError, META_BM25_DOC_COUNT, META_BM25_TOTAL_LEN};

/// Per-view `meta`: the dirty token a finished pass found in place; the stored stats are exact while
/// it equals [`META_BM25_DIRTY`].
const META_BM25_EXACT: &[u8] = b"code_bm25_exact";
/// Per-view `meta`: a token replaced by every forward-row write outside a pass and by every pass
/// when it begins. The stats are exact while [`META_BM25_EXACT`] holds the same token.
pub(crate) const META_BM25_DIRTY: &[u8] = b"code_bm25_dirty";

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
    /// The pass the delta was staged under; `0` until the writer stages in one.
    generation: u64,
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

    /// Bind the delta to the pass of `generation` (the first one it stages under).
    pub(crate) fn staged_under(&mut self, generation: u64) {
        if self.generation == 0 {
            self.generation = generation;
        }
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

/// The open pass's tally. One per view, shared by every handle on it (`GroupDb::open_view`), so a
/// second handle's writes land in the same pass.
pub(crate) struct Bm25PassLedger {
    /// The open pass's generation; `0` while none is open.
    generation: AtomicU64,
    next_generation: AtomicU64,
    /// [`RETIRED_KEYS_CEILING_BYTES`], lowered by a test to reach the overflow with a small pass.
    ceiling: AtomicU64,
    full: AtomicBool,
    state: Mutex<PassState>,
}

impl Default for Bm25PassLedger {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(0),
            next_generation: AtomicU64::new(1),
            ceiling: AtomicU64::new(RETIRED_KEYS_CEILING_BYTES),
            full: AtomicBool::new(false),
            state: Mutex::default(),
        }
    }
}

impl Bm25PassLedger {
    /// The open pass's generation, or `None` while no pass is collecting.
    pub(crate) fn open_generation(&self) -> Option<u64> {
        Some(self.generation.load(Ordering::Acquire)).filter(|generation| *generation != 0)
    }

    /// Whether a pass is collecting: writers then record their deltas instead of marking the view
    /// dirty.
    pub(crate) fn is_open(&self) -> bool {
        self.open_generation().is_some()
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

    /// Fold one committed batch's delta in. `false` when the delta belongs to no open pass (it was
    /// staged under one that has finished), so the caller must mark the view dirty instead.
    pub(crate) fn absorb(&self, delta: Bm25Delta) -> bool {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.open_generation() != Some(delta.generation) {
            return false;
        }
        state.docs += delta.docs;
        state.total_len += delta.total_len;
        state.poisoned |= delta.poisoned;
        if state.overflowed || delta.overflowed {
            state.overflowed = true;
            return true;
        }
        state.retired_bytes += delta.retired_bytes;
        if state.retired_bytes > self.ceiling.load(Ordering::Relaxed) {
            state.overflowed = true;
            state.retired = AHashMap::new();
            self.full.store(true, Ordering::Relaxed);
            return true;
        }
        for (content, postings) in delta.retired {
            state.retired.entry(content).or_default().extend(postings);
        }
        true
    }

    fn open(&self) -> u64 {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = PassState::default();
        self.full.store(false, Ordering::Relaxed);
        self.generation.store(generation, Ordering::Release);
        generation
    }

    #[cfg(test)]
    pub(crate) fn set_ceiling_for_test(&self, bytes: u64) {
        self.ceiling.store(bytes, Ordering::Relaxed);
    }

    /// Close the pass of `generation` and hand back its tally; `None` when it is not the open one.
    fn close(&self, generation: u64) -> Option<PassState> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.generation.load(Ordering::Acquire) != generation {
            return None;
        }
        self.generation.store(0, Ordering::Release);
        self.full.store(false, Ordering::Relaxed);
        Some(std::mem::take(&mut *state))
    }
}

/// Which pass a [`Bm25Pass`] ends: only a full pass pays the group's owed sweep. An incremental
/// pass (a watcher batch) leaves a debt, or the garbage of an incomplete tally, owed to the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PassKind {
    Full,
    Incremental,
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
/// closes the ledger and leaves the view's stats unvouched and the group's sweep owed.
pub(crate) struct Bm25Pass {
    db: IndexDb,
    kind: PassKind,
    generation: u64,
    /// The dirty token this pass wrote at its begin; the view is vouched for only if it is still
    /// there at the finish.
    token: u64,
    /// Stored `(N, total_len)` when the pass began, when they were exact; `None` means recompute.
    base: Option<(u64, u64)>,
    /// The group's owed token as the pass found it: a debt this pass pays with a sweep.
    owed_before: Option<u64>,
    /// The owed token this pass set at its begin when none was owed, cleared by its finish.
    owed_own: Option<u64>,
}

impl IndexDb {
    fn bm25_stat(&self, key: &[u8]) -> Result<Option<u64>, IndexError> {
        Ok(self
            .meta
            .get(key)?
            .and_then(|bytes| <[u8; 8]>::try_from(&bytes[..]).ok())
            .map(u64::from_be_bytes))
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

    /// Open a pass: read whether the view's stats are exact, mark the view dirty with the pass's
    /// own token, owe the group a sweep until the pass finishes, and start collecting.
    pub(crate) fn begin_bm25_pass(&self, kind: PassKind) -> Result<Bm25Pass, IndexError> {
        let exact = self.bm25_stat(META_BM25_EXACT)?;
        let base = if self.code_bm25_by_path.keys().next().transpose()?.is_none() {
            Some((0, 0))
        } else if exact.is_some() && exact == self.bm25_stat(META_BM25_DIRTY)? {
            self.bm25_stat(META_BM25_DOC_COUNT)?
                .zip(self.bm25_stat(META_BM25_TOTAL_LEN)?)
        } else {
            None
        };
        let token = fresh_token();
        self.meta.insert(META_BM25_DIRTY, token.to_be_bytes())?;
        let owed_before = self.sweep_owed_token()?;
        let owed_own = match owed_before {
            Some(_) => None,
            None => {
                let own = fresh_token();
                self.meta.raw().insert(META_BM25_SWEEP_OWED, own.to_be_bytes())?;
                Some(own)
            }
        };
        let generation = self.bm25_pass.open();
        Ok(Bm25Pass {
            db: self.clone(),
            kind,
            generation,
            token,
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
    /// ledger overflowed. On an error nothing is vouched for.
    pub(crate) fn finish(self) -> Result<PassOutcome, IndexError> {
        let db = &self.db;
        let Some(state) = db.bm25_pass.close(self.generation) else {
            return Err(IndexError::Io {
                path: std::path::PathBuf::new(),
                source: std::io::Error::other("the BM25 pass ledger was closed by another pass"),
            });
        };
        // A write that reached the view outside the ledger replaced the token: it is in neither ~keep
        // the base nor the tally, so the stats must be recomputed and not vouched for. ~keep
        let untouched = db.bm25_stat(META_BM25_DIRTY)? == Some(self.token);
        let exact = self.base.filter(|_| untouched && !state.poisoned).and_then(|(n, len)| {
            let n = i64::try_from(n).ok()?.checked_add(state.docs)?;
            let len = i64::try_from(len).ok()?.checked_add(state.total_len)?;
            Some((u64::try_from(n).ok()?, u64::try_from(len).ok()?))
        });
        // Stats and postings fail over separately: inexact stats need only a recompute of this ~keep
        // view; the postings need the sweep only when a debt is owed or the tally is incomplete. ~keep
        let complete = !state.overflowed && !state.poisoned;
        let sweep = self.kind == PassKind::Full && (self.owed_before.is_some() || !complete);
        let outcome = PassOutcome {
            stats_from_ledger: exact.is_some(),
            swept: sweep,
        };
        // The stats first: they do not depend on the retirement, and a retirement that fails then ~keep
        // costs only garbage, still owed to the next full pass. ~keep
        match exact {
            Some((n, len)) => {
                db.meta.insert(META_BM25_DOC_COUNT, n.to_be_bytes())?;
                db.meta.insert(META_BM25_TOTAL_LEN, len.to_be_bytes())?;
            }
            None => db.recompute_bm25_stats()?,
        }
        if db.bm25_stat(META_BM25_DIRTY)? == Some(self.token) {
            db.meta.insert(META_BM25_EXACT, self.token.to_be_bytes())?;
        }
        if sweep {
            let owed = db.sweep_owed_token()?;
            drop(state);
            db.sweep_bm25_postings()?;
            if let Some(owed) = owed {
                db.clear_sweep_owed_if(owed)?;
            }
        } else if complete {
            db.retire_bm25_postings(Bm25Candidates(state.retired))?;
            if let Some(own) = self.owed_own {
                db.clear_sweep_owed_if(own)?;
            }
        }
        Ok(outcome)
    }
}

#[cfg(test)]
impl IndexDb {
    /// Every posting in the (group's) posting list.
    pub(crate) fn posting_count_for_test(&self) -> usize {
        self.code_bm25_postings.iter().count()
    }
}

impl Drop for Bm25Pass {
    fn drop(&mut self) {
        self.db.bm25_pass.close(self.generation);
    }
}

#[cfg(test)]
#[path = "bm25_pass_tests.rs"]
mod tests;
