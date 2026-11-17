//! The in-RAM map over a workspace's index: [`MapCache`], the snapshot every cache-reading tool
//! loads once per call.

use std::sync::Arc;

use crate::engine::l1_cache;
use crate::extract::FileMapL1;
use crate::mcp::{helpers_calls, helpers_impls};
use crate::store::Store;

pub(crate) struct MapCache {
    /// Symbol-free view of every indexed file. Permanent and O(files) — never O(symbols).
    files: l1_cache::FileIndexView,
    /// The only place a decoded [`FileMapL1`] lives: a byte-charged, read-through LRU keyed by
    /// content hash. Shared with the snapshot this cache was derived from, so an incremental
    /// rescan is a refcount bump rather than a rebuild.
    l1: Arc<l1_cache::L1Cache>,
    /// In-RAM callee index: the LAST-RESORT fallback for a session that has neither a Fjall index
    /// nor a daemon to forward `references` / `callers` to.
    /// Built on first use ([`calls_projection`](Self::calls_projection)) and never otherwise, so a
    /// daemon-backed session that only asks point queries holds none of it.
    calls: std::sync::OnceLock<helpers_calls::InRamCallIndex>,
    /// In-RAM trait→impl index, same lazy last-resort role as `calls` for `find_implementations`.
    impls: std::sync::OnceLock<helpers_impls::InRamImplIndex>,
    /// Searchable symbol-name / import text of every file, built on first use (or by the cache
    /// warm) and patched rather than rebuilt by [`with_delta`](Self::with_delta). Backs the
    /// substring sweeps of `symbols` and `dependents`.
    terms: std::sync::OnceLock<Arc<crate::engine::term_index::TermIndex>>,
    /// Every path of the document tier (markdown, config, data, PDFs ...), sorted. The code map
    /// holds code only, so decision records (`docs/adr/*.md`) and path citations to prose / config
    /// files resolve against this set instead of [`Self::files`]. Projected straight from the index
    /// (no blob or LanceDB read), so it is rebuilt with every cache build and delta.
    pub(crate) doc_paths: std::sync::Arc<[crate::path::RelPath]>,
}

/// The sorted document-tier paths of `store`'s index.
fn doc_paths_of(store: &Store) -> std::sync::Arc<[crate::path::RelPath]> {
    let mut paths: Vec<crate::path::RelPath> = store.index.doc_files.keys().cloned().collect();
    paths.sort_unstable();
    paths.into()
}

impl MapCache {
    /// Build the read stack's map over `store`, charging decoded L1s against `budget_bytes`
    /// (`0` = unbounded).
    ///
    /// Unlike the whole-corpus map this replaces, `build` decodes NO L1 blobs: it projects the
    /// index into [`l1_cache::FileIndexView`] and leaves the outlines to be faulted in on demand.
    /// The read-only projections (`calls` / `impls`) are not built here either: they are the
    /// last-resort fallback for a session that can reach no Fjall index, built lazily on first use
    /// and charged against the same budget.
    pub(crate) fn build(store: &Store, budget_bytes: u64) -> Self {
        let files = l1_cache::FileIndexView::build(store);
        let l1 = Arc::new(l1_cache::L1Cache::from_store(store, budget_bytes));
        Self {
            files,
            l1,
            calls: Default::default(),
            impls: Default::default(),
            terms: Default::default(),
            doc_paths: doc_paths_of(store),
        }
    }

    /// An empty map cache: the placeholder a `serve` boots with while the real [`build`](Self::build)
    /// runs in the background (see [`crate::mcp::background::spawn_cache_warm`]). Deferring the index
    /// projection off the startup path is what lets the MCP `initialize`/`tools/list` handshake
    /// answer immediately. Cache-reading tools await [`ServerState::await_cache_ready`](crate::mcp::ServerState::await_cache_ready) before
    /// reading, so they observe the built map, never this placeholder.
    pub(crate) fn empty() -> Self {
        Self {
            files: l1_cache::FileIndexView::default(),
            l1: Arc::new(l1_cache::L1Cache::placeholder()),
            calls: Default::default(),
            impls: Default::default(),
            terms: Default::default(),
            doc_paths: Default::default(),
        }
    }

