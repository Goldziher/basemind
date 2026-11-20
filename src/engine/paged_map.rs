//! Sorted maps whose next version shares every page an edit does not touch.
//!
//! A watcher batch derives the next [`MapCache`](crate::engine::MapCache) from the current one: its
//! file view, its term index and its document paths. Copying a `BTreeMap` for that costs one
//! allocation per entry, so at 100k files one saved file paid several whole copies of the map.
//! [`PagedMap`] holds reference-counted entries in sorted pages of about [`PAGE`] entries;
//! [`PagedMap::patched`] copies the page pointers, rebuilds only the pages its edits land on and
//! allocates nothing for an entry it leaves alone.

use std::sync::Arc;

/// Entries per page as built. A patched page is split once it holds more than twice this, and one
/// left with fewer than half of it is merged into the next, so every page but the last stays within
/// a constant factor of it however a map is edited.
const PAGE: usize = 256;

/// One page: entries in key order, each shared by every version of the map that holds it.
type Page<K, V> = Arc<[Arc<(K, V)>]>;

/// A sorted map from `K` to `V` with structural sharing between versions.
pub(crate) struct PagedMap<K, V> {
    /// Non-empty pages in key order; the last key of one is below the first of the next.
    pages: Vec<Page<K, V>>,
    len: usize,
}

impl<K, V> Default for PagedMap<K, V> {
    fn default() -> Self {
        Self {
            pages: Vec::new(),
            len: 0,
        }
    }
}

impl<K, V> Clone for PagedMap<K, V> {
    fn clone(&self) -> Self {
        Self {
            pages: self.pages.clone(),
            len: self.len,
        }
    }
}

impl<K: Ord, V> PagedMap<K, V> {
    /// The map of `pairs`, in any order. Of two pairs with one key, the later is kept.
    pub(crate) fn from_unsorted(pairs: Vec<(K, V)>) -> Self {
        let entries = last_per_key(pairs).into_iter().map(Arc::new).collect();
        let mut pages = Vec::new();
        paginate(entries, PAGE, &mut pages);
        Self::from_pages(pages)
    }

    fn from_pages(pages: Vec<Page<K, V>>) -> Self {
        let len = pages.iter().map(|page| page.len()).sum();
        Self { pages, len }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        let page = self.pages.get(self.page_of(key))?;
        let at = page.binary_search_by(|entry| entry.0.cmp(key)).ok()?;
        Some(&page[at].1)
    }

    pub(crate) fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// Every entry, in key order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.pages
            .iter()
            .flat_map(|page| page.iter())
            .map(|entry| (&entry.0, &entry.1))
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|(key, _)| key)
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, value)| value)
    }

    /// The map with `edits` applied in order: `Some` inserts or replaces the key's value, `None`
    /// removes the key. A page no edit lands on is shared with `self`, so the cost is one pointer per
    /// page plus the pages edited, never a copy of the entries.
    pub(crate) fn patched(&self, edits: impl IntoIterator<Item = (K, Option<V>)>) -> Self {
        let edits = last_per_key(edits.into_iter().collect());
        if edits.is_empty() {
            return self.clone();
        }
        let mut pages = Vec::with_capacity(self.pages.len() + 1);
        let mut edits = edits.into_iter().peekable();
        let Some(last) = self.pages.len().checked_sub(1) else {
            let entries = edits.filter_map(|(key, value)| Some(Arc::new((key, value?)))).collect();
            paginate(entries, 2 * PAGE, &mut pages);
            return Self::from_pages(pages);
        };
        // Entries of an edited page left too small to stand alone, merged into the next page.
        let mut carry: Vec<Arc<(K, V)>> = Vec::new();
        for (index, page) in self.pages.iter().enumerate() {
            let bound = &page[page.len() - 1].0;
            // An edit lands on the first page whose last key is not below it; past the end, the last.
            let landing: Vec<(K, Option<V>)> =
                std::iter::from_fn(|| edits.next_if(|(key, _)| index == last || key <= bound)).collect();
            if landing.is_empty() && carry.is_empty() {
                pages.push(Arc::clone(page));
                continue;
            }
            let mut merged = std::mem::take(&mut carry);
            merge_into(&mut merged, page, landing);
            if merged.len() < PAGE / 2 && index != last {
                carry = merged;
                continue;
            }
            paginate(merged, 2 * PAGE, &mut pages);
        }
        Self::from_pages(pages)
    }

    /// Index of the page `key` belongs on: the first whose last key is not below it.
    fn page_of(&self, key: &K) -> usize {
        self.pages.partition_point(|page| page[page.len() - 1].0 < *key)
    }
}

