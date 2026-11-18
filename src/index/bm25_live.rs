//! The BM25 chunks one view holds (ADR-0028 rule 7), and the gate that keeps the group's posting
//! sweep from deleting a posting a concurrent scan is writing.
//!
//! In a group index every view shares the `code_bm25_postings` rows, keyed by [`Bm25Key`]. A view
//! scores only the postings of the keys its own `code_bm25_by_path` rows name: [`Bm25Live`] maps
//! each such key to its content hash, so a hit is reported under the chunk id (`<hash>:<ordinal>`)
//! a standalone index would give it. The map is built from those rows on first use and cached per
//! view; every commit that changes them, and every clear of the view, invalidates it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ahash::AHashMap;

use super::IndexError;
#[cfg(feature = "code-search")]
use super::keys::BM25_KEY_LEN;
#[cfg(feature = "code-search")]
use super::keys::parse_code_bm25_posting_chunk;
use super::keys::{Bm25Key, parse_code_bm25_by_path, parse_code_bm25_by_path_value};
#[cfg(all(test, feature = "code-search"))]
use super::keys::{code_bm25_by_path_prefix, code_bm25_posting};
use super::view::KvResult;
use crate::path::RelPath;

/// The keys of the BM25 chunks a view holds, each with the content hash it was written for.
#[derive(Debug, Default)]
pub struct Bm25Live {
    by_key: AHashMap<Bm25Key, Box<str>>,
}

impl Bm25Live {
    /// The live keys named by `code_bm25_by_path` rows (`rows`, in any order).
    pub(crate) fn from_rows(rows: impl Iterator<Item = KvResult>) -> Result<Self, IndexError> {
        let mut by_key = AHashMap::new();
        for row in rows {
            let (key, value) = row?;
            if let (Some((_, content, _)), Some((_, hash, _))) =
                (parse_code_bm25_by_path(&key), parse_code_bm25_by_path_value(&value))
                && !by_key.contains_key(&content)
            {
                by_key.insert(content, Box::from(hash));
            }
        }
        Ok(Self { by_key })
    }

    /// The content hash of `key`, when the view holds it.
    pub fn hash_of(&self, key: &Bm25Key) -> Option<&str> {
        self.by_key.get(key).map(|hash| &**hash)
    }

    /// Distinct keys the view holds.
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// Whether the view holds no BM25 chunk.
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

/// [`Bm25Live`] of one view, shared by every handle on it. See the module docs.
#[derive(Default)]
pub(crate) struct Bm25LiveCache {
    /// Bumped after every commit that changed the view's forward rows.
    generation: AtomicU64,
    built: parking_lot::Mutex<Option<(u64, Arc<Bm25Live>)>>,
}

impl Bm25LiveCache {
    /// Forget the cached map. Call after the change is committed: a build that started before
    /// the commit is then stored under the older generation and rebuilt on its next use.
    pub(crate) fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// The cached map, or one `build` makes and the cache keeps. Concurrent callers wait for one
    /// build rather than each scanning the view's rows.
    pub(crate) fn get_or_build(
        &self,
        build: impl FnOnce() -> Result<Bm25Live, IndexError>,
    ) -> Result<Arc<Bm25Live>, IndexError> {
        let generation = self.generation.load(Ordering::Acquire);
        let mut built = self.built.lock();
        if let Some((at, live)) = built.as_ref()
            && *at == generation
        {
            return Ok(Arc::clone(live));
        }
        let live = Arc::new(build()?);
        *built = Some((generation, Arc::clone(&live)));
        Ok(live)
    }
}

/// Orders the group's posting sweep against the commits that write postings: a commit holds it
/// shared, the sweep exclusively. Without it a sweep could read the holders, then delete a posting
/// that a scan of another view re-wrote, with its holder, in between.
///
/// It also counts the keys a writer has staged forward rows for and not yet committed: such a writer
/// may have found the key's postings stored and written none, so they stay until it commits its
/// holder row. A writer registers a key while holding the gate shared, so a sweep holding it
/// exclusively sees every registration made before it.
#[derive(Default)]
pub(crate) struct Bm25GateState {
    lock: parking_lot::RwLock<()>,
    #[cfg(feature = "code-search")]
    in_flight: parking_lot::Mutex<AHashMap<Bm25Key, u32>>,
}

/// One per group: see [`Bm25GateState`].
pub(crate) type Bm25Gate = Arc<Bm25GateState>;

impl Bm25GateState {
    pub(crate) fn read(&self) -> parking_lot::RwLockReadGuard<'_, ()> {
        self.lock.read()
    }

