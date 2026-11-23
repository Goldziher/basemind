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
//!
//! The one write that keeps a hash is an extract-epoch refresh: a newer extractor rewrites the blob
//! of unchanged content in place. [`FileMeta`] therefore carries the entry's epoch, and a cached
//! outline decoded under an older epoch is a miss for it, so the refreshed blob is read again while
//! a reader still holding the previous snapshot keeps its hit.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lru::LruCache;

use crate::engine::paged_map::PagedMap;
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
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FileMeta {
    /// Content hash of the source file — the [`L1Cache`] key and the blob name to read on a miss.
    pub(crate) hash_hex: Box<str>,
    pub(crate) language: Box<str>,
    pub(crate) size_bytes: u64,
    /// File mtime in nanoseconds as indexed (0 = unknown); with `size_bytes` the stamp grep holds the
    /// live file to before trusting the content's bloom.
    pub(crate) mtime_ns: i64,
    /// The extract epoch the index entry names ([`crate::store::FileEntry::extract_epoch`]). A
    /// cached outline older than it was decoded before a refresh rewrote the blob under this hash.
    pub(crate) extract_epoch: u16,
    /// The key the entry's symbol and call rows were written under
    /// ([`crate::store::FileEntry::content_key`]).
    pub(crate) content_key: Option<crate::index::keys::ContentKey>,
    /// Lines in the file as indexed ([`crate::store::FileEntry::lines`]); 0 for a file the code
    /// map does not hold.
    pub(crate) lines: u32,
}

/// Sorted view of every indexed file.
///
/// Sorted because `list_files`, `find_files` and the completion surface all document path order as
/// their pagination order; an unordered view would make cursors non-deterministic. Paged
/// ([`PagedMap`]) so that the view a watcher batch derives shares every page the batch did not
/// touch with this one.
#[derive(Default)]
pub(crate) struct FileIndexView {
    files: PagedMap<RelPath, FileMeta>,
}

impl FileIndexView {
    /// Project the store's index into the view. The file map is trusted: no blob is probed, so a
    /// build over the whole index costs no filesystem call per file. A blob found missing when an
    /// outline is faulted in (see [`L1Cache`]) is served as absent and logged as a hint to rescan.
    pub(crate) fn build(store: &crate::store::Store) -> Self {
        use rayon::prelude::*;
        let files: Vec<(RelPath, FileMeta)> = store
            .index
            .files
            .par_iter()
            .map(|(path, entry)| (path.clone(), FileMeta::from(entry)))
            .collect();
        Self {
            files: PagedMap::from_unsorted(files),
        }
    }

    /// The paths a [`build`](Self::build) over `store` would map differently from this view:
    /// `updated` are in `store`'s index and absent here or projected differently, `removed` are here and no longer in the index.
    ///
    /// [`with_delta`](Self::with_delta) over exactly these paths yields the view `build` would. The
    /// comparison allocates nothing for an unchanged path, so a refresh whose scan changed a
    /// handful of files costs one pass over the index rather than a whole projection.
    pub(crate) fn diff(&self, store: &crate::store::Store) -> (Vec<RelPath>, Vec<RelPath>) {
        let updated = store
            .index
            .files
            .iter()
            .filter(|(path, entry)| !self.files.get(*path).is_some_and(|meta| meta.projects(entry)))
            .map(|(path, _)| path.clone())
            .collect();
        let removed = self
            .files
            .keys()
            .filter(|path| !store.index.files.contains_key(*path))
            .cloned()
            .collect();
        (updated, removed)
    }

    pub(crate) fn len(&self) -> usize {
        self.files.len()
    }

    pub(crate) fn get(&self, path: &RelPath) -> Option<&FileMeta> {
        self.files.get(path)
    }

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
            files: PagedMap::from_unsorted(pairs.into_iter().collect()),
        }
    }

    /// The pages and the entries of this view that it does not share with `base`.
    #[cfg(test)]
    pub(crate) fn unshared_with(&self, base: &Self) -> (usize, usize) {
        self.files.unshared_with(&base.files)
    }

    /// Derive the next view from this one for a scoped (watcher) rescan: drop `removed`, re-project
    /// `updated` from the reopened store, and share every other entry with this view.
    pub(crate) fn with_delta(&self, store: &crate::store::Store, updated: &[RelPath], removed: &[RelPath]) -> Self {
        let removals = removed.iter().map(|path| (path.clone(), None));
        let updates = updated.iter().map(|path| {
            let meta = store.index.files.get(path).map(FileMeta::from);
            (path.clone(), meta)
        });
        Self {
            files: self.files.patched(removals.chain(updates)),
        }
    }
}