    /// Build a cache over a synthetic, blob-less corpus. Test-only seam: hand-built outlines that
    /// no scan produced have no store to be read back from, so they are seeded straight into the
    /// L1 cache.
    #[cfg(test)]
    pub(crate) fn from_synthetic(files: Vec<(crate::path::RelPath, FileMapL1)>) -> Self {
        let l1 = Arc::new(l1_cache::L1Cache::new(std::path::PathBuf::new(), 0));
        let mut metas: Vec<(crate::path::RelPath, l1_cache::FileMeta)> = Vec::with_capacity(files.len());
        for (path, map) in files {
            let hash_hex = format!("synthetic-{}", path.to_str_lossy());
            let meta = l1_cache::FileMeta {
                hash_hex: hash_hex.as_str().into(),
                language: map.language.as_str().into(),
                size_bytes: map.size_bytes,
                mtime_ns: 0,
                extract_epoch: map.extract_epoch,
            };
            l1.seed(&hash_hex, Arc::new(map));
            metas.push((path, meta));
        }
        Self {
            files: l1_cache::FileIndexView::from_pairs(metas),
            l1,
            calls: Default::default(),
            impls: Default::default(),
            terms: Default::default(),
            doc_paths: Default::default(),
        }
    }

    /// The in-RAM call projection, built on first use. Only reached by a session with no reachable
    /// Fjall index (see [`calls`](Self::calls)); a build that hits the byte budget is truncated and
    /// reports it through [`projections_capped`](Self::projections_capped).
    pub(crate) fn calls_projection(&self) -> &helpers_calls::InRamCallIndex {
        self.calls.get_or_init(|| {
            helpers_calls::InRamCallIndex::build(&self.files, self.l1.blobs_dir(), self.l1.budget_bytes())
        })
    }

    /// The in-RAM implementation projection, built on first use. See
    /// [`calls_projection`](Self::calls_projection).
    pub(crate) fn impls_projection(&self) -> &helpers_impls::InRamImplIndex {
        self.impls
            .get_or_init(|| helpers_impls::InRamImplIndex::build(&self.files, &self.l1, self.l1.budget_bytes()))
    }

    /// Whether either projection has been built. Test seam for the memory bound: a daemon-backed
    /// session must answer its reference reads without ever allocating one.
    #[cfg(all(test, feature = "comms", any(unix, windows)))]
    pub(crate) fn projections_built(&self) -> bool {
        self.calls.get().is_some() || self.impls.get().is_some()
    }

    /// True when a projection this session has ALREADY built hit its byte cap, so the answers it
    /// backed are incomplete. Surfaced through [`ServerState::lifecycle_notice`] rather than
    /// silently degrading a result to "no matches". `false` until a projection exists, which is why
    /// the handlers that fall back to one read the notice AFTER running the scan.
    pub(crate) fn projections_capped(&self) -> bool {
        self.calls.get().is_some_and(helpers_calls::InRamCallIndex::capped)
            || self.impls.get().is_some_and(helpers_impls::InRamImplIndex::capped)
    }

    /// Number of indexed files this map covers.
    pub(crate) fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether `path` is one of the indexed files. Answered from the symbol-free view, so it costs
    /// no blob read.
    pub(crate) fn contains(&self, path: &crate::path::RelPath) -> bool {
        self.files.contains(path)
    }

    /// Whether `path` is a tracked document (non-code file in the document tier).
    pub(crate) fn contains_doc(&self, path: &crate::path::RelPath) -> bool {
        self.doc_paths.binary_search(path).is_ok()
    }

    /// What the index recorded about `path`'s content, without faulting in its outline.
    pub(crate) fn meta(&self, path: &crate::path::RelPath) -> Option<&l1_cache::FileMeta> {
        self.files.get(path)
    }

