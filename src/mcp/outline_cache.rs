//! The parsed-outline cache: a blob's outline and source by `(blob id, language)`.
//!
//! Symbol history, `git_blame {symbol}` and the outline of a file edited since the scan each parse a
//! file's content once and keep it here, so repeated visits to the same content (across commits,
//! pages or tool calls) skip the tree-sitter parse. Blob ids are content-addressed, so an entry never
//! goes stale: a new content is a new key.
//!
//! An entry holds the whole file, up to `scan.max_file_bytes`, and its parsed outline, so the cache
//! is bounded in bytes as well as in entries: by count alone, 512 large files stayed resident.

use std::sync::{Arc, Mutex};

use lru::LruCache;

use super::OutlineEntry;
use crate::lang::LangId;

/// Entries kept at once.
const ENTRIES_MAX: usize = 512;
/// Bytes of source and parsed outline kept at once, together.
const BYTES_MAX: u64 = 64 * 1024 * 1024;

type Key = (gix::ObjectId, LangId);

/// Outlines by blob, least recently used first. Bounded by an entry count and a byte budget; the
/// entry just stored is never the one evicted, so a file larger than the budget is still answered.
pub(crate) struct OutlineCache {
    inner: Mutex<Held>,
}

struct Held {
    /// Each entry with its [`cost`], computed once before the lock is taken.
    lru: LruCache<Key, (Arc<OutlineEntry>, u64)>,
    entries_max: usize,
    bytes_max: u64,
    bytes: u64,
}

impl Default for OutlineCache {
    fn default() -> Self {
        Self::with_bounds(ENTRIES_MAX, BYTES_MAX)
    }
}

impl OutlineCache {
    pub(crate) fn with_bounds(entries_max: usize, bytes_max: u64) -> Self {
        Self {
            inner: Mutex::new(Held {
                lru: LruCache::unbounded(),
                entries_max: entries_max.max(1),
                bytes_max,
                bytes: 0,
            }),
        }
    }

    /// The entry stored for `key`, marked most recently used.
    pub(crate) fn get(&self, key: &Key) -> Option<Arc<OutlineEntry>> {
        self.inner.lock().ok()?.lru.get(key).map(|(entry, _)| Arc::clone(entry))
    }

    /// Store `entry` under `key`, then evict least recently used entries until both bounds hold.
    pub(crate) fn put(&self, key: Key, entry: Arc<OutlineEntry>) {
        let added = cost(&entry);
        let Ok(mut held) = self.inner.lock() else {
            return;
        };
        if let Some((_, replaced)) = held.lru.put(key, (entry, added)) {
            held.bytes = held.bytes.saturating_sub(replaced);
        }
        held.bytes = held.bytes.saturating_add(added);
        while held.lru.len() > held.entries_max || (held.bytes > held.bytes_max && held.lru.len() > 1) {
            let Some((_, (_, evicted))) = held.lru.pop_lru() else {
                break;
            };
            held.bytes = held.bytes.saturating_sub(evicted);
        }
    }

    /// Entries held, and their bytes.
    #[cfg(test)]
    pub(crate) fn held(&self) -> (usize, u64) {
        let held = self.inner.lock().expect("outline cache lock");
        (held.lru.len(), held.bytes)
    }
}

fn cost(entry: &OutlineEntry) -> u64 {
    entry.source.len() as u64 + super::l1_cache::l1_heap_bytes(&entry.map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(bytes: usize) -> Arc<OutlineEntry> {
        let _ = crate::lang::ensure_grammars().expect("grammar bootstrap");
        let rust = crate::lang::intern("rust").expect("rust grammar");
        Arc::new(OutlineEntry {
            map: Arc::new(crate::extract::l1::extract_l1(rust, b"").expect("empty outline")),
            source: Arc::new(vec![b'x'; bytes]),
        })
    }

    fn key(n: u8) -> Key {
        let mut raw = [0u8; 20];
        raw[19] = n;
        (gix::ObjectId::from_bytes_or_panic(&raw), "rust")
    }

    /// Each entry holds a whole file, so the count alone does not bound the memory: 512 files of
    /// `scan.max_file_bytes` stayed resident.
    #[test]
    fn large_entries_are_held_to_the_byte_budget() {
        let empty = cost(&entry(0));
        let cache = OutlineCache::with_bounds(512, 3 * (1000 + empty));
        for n in 0..10 {
            cache.put(key(n), entry(1000));
        }
        let (entries, bytes) = cache.held();
        assert_eq!(entries, 3, "three fit the budget, not 512");
        assert!(bytes <= 3 * (1000 + empty));
        assert!(cache.get(&key(9)).is_some(), "the newest is kept");
        assert!(cache.get(&key(0)).is_none(), "the oldest went first");

        cache.put(key(42), entry(10_000));
        assert!(
            cache.get(&key(42)).is_some(),
            "one entry over the budget is still answered"
        );
        assert_eq!(cache.held().0, 1);
    }

    #[test]
    fn the_entry_count_still_bounds_small_entries() {
        let cache = OutlineCache::with_bounds(4, u64::MAX);
        for n in 0..6 {
            cache.put(key(n), entry(1));
        }
        assert_eq!(cache.held().0, 4);
        cache.put(key(5), entry(1));
        assert_eq!(
            cache.held(),
            (4, 4 * cost(&entry(1))),
            "storing a key again replaces it"
        );
    }
}