    pub(crate) fn write(&self) -> parking_lot::RwLockWriteGuard<'_, ()> {
        self.lock.write()
    }

    #[cfg(feature = "code-search")]
    pub(crate) fn begin_in_flight(&self, key: Bm25Key) {
        *self.in_flight.lock().entry(key).or_default() += 1;
    }

    #[cfg(feature = "code-search")]
    pub(crate) fn end_in_flight(&self, key: &Bm25Key) {
        let mut in_flight = self.in_flight.lock();
        if let Some(count) = in_flight.get_mut(key) {
            *count -= 1;
            if *count == 0 {
                in_flight.remove(key);
            }
        }
    }

    #[cfg(feature = "code-search")]
    fn is_in_flight(&self, key: &Bm25Key) -> bool {
        self.in_flight.lock().contains_key(key)
    }
}

/// The BM25 keys one writer registered in flight; released when the writer commits or drops.
#[cfg(feature = "code-search")]
#[derive(Default)]
pub(crate) struct InFlight {
    gate: Option<Bm25Gate>,
    keys: Vec<Bm25Key>,
}

#[cfg(feature = "code-search")]
impl InFlight {
    pub(crate) fn register(&mut self, gate: &Bm25Gate, key: Bm25Key) {
        gate.begin_in_flight(key);
        self.gate.get_or_insert_with(|| Arc::clone(gate));
        self.keys.push(key);
    }
}

#[cfg(feature = "code-search")]
impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            for key in &self.keys {
                gate.end_in_flight(key);
            }
        }
    }
}

impl super::IndexDb {
    /// The BM25 chunks this view holds, from the cache or built from its forward rows.
    pub fn bm25_live(&self) -> Result<Arc<Bm25Live>, IndexError> {
        self.bm25_live
            .get_or_build(|| Bm25Live::from_rows(self.code_bm25_by_path.iter()))
    }

    /// Whether any view of the group holds `key`, or a writer has it in flight. Call holding the
    /// gate exclusively.
    #[cfg(feature = "code-search")]
    fn bm25_key_is_held(&self, key: &Bm25Key) -> Result<bool, IndexError> {
        Ok(self.bm25_gate.is_in_flight(key) || self.code_bm25_holders.prefix_keys(key).next().transpose()?.is_some())
    }

    /// The key of every BM25 chunk any view of the group holds, or a writer has in flight: one seek
    /// per distinct key of the holders. Call holding the gate exclusively.
    #[cfg(feature = "code-search")]
    fn group_bm25_keys(&self) -> Result<ahash::AHashSet<Bm25Key>, IndexError> {
        let mut live: ahash::AHashSet<Bm25Key> = self.bm25_gate.in_flight.lock().keys().copied().collect();
        let mut from: Vec<u8> = Vec::new();
        while let Some(row) = self
            .code_bm25_holders
            .range(std::ops::Bound::Included(from.as_slice()), std::ops::Bound::Unbounded)
            .next()
        {
            let (key, _) = row?;
            let Some(content) = key.get(..BM25_KEY_LEN).and_then(|head| Bm25Key::try_from(head).ok()) else {
                break;
            };
            live.insert(content);
            match super::name_dict::upper_bound(&content) {
                Some(next) => from = next,
                None => break,
            }
        }
        Ok(live)
    }

    /// Stage the removal of every holder row of view `view_id`, named by its forward rows `forward`:
    /// before those rows are cleared wholesale.
    pub(crate) fn remove_view_holders(&self, forward: &super::scoped::Scoped, view_id: u32) -> Result<(), IndexError> {
        let mut batch = self.db.batch();
        let mut last: Option<(RelPath, Bm25Key)> = None;
        for key in forward.keys() {
            let key = key?;
            let Some((rel, content, _)) = parse_code_bm25_by_path(&key) else {
                continue;
            };
            if last.as_ref().is_some_and(|(r, c)| *r == rel && *c == content) {
                continue;
            }
            self.code_bm25_holders
                .stage_remove(&mut batch, super::keys::code_bm25_holder(&content, view_id, &rel));
            last = Some((rel, content));
        }
        let _shared = self.bm25_gate.read();
        batch.commit()?;
        Ok(())
    }

