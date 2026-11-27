//! Content-keyed pools that let the read stacks of one host share what they derive from the same
//! content, instead of each holding its own copy.
//!
//! Three pools, each weak or pruned so nothing outlives the stacks that read it:
//!
//! - [`StrPool`] interns the strings every file-map entry repeats (content hash, language), so the
//!   file maps of two worktrees of one repository hold one allocation per distinct string.
//! - [`TermPool`] interns a file's [`FileTerms`] by `(content hash, extract epoch)`, the same key
//!   the outline cache decodes by, so two term indexes over identical content hold one projection.
//! - [`GraphPool`] holds the import graphs of the host by the digest of the content they were built
//!   from ([`graph_key`]), so two worktrees that check out the same tree read one graph.
//!
//! The string and term pools ride on the shared outline cache ([`crate::engine::l1_cache::L1Cache`]),
//! which lives while any stack of a blob directory holds it; the graph pool rides on the host's
//! [`SharedOutlines`](crate::engine::l1_shared::SharedOutlines). A stack with no host gets pools of
//! its own, which share nothing and cost nothing.
//!
//! Residency is bounded two ways. The term and graph pools hold values weakly, so a value lives
//! exactly while some index or workspace slot holds it, and a pool's own bookkeeping is one small
//! entry per live value, swept when it doubles. The string pool holds its strings strongly and
//! drops the ones no file map names any more at the same doubling.

use std::sync::{Arc, Weak};

use ahash::{AHashMap, AHashSet};
use parking_lot::Mutex;

use crate::engine::term_index::FileTerms;
use crate::path::RelPath;

/// Entries a pool holds before its first sweep of dead ones.
const SWEEP_FLOOR: usize = 1024;

/// What a pool tracks, swept when it has doubled since the last sweep.
struct Sweep {
    at: usize,
}

impl Sweep {
    const fn new() -> Self {
        Self { at: SWEEP_FLOOR }
    }

    fn due(&self, len: usize) -> bool {
        len >= self.at
    }

    fn done(&mut self, live: usize) {
        self.at = live.saturating_mul(2).max(SWEEP_FLOOR);
    }
}

/// Interns strings: [`intern`](Self::intern) of equal strings returns clones of one allocation.
pub(crate) struct StrPool {
    inner: Mutex<(AHashSet<Arc<str>>, Sweep)>,
}

impl Default for StrPool {
    fn default() -> Self {
        Self {
            inner: Mutex::new((AHashSet::new(), Sweep::new())),
        }
    }
}

impl StrPool {
    /// The pooled allocation holding `text`, added when none is.
    pub(crate) fn intern(&self, text: &str) -> Arc<str> {
        let mut inner = self.inner.lock();
        let (set, sweep) = &mut *inner;
        if let Some(pooled) = set.get(text) {
            return Arc::clone(pooled);
        }
        if sweep.due(set.len()) {
            set.retain(|pooled| Arc::strong_count(pooled) > 1);
            sweep.done(set.len());
        }
        let pooled: Arc<str> = Arc::from(text);
        set.insert(Arc::clone(&pooled));
        pooled
    }

    /// Strings the pool holds, those no file map names any more included until the next sweep.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().0.len()
    }
}

/// What a file's terms are a function of: the content hash and the extract epoch it was decoded
/// under, which is also what keys the outline cache.
pub(crate) type TermKey = (Arc<str>, u16);

/// Interns [`FileTerms`] by [`TermKey`], holding them weakly.
pub(crate) struct TermPool {
    inner: Mutex<(AHashMap<TermKey, Weak<FileTerms>>, Sweep)>,
}

impl Default for TermPool {
    fn default() -> Self {
        Self {
            inner: Mutex::new((AHashMap::new(), Sweep::new())),
        }
    }
}

impl TermPool {
    /// The live terms for `key`, or `make`'s, which are then the pool's for the next caller. `make`
    /// runs with no lock held; of two callers racing on one key the first to finish wins and the
    /// other drops its projection.
    pub(crate) fn intern(&self, key: TermKey, make: impl FnOnce() -> FileTerms) -> Arc<FileTerms> {
        if let Some(live) = self.live(&key) {
            return live;
        }
        let made = Arc::new(make());
        let mut inner = self.inner.lock();
        let (map, sweep) = &mut *inner;
        if let Some(live) = map.get(&key).and_then(Weak::upgrade) {
            return live;
        }
        if sweep.due(map.len()) {
            map.retain(|_, terms| terms.strong_count() > 0);
            sweep.done(map.len());
        }
        map.insert(key, Arc::downgrade(&made));
        made
    }