    /// The language recorded for `path` at scan time, without faulting in its outline.
    pub(crate) fn language_of(&self, path: &crate::path::RelPath) -> Option<&str> {
        self.files.get(path).map(|meta| &*meta.language)
    }

    /// Every indexed path, in sorted order.
    pub(crate) fn paths(&self) -> impl Iterator<Item = &crate::path::RelPath> {
        self.files.keys()
    }

    /// Every indexed path with its symbol-free metadata, in sorted order. The whole surface a
    /// path/language/size filter needs, with no L1 decode anywhere.
    pub(crate) fn file_metas(&self) -> impl Iterator<Item = (&crate::path::RelPath, &l1_cache::FileMeta)> {
        self.files.iter()
    }

    /// The decoded outline for one file — the point-lookup path. A hit is a pointer clone; a miss
    /// is one blob read, which is exactly what the old whole-corpus build paid per file up front.
    pub(crate) fn get(&self, path: &crate::path::RelPath) -> Option<Arc<FileMapL1>> {
        let meta = self.files.get(path)?;
        self.l1.load(&meta.hash_hex, meta.extract_epoch)
    }

    /// The decoded outline of the content `hash_hex`, whichever path holds it. Outlines are keyed
    /// by content, so this is the outline of an index entry however old this snapshot's file view
    /// is next to the store the entry came from. `extract_epoch` is that entry's epoch, so an
    /// outline cached before a refresh rewrote the blob is not served for it.
    pub(crate) fn outline_of_content(&self, hash_hex: &str, extract_epoch: u16) -> Option<Arc<FileMapL1>> {
        self.l1.load(hash_hex, extract_epoch)
    }

    /// The term index, building it from one corpus stream on first use. Later calls (and the
    /// snapshots derived by [`with_delta`](Self::with_delta)) reuse it.
    pub(crate) fn terms(&self) -> &crate::engine::term_index::TermIndex {
        self.terms.get_or_init(|| {
            Arc::new(crate::engine::term_index::TermIndex::build(|visit| {
                self.for_each(|path, l1| visit(path, l1));
            }))
        })
    }

    /// Stream every `(path, outline)` in path order, stopping early when `f` returns `false`.
    ///
    /// The whole-corpus read primitive. Consumers project into a compact structure and drop the
    /// outline, so the corpus is never simultaneously materialised — the invariant the old
    /// `for (path, l1) in &cache.by_path` loop violated by construction.
    pub(crate) fn for_each_while<F>(&self, f: F)
    where
        F: FnMut(&crate::path::RelPath, &FileMapL1) -> bool,
    {
        l1_cache::stream_while(&self.files, &self.l1, f);
    }

    /// [`for_each_while`](Self::for_each_while) for a consumer that always runs to completion.
    pub(crate) fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(&crate::path::RelPath, &FileMapL1),
    {
        self.for_each_while(|path, l1| {
            f(path, l1);
            true
        });
    }

    /// The L1 cache backing this map. The budget tests read its counters.
    #[cfg(test)]
    pub(crate) fn l1_cache(&self) -> &l1_cache::L1Cache {
        &self.l1
    }