impl FileMeta {
    /// Whether `entry` projects to this meta. Field by field, so no meta is built to compare.
    fn projects(&self, entry: &crate::store::FileEntry) -> bool {
        *self.hash_hex == *entry.hash_hex
            && *self.language == *entry.language
            && self.size_bytes == entry.size_bytes
            && self.mtime_ns == entry.mtime
            && self.extract_epoch == entry.extract_epoch
            && self.content_key == entry.content_key
            && self.lines == entry.lines
    }
}

impl From<&crate::store::FileEntry> for FileMeta {
    fn from(entry: &crate::store::FileEntry) -> Self {
        Self {
            hash_hex: entry.hash_hex.as_str().into(),
            language: entry.language.as_str().into(),
            size_bytes: entry.size_bytes,
            mtime_ns: entry.mtime,
            extract_epoch: entry.extract_epoch,
            content_key: entry.content_key,
            lines: entry.lines,
        }
    }
}

impl Clone for FileMeta {
    fn clone(&self) -> Self {
        Self {
            hash_hex: self.hash_hex.clone(),
            language: self.language.clone(),
            size_bytes: self.size_bytes,
            mtime_ns: self.mtime_ns,
            extract_epoch: self.extract_epoch,
            content_key: self.content_key,
            lines: self.lines,
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
/// Shared (behind an [`Arc`]) by every [`MapCache`](crate::engine::MapCache) snapshot derived from one
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

    /// The global content-addressed blob directory this cache reads from.
    pub(crate) fn from_store(store: &crate::store::Store, budget_bytes: u64) -> Self {
        Self::new(store.blobs_dir.clone(), budget_bytes)
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
    /// `min_epoch` is the extract epoch of the index entry naming `hash_hex`: a cached value decoded
    /// under an older epoch predates the refresh that rewrote the blob, so it is read again.
    ///
    /// `None` means the blob is gone or undecodable — the same outcome the old whole-corpus build
    /// produced by dropping the file from the map.
    pub(crate) fn load(&self, hash_hex: &str, min_epoch: u16) -> Option<Arc<FileMapL1>> {
        let hit = self
            .lock()
            .lru
            .get(hash_hex)
            .filter(|hit| hit.extract_epoch >= min_epoch)
            .cloned();
        if let Some(hit) = hit {
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
    fn load_chunk(&self, metas: &[&FileMeta]) -> Vec<Option<Arc<FileMapL1>>> {
        use rayon::prelude::*;
        let mut slots: Vec<Option<Arc<FileMapL1>>> = vec![None; metas.len()];
        let mut misses: Vec<usize> = Vec::new();
        {
            let mut guard = self.lock();
            for (i, meta) in metas.iter().enumerate() {
                let hit = guard
                    .lru
                    .get(&*meta.hash_hex)
                    .filter(|hit| hit.extract_epoch >= meta.extract_epoch);
                match hit {
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
            .map(|&i| (i, self.read_blob(&metas[i].hash_hex).map(Arc::new)))
            .collect();
        for (i, value) in decoded {
            if let Some(value) = value {
                self.admit(&metas[i].hash_hex, &value);
                slots[i] = Some(value);
            }
        }
        slots
    }

    fn read_blob(&self, hash_hex: &str) -> Option<FileMapL1> {
        self.misses.fetch_add(1, Ordering::Relaxed);
        match crate::store_blob::read_l1_blob_in(&self.blobs_dir, hash_hex) {
            Ok(Some(map)) => Some(map),
            Ok(None) => {
                tracing::warn!(
                    hash = hash_hex,
                    "blob for an indexed file is missing; rescan to rebuild it"
                );
                None
            }
            Err(error) => self.recover_unreadable(hash_hex, &error),
        }
    }

    /// Log why a blob the file map names could not be served, and remove a damaged one. The
    /// caller's refusal says the same to the agent (see `tool_contract::not_indexed`). Returns the
    /// blob when a writer replaced the damaged file with a good one in the meantime.
    fn recover_unreadable(&self, hash_hex: &str, error: &crate::store::StoreError) -> Option<FileMapL1> {
        if is_damage(error) && self.blobs_dir == crate::store::global_blobs_dir() {
            // A damaged blob (torn by a power loss: blobs are not fsynced) would be trusted for
            // existing by every later scan. With the blob gone the next scan of the file finds
            // nothing for its hash and extracts it again. Only the global store is repaired this
            // way; a published base generation is immutable.
            //
            // A scan may rename a good blob over the damaged one at any moment, and an unlink by
            // path would take that one. So the file is identified, read once more, and removed
            // only if that read finds damage too and it is still the file that was read.
            let path = crate::store_blob_layout::locate_blob(&self.blobs_dir, hash_hex, ".fm.msgpack");
            let failed = BlobIdentity::of(&path);
            match crate::store_blob::read_l1_blob_in(&self.blobs_dir, hash_hex) {
                Ok(replaced) => return replaced,
                Err(again) if is_damage(&again) => {
                    let dropped = remove_if_still(&path, failed);
                    self.forget(hash_hex);
                    tracing::warn!(hash = hash_hex, %error, dropped, "damaged blob for an indexed file; rescan to rebuild it");
                }
                // The file identified may be a good one a scan has just put there, and a read
                // that failed for another reason says nothing against it.
                Err(again) => {
                    tracing::warn!(hash = hash_hex, error = %again, "blob for an indexed file could not be read");
                }
            }
        } else {
            tracing::warn!(hash = hash_hex, %error, "blob for an indexed file could not be read");
        }
        None
    }

    /// Whether this cache reads the blob directory `store` reads. A reopened store that names
    /// another needs a cache of its own.
    pub(crate) fn serves(&self, store: &crate::store::Store) -> bool {
        self.blobs_dir == store.blobs_dir
    }

    /// Drop the outlines of the `superseded` hashes no path of `next` still names: content a rescan
    /// replaced or removed. Kept, they would hold the budget until evicted while no current path
    /// can reach them. A reader still on the previous snapshot reads such a blob again on a miss,
    /// which costs latency, never an answer.
    pub(crate) fn forget_superseded(&self, mut superseded: ahash::AHashSet<&str>, next: &FileIndexView) {
        for meta in next.files.values() {
            if superseded.is_empty() {
                return;
            }
            superseded.remove(&*meta.hash_hex);
        }
        for hash_hex in superseded {
            self.forget(hash_hex);
        }
    }

    /// Whether an outline of `hash_hex` is cached.
    #[cfg(test)]
    pub(crate) fn holds(&self, hash_hex: &str) -> bool {
        self.lock().lru.contains(hash_hex)
    }

    /// Drop the cached value of `hash_hex`, if any, so the next read decodes its blob again.
    pub(crate) fn forget(&self, hash_hex: &str) {
        let mut guard = self.lock();
        if let Some(previous) = guard.lru.pop(hash_hex) {
            guard.charged = guard.charged.saturating_sub(l1_heap_bytes(&previous));
        }
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

/// What tells one file at a path from another renamed over it: its inode where there is one, and
/// its length and modification time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BlobIdentity {
    inode: u64,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl BlobIdentity {
    fn of(path: &std::path::Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&meta);
        #[cfg(not(unix))]
        let inode = 0;
        Some(Self {
            inode,
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

/// Remove the file at `path` if it is still the file `expected` identified. Returns whether it
/// was removed.
fn remove_if_still(path: &std::path::Path, expected: Option<BlobIdentity>) -> bool {
    expected.is_some() && BlobIdentity::of(path) == expected && std::fs::remove_file(path).is_ok()
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
    let mut metas: Vec<&FileMeta> = Vec::with_capacity(STREAM_CHUNK);
    for (path, meta) in view.iter() {
        paths.push(path);
        metas.push(meta);
        if paths.len() == STREAM_CHUNK && !drain(cache, &mut paths, &mut metas, &mut f) {
            return;
        }
    }
    drain(cache, &mut paths, &mut metas, &mut f);
}

fn drain<F>(cache: &L1Cache, paths: &mut Vec<&RelPath>, metas: &mut Vec<&FileMeta>, f: &mut F) -> bool
where
    F: FnMut(&RelPath, &FileMapL1) -> bool,
{
    let loaded = cache.load_chunk(metas);
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
    metas.clear();
    keep_going
}

/// Approximate resident cost of one decoded L1 held by the cache, in bytes.
///
/// Approximate is the point: the budget shapes residency, and an exact walk of the allocator's
/// books would cost more than the eviction it informs. It counts every heap allocation the value
/// owns at its capacity, rounded up to the allocator's size class, plus what the cache itself
/// spends on the entry (the `Arc` header, the LRU node and its content-hash key). Counting lengths
/// alone, as it once did, charged a short name or signature a few bytes for an allocation that
/// occupies a whole size class, and charged nothing for the entry: the budget then admitted a heap
/// noticeably larger than itself. It still tracks the order-of-magnitude spread between a 40-line
/// source file and a generated one, which is why the budget is in bytes rather than entries.
pub(crate) fn l1_heap_bytes(l1: &FileMapL1) -> u64 {
    use std::mem::size_of;
    let strings = |items: &[String]| items.iter().map(|s| alloc_bytes(s.capacity())).sum::<u64>();
    let mut total = ENTRY_OVERHEAD_BYTES + alloc_bytes(size_of::<FileMapL1>()) + alloc_bytes(l1.language.capacity());
    total += alloc_bytes(l1.symbols.capacity() * size_of::<crate::extract::Symbol>());
    for sym in &l1.symbols {
        total += alloc_bytes(sym.name.capacity());
        total += sym.signature.as_ref().map_or(0, |sig| alloc_bytes(sig.capacity()));
        total += alloc_bytes(sym.decorators.capacity() * size_of::<String>()) + strings(&sym.decorators);
    }
    total += alloc_bytes(l1.imports.capacity() * size_of::<crate::extract::Import>());
    for imp in &l1.imports {
        total += imp.module.as_ref().map_or(0, |module| alloc_bytes(module.capacity()));
        total += alloc_bytes(imp.raw.capacity());
    }
    total += alloc_bytes(l1.implementations.capacity() * size_of::<crate::extract::Implementation>());
    for imp in &l1.implementations {
        total += alloc_bytes(imp.trait_name.capacity()) + alloc_bytes(imp.impl_type.capacity());
    }
    total += alloc_bytes(l1.rationale.capacity() * size_of::<crate::extract::RationaleRecord>());
    for rec in &l1.rationale {
        total += alloc_bytes(rec.text.capacity());
        total += alloc_bytes(rec.citations.capacity() * size_of::<String>()) + strings(&rec.citations);
    }
    total
}

/// What the cache spends on one entry besides the outline: the `Arc` header, the LRU's node and
/// hash slot, and the boxed 64-hex content-hash key.
const ENTRY_OVERHEAD_BYTES: u64 = 16 + 64 + 16 + 64;

/// Bytes an allocation of `len` bytes occupies: the allocator serves it from a size class, at
/// least 16-byte granular. Nothing for an empty (unallocated) buffer.
fn alloc_bytes(len: usize) -> u64 {
    if len == 0 { 0 } else { len.next_multiple_of(16) as u64 }
}

/// Resolve `[resources] max_map_cache_mb` to a byte budget. `0` stays `0` (unbounded).
pub(crate) fn budget_bytes_from(resources: &crate::config::ResourcesConfig) -> u64 {
    (resources.max_map_cache_mb as u64).saturating_mul(1024 * 1024)
}

/// Whether `error` says the blob's bytes are bad, as opposed to the read having failed. The
/// global store's directory is this binary's own schema namespace, so a blob there that claims
/// another schema is not one any basemind wrote: it is damaged like a torn one.
fn is_damage(error: &crate::store::StoreError) -> bool {
    use crate::store::StoreError;
    matches!(
        error,
        StoreError::CorruptBlob { .. }
            | StoreError::Decode(_)
            | StoreError::Decompression { .. }
            | StoreError::SchemaMismatch { .. }
            | StoreError::NewerSchema { .. }
    )
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
            extract_epoch: crate::extract::EXTRACT_EPOCH,
        }
    }

    /// A short string occupies a whole allocation, and every entry costs the cache its own node and
    /// key: a one-symbol outline is charged for both, not for the few bytes of its name.
    #[test]
    fn heap_bytes_charges_size_classes_and_the_entry() {
        let mut l1 = l1_with("a", 1);
        l1.language = String::new();
        let floor = ENTRY_OVERHEAD_BYTES
            + alloc_bytes(std::mem::size_of::<FileMapL1>())
            + alloc_bytes(std::mem::size_of::<Symbol>())
            + 16
            + 16;
        assert_eq!(l1_heap_bytes(&l1), floor);
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
        assert!(cache.load("ff", 0).is_some(), "the newest entry is never evicted");
    }

    /// A damaged blob must not stay where every later scan would trust it for existing.
    #[test]
    fn a_damaged_blob_is_dropped_so_the_next_scan_rebuilds_it() {
        crate::store::init_isolated_cache();
        let blobs = crate::store::global_blobs_dir();
        std::fs::create_dir_all(&blobs).unwrap();
        let hash = "d".repeat(64);
        let path = crate::store_blob_layout::sharded_blob_path(&blobs, &hash, ".fm.msgpack");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut torn = crate::store_blob::frame_filemap(&l1_with("a", 64), None).unwrap();
        torn.truncate(torn.len() / 2);
        std::fs::write(&path, torn).unwrap();
        let cache = L1Cache::new(blobs, 0);

        assert!(cache.load(&hash, 0).is_none());
        assert!(!path.exists(), "the damaged blob must be removed");
    }

    /// An extract-epoch refresh rewrites a blob under the same content hash. An entry naming the
    /// newer epoch must get the rewritten outline, not the one decoded before the rewrite, while a
    /// caller still at the older epoch keeps hitting the cache.
    #[test]
    fn an_outline_cached_before_an_epoch_refresh_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "e".repeat(64);
        let path = crate::store_blob_layout::sharded_blob_path(dir.path(), &hash, ".fm.msgpack");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old_epoch = crate::extract::EXTRACT_EPOCH - 1;
        let mut stale = l1_with("before", 1);
        stale.extract_epoch = old_epoch;
        std::fs::write(&path, crate::store_blob::frame_filemap(&stale, None).unwrap()).unwrap();
        let cache = L1Cache::new(dir.path().to_path_buf(), 0);
        assert_eq!(cache.load(&hash, old_epoch).unwrap().symbols[0].name, "before");

        let refreshed = l1_with("after", 1);
        std::fs::write(&path, crate::store_blob::frame_filemap(&refreshed, None).unwrap()).unwrap();
        let misses = cache.misses();
        assert_eq!(cache.load(&hash, old_epoch).unwrap().symbols[0].name, "before");
        assert_eq!(cache.misses(), misses, "a caller at the older epoch still hits");

        let newest = cache.load(&hash, crate::extract::EXTRACT_EPOCH).unwrap();
        assert_eq!(newest.symbols[0].name, "after");
        assert_eq!(newest.extract_epoch, crate::extract::EXTRACT_EPOCH);
    }

    /// A scan can rename a good blob over the damaged one between the failed read and the unlink.
    /// The unlink is for the file that failed, not for whatever is at its path by then.
    #[test]
    fn a_blob_renamed_into_place_after_the_failed_read_is_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.fm.msgpack");
        std::fs::write(&path, b"torn").unwrap();
        let failed = BlobIdentity::of(&path);
        let fresh = dir.path().join("blob.fm.msgpack.tmp");
        std::fs::write(&fresh, b"a good blob").unwrap();
        std::fs::rename(&fresh, &path).unwrap();

        assert!(!remove_if_still(&path, failed), "another file is at the path now");
        assert_eq!(std::fs::read(&path).unwrap(), b"a good blob");

        assert!(
            remove_if_still(&path, BlobIdentity::of(&path)),
            "the same file is removed"
        );
        assert!(!path.exists());
        assert!(
            !remove_if_still(&path, None),
            "a file that could not be identified is left alone"
        );
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

    /// Only bad bytes condemn a blob. A read that failed for another reason (the disk, a
    /// permission) says nothing about the file, which may be a good one a scan has just written.
    #[test]
    fn a_read_that_failed_is_not_a_damaged_blob() {
        use crate::store::StoreError;

        let path = std::path::PathBuf::from("blob.fm.msgpack");
        assert!(is_damage(&StoreError::CorruptBlob { path: path.clone() }));
        assert!(is_damage(&StoreError::SchemaMismatch { found: 1, expected: 2 }));
        assert!(!is_damage(&StoreError::Io {
            path,
            source: std::io::Error::from(std::io::ErrorKind::Interrupted),
        }));
    }
}
