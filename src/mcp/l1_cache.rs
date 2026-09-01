//! The bounded half of the MCP read stack: a symbol-free [`FileIndexView`] over every indexed
//! file plus a byte-charged, read-through [`L1Cache`] of decoded L1 blobs.
//!
//! Before this split the read stack held one `BTreeMap<RelPath, FileMapL1>` — every file's decoded
//! outline, for the lifetime of the process, times one entry per hot workspace in the daemon. That
//! is O(total symbols) resident and was the largest single structure in the process. The two types
//! here divide it along the line the consumers already fall on:
//!
//! * [`FileIndexView`] is O(files) and carries no symbols. Path membership, language and size
//!   filters, and `list_files`-style enumeration answer from it with no I/O at all.
//! * [`L1Cache`] is the only place a decoded [`FileMapL1`] lives. It is charged in BYTES, not in
//!   entries: measured L1 sizes span three orders of magnitude (~1.4 KB median against a 697 KB
//!   maximum), so an entry-count LRU is the same unbounded structure with a different constant.
//!
//! **A miss changes latency, never an answer.** Every miss is one `read_l1_by_hex` against the same
//! content-addressed blob `MapCache::build` reads at boot, so a cold cache and a warm one return
//! byte-identical data. `tests/map_cache_equivalence.rs` pins that by running the tool surface at a
//! 1 MiB budget and unbounded and comparing the responses byte for byte.
//!
//! A miss reads a blob, so a tool that streams a cold corpus now performs I/O where it previously
//! walked RAM. That is a deliberate trade and not a new hazard: the read handlers already do
//! blocking work inline (`workspace_grep` reads every candidate file from the same context), and on
//! any repo whose outlines fit the budget — the default 256 MiB covers roughly 180k files — the
//! stream is served entirely from RAM after the first pass.
//!
//! The LRU is keyed by CONTENT HASH rather than by path, which is what makes one cache safely
//! shareable across cache snapshots: an incremental rescan publishes a new [`FileIndexView`] whose
//! changed paths point at new hashes, so a reader still holding the previous snapshot keeps
//! resolving its own (older) hashes and can never observe the newer file. No invalidation pass, and
//! two paths with identical content share one entry.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lru::LruCache;

use crate::extract::FileMapL1;
use crate::path::RelPath;

/// How many files one streaming pass resolves before handing them to the consumer.
///
/// The chunk is what bounds a whole-corpus iteration: at most this many decoded L1s are live at
/// once regardless of corpus size. It is also the unit of parallel decode, so it must be large
/// enough to amortise a rayon fan-out and small enough that the live set stays trivial next to the
/// LRU budget (256 files × 1.4 KB ≈ 350 KB typical).
const STREAM_CHUNK: usize = 256;

/// Symbol-free per-file metadata — the O(files) half of the old whole-corpus map.
///
/// Every field is already in `store.index.files`; they are copied into the view so a read tool can
/// answer membership / language / size questions without taking the store lock.
pub(crate) struct FileMeta {
    /// Content hash of the source file — the [`L1Cache`] key and the blob name to read on a miss.
    pub(crate) hash_hex: Box<str>,
    pub(crate) language: Box<str>,
    pub(crate) size_bytes: u64,
}

/// Sorted view of every indexed file that has a readable L1 blob.
///
/// Sorted (a `BTreeMap`) because `list_files`, `find_files` and the completion surface all document
/// path order as their pagination order; an unordered view would make cursors non-deterministic.
#[derive(Default)]
pub(crate) struct FileIndexView {
    files: BTreeMap<RelPath, FileMeta>,
}

impl FileIndexView {
    /// Project the store's index into the view, keeping only files whose L1 blob is actually
    /// present.
    ///
    /// The existence probe is what preserves the previous semantics exactly: the old build
    /// `filter_map`ped away any file whose blob failed to read, so a blob-less index entry was
    /// invisible to every consumer. Probing costs one `stat` per file instead of the full read +
    /// msgpack decode the old build paid, so this is strictly cheaper than what it replaces.
    pub(crate) fn build(store: &crate::store::Store) -> Self {
        use rayon::prelude::*;
        let files: BTreeMap<RelPath, FileMeta> = store
            .index
            .files
            .par_iter()
            .filter_map(|(path, entry)| {
                store
                    .blob_path_fm_hex(&entry.hash_hex)
                    .is_file()
                    .then(|| (path.clone(), FileMeta::from(entry)))
            })
            .collect();
        Self { files }
    }

    pub(crate) fn len(&self) -> usize {
        self.files.len()
    }

    pub(crate) fn get(&self, path: &RelPath) -> Option<&FileMeta> {
        self.files.get(path)
    }