impl<K: PartialEq, V: PartialEq> PartialEq for PagedMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len
            && self
                .pages
                .iter()
                .flat_map(|page| page.iter())
                .eq(other.pages.iter().flat_map(|page| page.iter()))
    }
}

impl<K: std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for PagedMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(
                self.pages
                    .iter()
                    .flat_map(|page| page.iter())
                    .map(|entry| (&entry.0, &entry.1)),
            )
            .finish()
    }
}

/// A sorted set of `K` with the structural sharing of [`PagedMap`].
pub(crate) struct PagedSet<K>(PagedMap<K, ()>);

impl<K> Default for PagedSet<K> {
    fn default() -> Self {
        Self(PagedMap::default())
    }
}

impl<K: Ord> PagedSet<K> {
    /// The set of `keys`, in any order.
    pub(crate) fn from_unsorted(keys: Vec<K>) -> Self {
        Self(PagedMap::from_unsorted(keys.into_iter().map(|key| (key, ())).collect()))
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn contains(&self, key: &K) -> bool {
        self.0.contains_key(key)
    }

    /// Every key, in order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &K> {
        self.0.keys()
    }

    /// The set with `edits` applied in order: `true` inserts the key, `false` removes it.
    pub(crate) fn patched(&self, edits: impl IntoIterator<Item = (K, bool)>) -> Self {
        Self(
            self.0
                .patched(edits.into_iter().map(|(key, present)| (key, present.then_some(())))),
        )
    }
}

impl<K: PartialEq> PartialEq for PagedSet<K> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<K: std::fmt::Debug> std::fmt::Debug for PagedSet<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set()
            .entries(self.0.pages.iter().flat_map(|page| page.iter()).map(|entry| &entry.0))
            .finish()
    }
}

/// `pairs` sorted by key, keeping the later of two pairs with one key.
fn last_per_key<K: Ord, T>(mut pairs: Vec<(K, T)>) -> Vec<(K, T)> {
    pairs.reverse();
    // Stable, so of two pairs with one key the later one in the input now comes first.
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs.dedup_by(|next, kept| next.0 == kept.0);
    pairs
}

/// Append `page` with the sorted `edits` applied to `out`, whose entries all sort before `page`'s.
fn merge_into<K: Ord, V>(out: &mut Vec<Arc<(K, V)>>, page: &[Arc<(K, V)>], edits: Vec<(K, Option<V>)>) {
    out.reserve(page.len() + edits.len());
    let mut entries = page.iter().peekable();
    for (key, value) in edits {
        while let Some(entry) = entries.next_if(|entry| entry.0 < key) {
            out.push(Arc::clone(entry));
        }
        entries.next_if(|entry| entry.0 == key);
        if let Some(value) = value {
            out.push(Arc::new((key, value)));
        }
    }
    out.extend(entries.cloned());
}

/// Push `entries` to `pages` as one page when they number at most `max`, else as the fewest pages
/// of at most [`PAGE`] each, their sizes differing by at most one (so none falls under half of it).
fn paginate<K, V>(entries: Vec<Arc<(K, V)>>, max: usize, pages: &mut Vec<Page<K, V>>) {
    if entries.is_empty() {
        return;
    }
    if entries.len() <= max {
        pages.push(entries.into());
        return;
    }
    let count = entries.len().div_ceil(PAGE);
    let (base, extra) = (entries.len() / count, entries.len() % count);
    let mut rest = entries.as_slice();
    for index in 0..count {
        let (page, tail) = rest.split_at(base + usize::from(index < extra));
        pages.push(Page::from(page));
        rest = tail;
    }
}

#[cfg(test)]
#[path = "paged_map_tests.rs"]
mod tests;