    /// Incrementally derive a fresh cache from `self` for a **scoped** (watcher) rescan: patch the
    /// file view for the changed paths only and carry the L1 cache forward by refcount. This avoids
    /// `build`'s whole-corpus index projection on every debounced batch, which is what pegged
    /// multi-core CPU on gitignored / nested-`.basemind` churn (issue #33).
    ///
    /// Sharing the L1 cache across the two snapshots is safe because it is keyed by CONTENT HASH:
    /// an updated path's new view entry names a new hash, so a reader still holding `self` keeps
    /// resolving the old hash and cannot observe the newer content. An extract-epoch refresh keeps
    /// the hash and rewrites the blob; the new view entry names the newer epoch, which the cache
    /// does not answer from an outline decoded under an older one.
    ///
    /// The lazy `calls`/`impls` projections start empty on the derived snapshot, so they are
    /// rebuilt from the post-rescan file view if ever needed and cannot drift out of sync.
    ///
    /// The outlines of the content `updated` and `removed` paths named before are dropped from the
    /// shared L1 cache once no path of the new view names them (see
    /// [`L1Cache::forget_superseded`](l1_cache::L1Cache::forget_superseded)).
    pub(crate) fn with_delta(
        &self,
        store: &Store,
        updated: &[crate::path::RelPath],
        removed: &[crate::path::RelPath],
    ) -> Self {
        let superseded: ahash::AHashSet<&str> = updated
            .iter()
            .chain(removed)
            .filter_map(|path| self.files.get(path))
            .map(|meta| &*meta.hash_hex)
            .collect();
        // An extract-epoch refresh rewrote the blob under the same hash: the outline decoded from
        // the replaced blob must not be served for it, whoever still names the hash.
        for path in updated {
            if let (Some(old), Some(entry)) = (self.files.get(path), store.index.files.get(path))
                && *old.hash_hex == *entry.hash_hex
                && old.extract_epoch != entry.extract_epoch
            {
                self.l1.forget(&old.hash_hex);
            }
        }
        let next = Self {
            files: self.files.with_delta(store, updated, removed),
            l1: Arc::clone(&self.l1),
            // Fresh and empty: a derived snapshot rebuilds its projections lazily, so they can never
            // describe the pre-rescan corpus.
            calls: Default::default(),
            impls: Default::default(),
            terms: Default::default(),
            doc_paths: doc_paths_of(store),
        };
        self.l1.forget_superseded(superseded, &next.files);
        if let Some(terms) = self.terms.get() {
            let _ = next
                .terms
                .set(Arc::new(terms.patched(updated, removed, |p| next.get(p))));
        }
        next
    }

    /// The map of `store` derived from this one, or `None` when it would map `store` exactly as
    /// this one does.
    ///
    /// A rescan publishes a new index; rebuilding the map over it started a fresh L1 cache, and the
    /// term-index warm that follows a build then decoded every outline of the repository again,
    /// however few files the scan changed. Instead the paths a [`build`](Self::build) would map
    /// differently are found by comparing the whole index against this map's view
    /// ([`FileIndexView::diff`](l1_cache::FileIndexView::diff)), and [`with_delta`](Self::with_delta)
    /// applies exactly those, so the result is the map `build` would produce, while unchanged
    /// outlines stay decoded. The diff is taken from the store, never from a scan's report of what it
    /// touched, so a path a report missed cannot leave the map stale.
    ///
    /// `None` is what keeps a no-op daemon scan from transiently doubling the map's residency. The
    /// boot placeholder, or a cache over other blob directories than `store` reads, is rebuilt.
    pub(crate) fn refreshed(&self, store: &Store, budget_bytes: u64) -> Option<Self> {
        if !self.l1.serves(store) {
            return Some(Self::build(store, budget_bytes));
        }
        let (updated, removed) = self.files.diff(store);
        let doc_paths = doc_paths_of(store);
        // A path left out of the view for a missing blob is re-probed by every diff; while its
        // blob is still missing the delta would map exactly what this map does.
        let unchanged = removed.is_empty()
            && doc_paths == self.doc_paths
            && updated.iter().all(|path| {
                !self.files.contains(path)
                    && store
                        .index
                        .files
                        .get(path)
                        .is_none_or(|entry| !l1_cache::has_blob(store, entry))
            });
        if unchanged {
            return None;
        }
        Some(self.with_delta(store, &updated, &removed))
    }
}

#[cfg(test)]
#[path = "map_cache_budget_tests.rs"]
mod map_cache_budget_tests;

#[cfg(test)]
#[path = "map_cache_delta_tests.rs"]
mod map_cache_delta_tests;