    // Membership is asked only by the governance audit (`memory`) and the codegraph document lane
    // (`documents`); gated rather than blanket-allowed so a build that drops both keeps no dead code.
    #[cfg(any(test, feature = "documents", feature = "memory"))]
    pub(crate) fn contains(&self, path: &RelPath) -> bool {
        self.files.contains_key(path)
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &RelPath> {
        self.files.keys()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&RelPath, &FileMeta)> {
        self.files.iter()
    }

    /// Build a view directly from `(path, meta)` pairs — the seam a unit test uses to stand up a
    /// synthetic corpus with no store and no blobs behind it.
    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: impl IntoIterator<Item = (RelPath, FileMeta)>) -> Self {
        Self {
            files: pairs.into_iter().collect(),
        }
    }

    /// Derive the next view from this one for a scoped (watcher) rescan: drop `removed`, re-project
    /// `updated` from the reopened store, leave everything else untouched.
    pub(crate) fn with_delta(&self, store: &crate::store::Store, updated: &[RelPath], removed: &[RelPath]) -> Self {
        let mut files: BTreeMap<RelPath, FileMeta> = self
            .files
            .iter()
            .map(|(path, meta)| (path.clone(), meta.clone()))
            .collect();
        for path in removed {
            files.remove(path);
        }
        for path in updated {
            match store.index.files.get(path) {
                Some(entry) if store.blob_path_fm_hex(&entry.hash_hex).is_file() => {
                    files.insert(path.clone(), FileMeta::from(entry));
                }
                _ => {
                    files.remove(path);
                }
            }
        }
        Self { files }
    }
}

impl From<&crate::store::FileEntry> for FileMeta {
    fn from(entry: &crate::store::FileEntry) -> Self {
        Self {
            hash_hex: entry.hash_hex.as_str().into(),
            language: entry.language.as_str().into(),
            size_bytes: entry.size_bytes,
        }
    }
}

impl Clone for FileMeta {
    fn clone(&self) -> Self {
        Self {
            hash_hex: self.hash_hex.clone(),
            language: self.language.clone(),
            size_bytes: self.size_bytes,
        }
    }
}

/// LRU state behind the cache mutex: the map plus the running byte charge over its values.
struct Charged {
    lru: LruCache<Box<str>, Arc<FileMapL1>>,
    charged: u64,
}

/// Byte-charged, read-through cache of decoded L1 outlines, keyed by content hash.
///
/// Shared (behind an [`Arc`]) by every [`MapCache`](super::MapCache) snapshot derived from one
/// another, so a watcher rescan does not throw away a warm cache or transiently double its
/// residency.
pub(crate) struct L1Cache {
    /// Global content-addressed blob directory. Reading a blob needs nothing but this path, which
    /// is why a miss can be served without ever taking the store lock.
    blobs_dir: PathBuf,
    /// Byte ceiling for decoded values; `0` means unbounded (the pre-split behaviour).
    budget_bytes: u64,
    inner: Mutex<Charged>,
    /// Cumulative miss count. Diagnostic only — a test asserts a small budget really does force
    /// misses, so an "equivalent output" claim is not silently vacuous.
    misses: AtomicU64,
}

impl L1Cache {
    pub(crate) fn new(blobs_dir: PathBuf, budget_bytes: u64) -> Self {
        Self {
            blobs_dir,
            budget_bytes,
            inner: Mutex::new(Charged {
                lru: LruCache::unbounded(),
                charged: 0,
            }),
            misses: AtomicU64::new(0),
        }
    }

    /// Cache for the boot placeholder, which has no files to read.
    pub(crate) fn placeholder() -> Self {
        Self::new(PathBuf::new(), 0)
    }

    pub(crate) fn budget_bytes(&self) -> u64 {
        self.budget_bytes
    }

    /// Bytes currently charged to the cache. Test surface for the budget assertions.
    #[cfg(test)]
    pub(crate) fn charged_bytes(&self) -> u64 {
        self.lock().charged
    }

    #[cfg(test)]
    pub(crate) fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// The decoded L1 for `hash_hex`, from the cache or from the blob.
    ///
    /// `None` means the blob is gone or undecodable — the same outcome the old whole-corpus build
    /// produced by dropping the file from the map.
    pub(crate) fn load(&self, hash_hex: &str) -> Option<Arc<FileMapL1>> {
        if let Some(hit) = self.lock().lru.get(hash_hex).cloned() {
            return Some(hit);
        }
        let decoded = Arc::new(self.read_blob(hash_hex)?);
        self.admit(hash_hex, &decoded);
        Some(decoded)
    }