    fn live(&self, key: &TermKey) -> Option<Arc<FileTerms>> {
        self.inner.lock().0.get(key).and_then(Weak::upgrade)
    }

    /// How many entries have a live projection, and the heap bytes of those.
    #[cfg(test)]
    pub(crate) fn live_stats(&self) -> (usize, usize) {
        let inner = self.inner.lock();
        inner
            .0
            .values()
            .filter_map(Weak::upgrade)
            .fold((0, 0), |(count, bytes), terms| (count + 1, bytes + terms.heap_bytes()))
    }
}

/// The pools keyed by file content, shared by every map over one outline cache.
#[derive(Default)]
pub(crate) struct ContentPools {
    pub(crate) strings: StrPool,
    pub(crate) terms: TermPool,
}

/// The digest of the content an import graph is a function of.
pub(crate) type GraphKey = [u8; 32];

/// One input of [`graph_key`]: a path and what the index records for its content.
pub(crate) struct KeyedFile<'a> {
    pub(crate) path: &'a RelPath,
    pub(crate) hash_hex: &'a str,
    pub(crate) language: &'a str,
    pub(crate) extract_epoch: u16,
}

/// The digest of everything an import graph is built from: every indexed file's path, content hash,
/// language and extract epoch, every document's path and content hash (resolvers read package and compiler
/// configuration from the tree), and whether the view's import probes were complete. Two workspaces
/// with equal digests read the same tree through the same resolvers and so build the same graph.
/// The digest does not depend on the order of the inputs.
pub(crate) fn graph_key<'a>(
    files: impl IntoIterator<Item = KeyedFile<'a>>,
    documents: impl IntoIterator<Item = (&'a RelPath, &'a str)>,
    probes_complete: bool,
) -> GraphKey {
    let mut files: Vec<KeyedFile<'a>> = files.into_iter().collect();
    files.sort_unstable_by(|a, b| a.path.cmp(b.path));
    let mut documents: Vec<(&RelPath, &str)> = documents.into_iter().collect();
    documents.sort_unstable_by(|a, b| a.0.cmp(b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(&[u8::from(probes_complete)]);
    hasher.update(&(files.len() as u64).to_le_bytes());
    for file in &files {
        update_field(&mut hasher, file.path.as_bytes());
        update_field(&mut hasher, file.hash_hex.as_bytes());
        update_field(&mut hasher, file.language.as_bytes());
        hasher.update(&file.extract_epoch.to_le_bytes());
    }
    hasher.update(&(documents.len() as u64).to_le_bytes());
    for (path, hash_hex) in &documents {
        update_field(&mut hasher, path.as_bytes());
        update_field(&mut hasher, hash_hex.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// A length-prefixed field, so no two sequences of fields feed the hasher the same bytes.
fn update_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// The import graphs of a host by [`GraphKey`], held weakly: a graph lives while some workspace
/// keeps it and is shared with every workspace that builds the same one in the meantime.
pub(crate) struct GraphPool {
    inner: Mutex<(
        AHashMap<GraphKey, Weak<crate::engine::import_graph::ImportGraph>>,
        Sweep,
    )>,
}

impl Default for GraphPool {
    fn default() -> Self {
        Self {
            inner: Mutex::new((AHashMap::new(), Sweep::new())),
        }
    }
}

impl GraphPool {
    /// The live graph for `key`, or the one `build` makes, which is then the pool's for the next
    /// caller. `build` runs with no lock held.
    pub(crate) fn get_or_build<E>(
        &self,
        key: GraphKey,
        build: impl FnOnce() -> Result<crate::engine::import_graph::ImportGraph, E>,
    ) -> Result<Arc<crate::engine::import_graph::ImportGraph>, E> {
        if let Some(live) = self.inner.lock().0.get(&key).and_then(Weak::upgrade) {
            return Ok(live);
        }
        let built = Arc::new(build()?);
        let mut inner = self.inner.lock();
        let (map, sweep) = &mut *inner;
        if let Some(live) = map.get(&key).and_then(Weak::upgrade) {
            return Ok(live);
        }
        if sweep.due(map.len()) {
            map.retain(|_, graph| graph.strong_count() > 0);
            sweep.done(map.len());
        }
        map.insert(key, Arc::downgrade(&built));
        Ok(built)
    }

    /// How many graphs are live, and their approximate bytes together.
    #[cfg(test)]
    pub(crate) fn live_stats(&self) -> (usize, usize) {
        let inner = self.inner.lock();
        inner
            .0
            .values()
            .filter_map(Weak::upgrade)
            .fold((0, 0), |(count, bytes), graph| {
                (count + 1, bytes + graph.approx_bytes())
            })
    }
}

#[cfg(test)]
#[path = "content_pools_tests.rs"]
mod tests;
