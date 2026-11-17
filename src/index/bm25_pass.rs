//! A full pass's BM25 upkeep without walking the whole posting list.
//!
//! A full pass used to end with [`IndexDb::sweep_bm25_postings`], which reads every
//! `code_bm25_postings` key (one per `(term, chunk)` pair, ~8.6 M on a 4 000-file corpus), and
//! [`IndexDb::recompute_bm25_stats`], which reads every forward row. Both pull the whole keyspace
//! through the block cache, which on a group database is sized at its 256 MiB ceiling, so every
//! full pass filled the cache and took a minute in debug builds, whatever had changed.
//!
//! Every posting a pass can leave dead belongs to a forward row the pass deletes, and the writer
//! reads those rows before deleting them anyway. While a pass is open ([`IndexDb::begin_bm25_pass`]),
//! each committed batch hands the ledger the postings its deleted rows named and the change in
//! `(N, total_len)` its deleted and inserted rows make. [`Bm25Pass::finish`] then retires exactly
//! those postings and stamps the stats as the stored ones plus the delta.
//!
//! The shortcut is taken only when the stored state was exact when the pass began: a `meta`
//! marker says no forward row has changed outside a completed pass since. A write outside a pass
//! (an incremental scan, a cleanup) drops the marker in its own batch; a pass drops it at its
//! start and stamps it only once its retirement and stats are written, so a pass that dies midway
//! leaves it unset. An unset marker, or a pass whose retired keys outgrow
//! [`RETIRED_KEYS_CEILING_BYTES`], falls back to the full sweep and recompute.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ahash::AHashMap;

use super::bm25_live::Bm25Candidates;
use super::keys::{self, Bm25Key};
use super::{IndexDb, IndexError, META_BM25_DOC_COUNT, META_BM25_TOTAL_LEN};

/// `meta` marker: the posting list holds no posting this view orphaned and the stored BM25 stats
/// are exact. Present only between a completed pass and the next forward-row write outside one.
const META_BM25_EXACT: &[u8] = b"code_bm25_exact";

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
}

impl Bm25Delta {
    pub(crate) fn is_empty(&self) -> bool {
        self.docs == 0 && self.total_len == 0 && self.retired.is_empty() && !self.overflowed
    }