    /// Resolve a whole chunk of hashes with one lock acquisition for the hits and a parallel decode
    /// for the misses.
    ///
    /// The hot case — a repo small enough to sit entirely inside the budget — never leaves the
    /// first lock, so streaming a warm corpus costs one mutex round trip per chunk rather than the
    /// per-file rayon fan-out an unconditional parallel decode would pay.
    fn load_chunk(&self, hashes: &[&str]) -> Vec<Option<Arc<FileMapL1>>> {
        use rayon::prelude::*;
        let mut slots: Vec<Option<Arc<FileMapL1>>> = vec![None; hashes.len()];
        let mut misses: Vec<usize> = Vec::new();
        {
            let mut guard = self.lock();
            for (i, hash) in hashes.iter().enumerate() {
                match guard.lru.get(*hash) {
                    Some(hit) => slots[i] = Some(Arc::clone(hit)),
                    None => misses.push(i),
                }
            }
        }
        if misses.is_empty() {
            return slots;
        }
        let decoded: Vec<(usize, Option<Arc<FileMapL1>>)> = misses
            .par_iter()
            .map(|&i| (i, self.read_blob(hashes[i]).map(Arc::new)))
            .collect();
        for (i, value) in decoded {
            if let Some(value) = value {
                self.admit(hashes[i], &value);
                slots[i] = Some(value);
            }
        }
        slots
    }

    fn read_blob(&self, hash_hex: &str) -> Option<FileMapL1> {
        self.misses.fetch_add(1, Ordering::Relaxed);
        crate::store_blob::read_l1_blob_in(&self.blobs_dir, hash_hex)
            .ok()
            .flatten()
    }

    /// Insert `value` and evict least-recently-used entries until the charge fits the budget.
    ///
    /// The newest entry is never evicted even when it alone exceeds the budget: a single 697 KB L1
    /// under a 1 MiB budget must still be returnable, or the "a miss only costs latency" contract
    /// would break into "a miss can cost the answer".
    fn admit(&self, hash_hex: &str, value: &Arc<FileMapL1>) {
        let bytes = l1_heap_bytes(value);
        let mut guard = self.lock();
        if let Some(previous) = guard.lru.put(hash_hex.into(), Arc::clone(value)) {
            guard.charged = guard.charged.saturating_sub(l1_heap_bytes(&previous));
        }
        guard.charged = guard.charged.saturating_add(bytes);
        if self.budget_bytes == 0 {
            return;
        }
        while guard.charged > self.budget_bytes && guard.lru.len() > 1 {
            let Some((_, evicted)) = guard.lru.pop_lru() else {
                break;
            };
            guard.charged = guard.charged.saturating_sub(l1_heap_bytes(&evicted));
        }
    }