    /// This handle's view id in the holder rows: `0` for a database of its own.
    pub(crate) fn holder_view_id(&self) -> u32 {
        self.group.as_ref().map_or(0, |own| own.view_id)
    }

    /// The postings this view's chunks at `paths` name, by key: read before a pass replaces or drops
    /// those paths, for [`Self::retire_bm25_postings`] after it. Passes collect these through their
    /// ledger (`index::bm25_pass`); tests still build them directly.
    #[cfg(all(test, feature = "code-search"))]
    pub(crate) fn bm25_posting_candidates<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a String>,
    ) -> Result<Bm25Candidates, IndexError> {
        let mut candidates = Bm25Candidates::default();
        for path in paths {
            let rel = crate::path::RelPath::from(path.as_str());
            for row in self.code_bm25_by_path.prefix(&code_bm25_by_path_prefix(&rel)) {
                let (key, value) = row?;
                let (Some((_, content, ordinal)), Some((_, _, terms))) =
                    (parse_code_bm25_by_path(&key), parse_code_bm25_by_path_value(&value))
                else {
                    continue;
                };
                let postings = candidates.0.entry(content).or_default();
                for term in rmp_serde::from_slice::<Vec<String>>(terms)? {
                    postings.extend(code_bm25_posting(&term, &content, ordinal));
                }
            }
        }
        Ok(candidates)
    }

    /// Delete the postings of every candidate key no view of the group holds any more. Holds the
    /// group's [`Bm25Gate`] exclusively (see [`Self::sweep_bm25_postings`]).
    #[cfg(feature = "code-search")]
    pub(crate) fn retire_bm25_postings(&self, candidates: Bm25Candidates) -> Result<(), IndexError> {
        if candidates.0.is_empty() {
            return Ok(());
        }
        let _exclusive = self.bm25_gate.write();
        let mut writer = self.writer();
        for (content, postings) in candidates.0 {
            if self.bm25_key_is_held(&content)? {
                continue;
            }
            for key in postings {
                writer.remove_bm25_posting(&key);
                if writer.staged_bytes() >= super::BM25_PRUNE_BATCH_BYTES {
                    writer.commit()?;
                    writer = self.writer();
                }
            }
        }
        writer.commit()
    }

    /// Delete every posting whose key no view of the group holds. Holds the group's [`Bm25Gate`]
    /// exclusively: a posting commit writes the forward row with it, so no posting a concurrent scan
    /// commits can be deleted between the read of the forward rows and the delete.
    #[cfg(feature = "code-search")]
    pub(crate) fn sweep_bm25_postings(&self) -> Result<(), IndexError> {
        let _exclusive = self.bm25_gate.write();
        let live = self.group_bm25_keys()?;
        let mut writer = self.writer();
        for key in self.code_bm25_postings.keys() {
            let key = key?;
            let held = parse_code_bm25_posting_chunk(&key).is_some_and(|(content, _)| live.contains(&content));
            if !held {
                writer.remove_bm25_posting(&key);
                if writer.staged_bytes() >= super::BM25_PRUNE_BATCH_BYTES {
                    writer.commit()?;
                    writer = self.writer();
                }
            }
        }
        writer.commit()
    }
}

/// Posting keys of the chunks a pass is about to replace or drop, by their [`Bm25Key`].
#[cfg(feature = "code-search")]
#[derive(Debug, Default)]
pub(crate) struct Bm25Candidates(pub(super) AHashMap<Bm25Key, Vec<Vec<u8>>>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cached_map_is_rebuilt_only_after_an_invalidation() {
        let cache = Bm25LiveCache::default();
        let builds = std::cell::Cell::new(0);
        let build = || {
            builds.set(builds.get() + 1);
            Ok(Bm25Live::default())
        };
        cache.get_or_build(build).expect("build");
        cache.get_or_build(build).expect("cached");
        assert_eq!(builds.get(), 1);
        cache.invalidate();
        cache.get_or_build(build).expect("rebuild");
        assert_eq!(builds.get(), 2);
    }
}