    /// A forward row the batch deletes: its chunk leaves `N`, and its postings may now be dead.
    pub(crate) fn remove_row(&mut self, ledger: &Bm25PassLedger, key: &[u8], value: &[u8]) {
        let (Some((_, content, ordinal)), Some((doclen, _, terms))) = (
            keys::parse_code_bm25_by_path(key),
            keys::parse_code_bm25_by_path_value(value),
        ) else {
            return;
        };
        self.docs -= 1;
        self.total_len -= i64::from(doclen);
        if self.overflowed || ledger.overflowed() {
            self.overflowed = true;
            return;
        }
        let Ok(terms) = rmp_serde::from_slice::<Vec<String>>(terms) else {
            self.overflowed = true;
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

    /// Fold one committed batch's delta in.
    pub(crate) fn absorb(&self, delta: Bm25Delta) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.docs += delta.docs;
        state.total_len += delta.total_len;
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

/// An open full pass; see the module docs. Dropping it unfinished (a cancelled or failed pass)
/// closes the ledger and leaves the exact marker unset, so the next pass sweeps.
pub(crate) struct Bm25Pass {
    db: IndexDb,
    /// Stored `(N, total_len)` when the pass began, when they were exact; `None` means sweep.
    base: Option<(u64, u64)>,
}

impl IndexDb {
    /// The meta marker drop a forward-row write outside a pass stages into its own batch.
    pub(crate) fn bm25_exact_marker() -> &'static [u8] {
        META_BM25_EXACT
    }

    /// Open a full pass: read whether the stored BM25 state is exact, drop the marker, and start
    /// collecting. An empty posting list is exact at zero, which is what makes a first scan cheap.
    pub(crate) fn begin_bm25_pass(&self) -> Result<Bm25Pass, IndexError> {
        self.bm25_pass.reset();
        let stat = |key: &[u8]| -> Result<Option<u64>, IndexError> {
            Ok(self
                .meta
                .get(key)?
                .and_then(|bytes| <[u8; 8]>::try_from(&bytes[..]).ok())
                .map(u64::from_be_bytes))
        };
        let base = if self.code_bm25_postings.raw().first_key_value().is_none() {
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
        self.bm25_pass.open.store(true, Ordering::Release);
        Ok(Bm25Pass { db: self.clone(), base })
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
    pub(crate) fn finish(self) -> Result<(), IndexError> {
        let db = &self.db;
        let state = db.bm25_pass.reset();
        let exact = self.base.filter(|_| !state.overflowed).and_then(|(n, len)| {
            let n = i64::try_from(n).ok()?.checked_add(state.docs)?;
            let len = i64::try_from(len).ok()?.checked_add(state.total_len)?;
            Some((u64::try_from(n).ok()?, u64::try_from(len).ok()?))
        });
        match exact {
            Some((n, len)) => {
                db.retire_bm25_postings(Bm25Candidates(state.retired))?;
                db.meta.insert(META_BM25_DOC_COUNT, n.to_be_bytes())?;
                db.meta.insert(META_BM25_TOTAL_LEN, len.to_be_bytes())?;
            }
            None => {
                drop(state);
                db.sweep_bm25_postings()?;
                db.recompute_bm25_stats()?;
            }
        }
        db.meta.insert(META_BM25_EXACT, [1u8])?;
        Ok(())
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
mod tests {
    use super::*;
    use crate::index::view::IndexView;
    use crate::path::RelPath;
    use crate::search::bm25::ChunkPosting;

    fn postings(hash: &str, terms: &[&str]) -> Vec<ChunkPosting> {
        vec![ChunkPosting {
            chunk_id: format!("{hash}:0"),
            doclen: u32::try_from(terms.len()).unwrap(),
            terms: terms.iter().map(|t| ((*t).to_string(), 1)).collect(),
        }]
    }

    fn upsert(db: &IndexDb, path: &str, hash: &str, terms: &[&str]) {
        let mut writer = db.writer();
        writer
            .upsert_bm25_file(
                &RelPath::from(path),
                &keys::bm25_content_key(hash, "rust", "d"),
                &postings(hash, terms),
            )
            .unwrap();
        writer.commit().unwrap();
    }

    fn posting_count(db: &IndexDb) -> usize {
        db.code_bm25_postings.iter().count()
    }

    /// What the full sweep and recompute would leave, to compare the ledger's shortcut against.
    fn swept(db: &IndexDb) -> (usize, Option<(u64, u64)>) {
        db.sweep_bm25_postings().unwrap();
        db.recompute_bm25_stats().unwrap();
        (posting_count(db), db.view().bm25_stats())
    }

    #[test]
    fn a_pass_over_an_empty_index_is_exact_and_needs_no_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        let pass = db.begin_bm25_pass().unwrap();
        assert!(pass.is_exact(), "an empty posting list is exact at zero");
        upsert(&db, "src/a.rs", "a1", &["alpha", "beta"]);
        upsert(&db, "src/b.rs", "b1", &["beta"]);
        pass.finish().unwrap();
        assert_eq!(db.view().bm25_stats(), Some((2, 3)));
        assert_eq!(posting_count(&db), 3);
        assert_eq!(swept(&db), (3, Some((2, 3))), "the ledger agrees with a full recompute");
    }

    #[test]
    fn a_pass_retires_exactly_the_postings_its_replaced_rows_named() {
        let dir = tempfile::tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        let pass = db.begin_bm25_pass().unwrap();
        upsert(&db, "src/a.rs", "a1", &["alpha", "beta"]);
        upsert(&db, "src/b.rs", "b1", &["gamma"]);
        upsert(&db, "src/c.rs", "a1", &["alpha", "beta"]);
        pass.finish().unwrap();

        let pass = db.begin_bm25_pass().unwrap();
        assert!(
            pass.is_exact(),
            "the marker the last pass stamped vouches for the stored state"
        );
        upsert(&db, "src/a.rs", "a2", &["delta"]);
        upsert(&db, "src/b.rs", "b2", &["epsilon", "zeta"]);
        let mut writer = db.writer();
        writer.remove_bm25_file(&RelPath::from("src/c.rs")).unwrap();
        writer.commit().unwrap();
        pass.finish().unwrap();

        assert_eq!(db.view().bm25_stats(), Some((2, 3)));
        assert_eq!(
            posting_count(&db),
            3,
            "every posting of a1 and b1 is retired; a2 and b2 remain"
        );
        assert_eq!(swept(&db), (3, Some((2, 3))), "nothing was left for a sweep to find");
    }

    #[test]
    fn a_write_outside_a_pass_or_an_unfinished_pass_makes_the_next_pass_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        let pass = db.begin_bm25_pass().unwrap();
        upsert(&db, "src/a.rs", "a1", &["alpha"]);
        pass.finish().unwrap();

        upsert(&db, "src/a.rs", "a2", &["beta"]);
        let pass = db.begin_bm25_pass().unwrap();
        assert!(
            !pass.is_exact(),
            "an incremental write dropped the marker in its own batch"
        );
        pass.finish().unwrap();
        assert_eq!(posting_count(&db), 1, "the fallback sweep retired a1's posting");
        assert_eq!(db.view().bm25_stats(), Some((1, 1)));

        let pass = db.begin_bm25_pass().unwrap();
        assert!(pass.is_exact());
        upsert(&db, "src/a.rs", "a3", &["gamma"]);
        drop(pass);
        assert!(!db.bm25_pass.is_open(), "dropping the pass closes the ledger");
        let pass = db.begin_bm25_pass().unwrap();
        assert!(!pass.is_exact(), "a pass that never finished left the marker unset");
        pass.finish().unwrap();
        assert_eq!(posting_count(&db), 1);
    }

    #[test]
    fn a_pass_whose_retired_keys_outgrow_the_ceiling_sweeps_instead() {
        let ledger = Bm25PassLedger::default();
        let mut delta = Bm25Delta {
            retired_bytes: RETIRED_KEYS_CEILING_BYTES + 1,
            ..Bm25Delta::default()
        };
        delta.retired.insert([0; keys::BM25_KEY_LEN], vec![vec![1]]);
        ledger.absorb(delta);
        assert!(ledger.overflowed());
        let state = ledger.reset();
        assert!(
            state.overflowed && state.retired.is_empty(),
            "the keys are dropped, not kept"
        );
    }
}