    /// Insert a value that has no blob behind it — the synthetic-corpus counterpart to
    /// [`FileIndexView::from_pairs`]. Charged like any other entry, so a test can still observe
    /// eviction.
    #[cfg(test)]
    pub(crate) fn seed(&self, hash_hex: &str, value: Arc<FileMapL1>) {
        self.admit(hash_hex, &value);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Charged> {
        // A poisoned cache costs at most a re-read: every value is reconstructible from its blob.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Stream `(path, l1)` for every file in `view`, in path order, calling `f` until it returns
/// `false`.
///
/// This is the whole-corpus read primitive. At most [`STREAM_CHUNK`] decoded L1s are live at any
/// instant, so a consumer that projects into a compact structure (which every whole-corpus consumer
/// does) never materialises the corpus. Files whose blob has vanished are skipped, exactly as the
/// old build's `filter_map` skipped them.
pub(crate) fn stream_while<F>(view: &FileIndexView, cache: &L1Cache, mut f: F)
where
    F: FnMut(&RelPath, &FileMapL1) -> bool,
{
    let mut paths: Vec<&RelPath> = Vec::with_capacity(STREAM_CHUNK);
    let mut hashes: Vec<&str> = Vec::with_capacity(STREAM_CHUNK);
    for (path, meta) in view.iter() {
        paths.push(path);
        hashes.push(&meta.hash_hex);
        if paths.len() == STREAM_CHUNK && !drain(cache, &mut paths, &mut hashes, &mut f) {
            return;
        }
    }
    drain(cache, &mut paths, &mut hashes, &mut f);
}

fn drain<F>(cache: &L1Cache, paths: &mut Vec<&RelPath>, hashes: &mut Vec<&str>, f: &mut F) -> bool
where
    F: FnMut(&RelPath, &FileMapL1) -> bool,
{
    let loaded = cache.load_chunk(hashes);
    let mut keep_going = true;
    for (path, value) in paths.iter().zip(loaded) {
        if let Some(value) = value
            && !f(path, &value)
        {
            keep_going = false;
            break;
        }
    }
    paths.clear();
    hashes.clear();
    keep_going
}

/// Approximate resident cost of one decoded L1, in bytes.
///
/// Approximate is the point: the budget shapes residency, and an exact `size_of_val` walk would
/// cost more than the eviction it informs. It counts every heap allocation the value owns (each
/// `Vec`'s element storage plus each owned `String`'s bytes), so it tracks the order-of-magnitude
/// spread between a 40-line source file and a generated one — which is the whole reason the budget
/// is in bytes rather than entries.
pub(crate) fn l1_heap_bytes(l1: &FileMapL1) -> u64 {
    let mut total = std::mem::size_of::<FileMapL1>() as u64;
    total += l1.language.len() as u64;
    total += (l1.symbols.len() * std::mem::size_of::<crate::extract::Symbol>()) as u64;
    for sym in &l1.symbols {
        total += sym.name.len() as u64;
        total += sym.signature.as_ref().map_or(0, String::len) as u64;
        total += (sym.decorators.len() * std::mem::size_of::<String>()) as u64;
        total += sym.decorators.iter().map(|d| d.len() as u64).sum::<u64>();
    }
    total += (l1.imports.len() * std::mem::size_of::<crate::extract::Import>()) as u64;
    for imp in &l1.imports {
        total += imp.module.as_ref().map_or(0, String::len) as u64;
        total += imp.raw.len() as u64;
    }
    total += (l1.implementations.len() * std::mem::size_of::<crate::extract::Implementation>()) as u64;
    for imp in &l1.implementations {
        total += (imp.trait_name.len() + imp.impl_type.len()) as u64;
    }
    total += (l1.rationale.len() * std::mem::size_of::<crate::extract::RationaleRecord>()) as u64;
    for rec in &l1.rationale {
        total += rec.text.len() as u64;
        total += (rec.citations.len() * std::mem::size_of::<String>()) as u64;
        total += rec.citations.iter().map(|c| c.len() as u64).sum::<u64>();
    }
    total
}

/// Resolve `[resources] max_map_cache_mb` to a byte budget. `0` stays `0` (unbounded).
pub(crate) fn budget_bytes_from(resources: &crate::config::ResourcesConfig) -> u64 {
    (resources.max_map_cache_mb as u64).saturating_mul(1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{Symbol, SymbolKind};

    fn l1_with(name: &str, padding: usize) -> FileMapL1 {
        FileMapL1 {
            schema_ver: crate::extract::SCHEMA_VER,
            language: "rust".to_string(),
            size_bytes: 1,
            had_errors: false,
            error_count: 0,
            symbols: vec![Symbol {
                name: name.to_string(),
                kind: SymbolKind::Function,
                start_byte: 0,
                end_byte: 1,
                start_row: 0,
                start_col: 0,
                signature: Some("x".repeat(padding)),
                decorators: Vec::new(),
            }],
            imports: Vec::new(),
            implementations: Vec::new(),
            rationale: Vec::new(),
        }
    }

    /// The charge must scale with a value's own heap, not with the entry count — the exact reason
    /// the budget is in bytes. A 100 KB signature must charge ~100 KB more than a 1-byte one.
    #[test]
    fn heap_bytes_tracks_payload_not_entry_count() {
        let small = l1_heap_bytes(&l1_with("a", 1));
        let large = l1_heap_bytes(&l1_with("a", 100_000));
        assert!(large > small + 99_000, "small={small} large={large}");
    }

    /// Eviction keeps the charge under budget, and the value just admitted always survives so a
    /// single oversized L1 is still returnable under a tiny budget.
    #[test]
    fn admit_evicts_to_budget_but_keeps_the_newest() {
        let cache = L1Cache::new(PathBuf::new(), 4096);
        for i in 0..64 {
            let value = Arc::new(l1_with(&format!("sym{i}"), 512));
            cache.admit(&format!("{i:064x}"), &value);
        }
        assert!(cache.charged_bytes() <= 4096, "charged={}", cache.charged_bytes());
        let huge = Arc::new(l1_with("huge", 1_000_000));
        cache.admit("ff", &huge);
        assert!(cache.load("ff").is_some(), "the newest entry is never evicted");
    }

    /// `0` disables eviction entirely — the documented "unbounded" sentinel the equivalence test
    /// compares against.
    #[test]
    fn zero_budget_never_evicts() {
        let cache = L1Cache::new(PathBuf::new(), 0);
        for i in 0..32 {
            let value = Arc::new(l1_with(&format!("sym{i}"), 4096));
            cache.admit(&format!("{i:064x}"), &value);
        }
        assert!(cache.charged_bytes() > 32 * 4096);
    }
}
