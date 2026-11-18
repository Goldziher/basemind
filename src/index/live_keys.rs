//! The content keys one view holds (ADR-0028 rule 7), and the reads that turn content rows into the
//! view's paths.
//!
//! Symbol and call rows are keyed by content ([`super::keys_content`]) and shared by every view of
//! a repository group. A view answers only from the content its file map names: [`LiveKeys`] maps
//! each such content key to the view's paths holding it, and back. The store builds it from the file
//! map and publishes it whenever it writes the map, so a reader sees the rows of exactly the files
//! the last written map lists, never another view's and never a half-written scan's.
//!
//! [`ExpandHits`] walks a name-led keyspace in key order and yields one hit key per live holder of
//! each row (row key ‖ path), in path order within the row. A row no live path holds is yielded as
//! its bare row key, which names no holder and so decodes to no hit: it still counts toward a
//! scan's examined-key cap, and a scan that stops on it resumes after it.

use std::cmp::Ordering;
use std::ops::Bound;
use std::sync::Arc;

use ahash::AHashMap;

use super::keys::{ContentKey, NameRow, split_hit};
use super::view::{IndexBytes, KvIter, KvResult};
use crate::path::RelPath;

/// The content keys a view holds. See the module docs.
///
/// One copy of each path: `entries` holds every `(key, path)` sorted by key then path, so a key's
/// holders are one run of it, and the two lookups index into it.
#[derive(Debug, Default)]
pub struct LiveKeys {
    entries: Box<[(ContentKey, RelPath)]>,
    /// Each live key's run in `entries`: `(start, end)`.
    runs: AHashMap<ContentKey, (u32, u32)>,
    /// Indices into `entries`, sorted by path.
    by_path: Box<[u32]>,
    /// See [`Self::fingerprint`].
    fingerprint: u64,
}

impl LiveKeys {
    /// The live keys of a file map's `(path, content key)` entries. A path listed twice keeps its
    /// last key.
    pub fn from_entries(entries: impl IntoIterator<Item = (RelPath, ContentKey)>) -> Self {
        let mut entries: Vec<(ContentKey, RelPath)> = entries.into_iter().map(|(path, key)| (key, path)).collect();
        entries.reverse();
        entries.sort_by(|a, b| a.1.cmp(&b.1));
        entries.dedup_by(|later, kept| later.1 == kept.1);
        entries.sort_unstable();
        let mut runs = AHashMap::with_capacity(entries.len());
        let mut start = 0;
        for run in entries.chunk_by(|a, b| a.0 == b.0) {
            let end = start + run.len();
            runs.insert(run[0].0, (index(start), index(end)));
            start = end;
        }
        let mut by_path: Vec<u32> = (0..entries.len()).map(index).collect();
        by_path.sort_unstable_by(|a, b| entries[*a as usize].1.cmp(&entries[*b as usize].1));
        let fingerprint = fingerprint_of(by_path.iter().map(|at| {
            let (key, path) = &entries[*at as usize];
            (path.as_bytes(), *key)
        }));
        Self {
            entries: entries.into_boxed_slice(),
            runs,
            by_path: by_path.into_boxed_slice(),
            fingerprint,
        }
    }

    /// The view's paths holding `key`, sorted, each with the key; empty when the view holds none.
    pub fn holders(&self, key: &ContentKey) -> &[(ContentKey, RelPath)] {
        self.runs
            .get(key)
            .map_or(&[], |(start, end)| &self.entries[*start as usize..*end as usize])
    }

    /// The content key the view holds at `rel`.
    pub fn key_of(&self, rel: &RelPath) -> Option<&ContentKey> {
        let at = self
            .by_path
            .binary_search_by(|at| self.entries[*at as usize].1.as_bytes().cmp(rel.as_bytes()))
            .ok()?;
        Some(&self.entries[self.by_path[at] as usize].0)
    }

    /// Every live path with its content key, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&RelPath, &ContentKey)> {
        self.entries.iter().map(|(key, path)| (path, key))
    }

    /// Every live key.
    pub fn keys(&self) -> impl Iterator<Item = &ContentKey> {
        self.runs.keys()
    }

    /// Whether the view holds no content.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// A digest of every `(path, key)` pair: equal for two publications exactly when they map the
    /// same paths to the same keys, in this process or another. A name scan's cursor carries it, so
    /// a page cut before the view changed is not continued after.
    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
}

/// The digest of `(path, key)` pairs given in path order ([`LiveKeys::fingerprint`]). The in-RAM map
/// digests its order keys the same way, so the two agree whenever they map the same paths alike.
pub(crate) fn fingerprint_of<'a>(pairs: impl Iterator<Item = (&'a [u8], ContentKey)>) -> u64 {
    let mut hasher = blake3::Hasher::new();
    for (path, key) in pairs {
        hasher.update(&u32::try_from(path.len()).unwrap_or(u32::MAX).to_be_bytes());
        hasher.update(path);
        hasher.update(&key);
    }
    let digest = hasher.finalize();
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_be_bytes(head)
}

/// `at` as an index into [`LiveKeys::entries`]. A view of more than `u32::MAX` files is not
/// representable anyway: the file map's own counts are `u32`.
fn index(at: usize) -> u32 {
    u32::try_from(at).unwrap_or(u32::MAX)
}

/// [`LiveKeys`] of one view, shared by every handle on it.
#[derive(Default)]
pub struct LiveKeysCell(parking_lot::RwLock<Option<Arc<LiveKeys>>>);

impl LiveKeysCell {
    /// The published keys; none before the first publication.
    pub(crate) fn get(&self) -> Arc<LiveKeys> {
        self.0.read().clone().unwrap_or_default()
    }

    /// Replace the published keys: the view's file map was just written.
    pub(crate) fn publish(&self, live: LiveKeys) {
        *self.0.write() = Some(Arc::new(live));
    }

    /// Publish `build()` unless a writer already has: for a reader that loaded the file map itself.
    pub(crate) fn publish_if_unset(&self, build: impl FnOnce() -> LiveKeys) {
        let mut cell = self.0.write();
        if cell.is_none() {
            *cell = Some(Arc::new(build()));
        }
    }
}

impl super::IndexDb {
    /// Publish this view's live keys. The store calls it whenever it writes the view's file map.
    pub fn publish_live_keys(&self, live: LiveKeys) {
        self.live.publish(live);
    }

    /// [`Self::publish_live_keys`] unless the view's keys are already published, by a writer of
    /// this process.
    pub fn publish_live_keys_if_unset(&self, build: impl FnOnce() -> LiveKeys) {
        self.live.publish_if_unset(build);
    }

    /// This view's published live keys.
    pub fn live_keys(&self) -> Arc<LiveKeys> {
        self.live.get()
    }
}

/// The row key and path a cursor names: holders of that row up to the path were already listed.
pub(crate) type Resume = Option<(Vec<u8>, Vec<u8>)>;

/// Where a scan of a name-led keyspace resumes after `cursor`, a hit key or a bare row key: the
/// lower bound in row keys, and the row and path the cursor names within it.
pub(crate) fn resume_after(cursor: Option<&[u8]>, shape: NameRow) -> (Bound<Vec<u8>>, Resume) {
    match cursor.map(|cursor| (cursor, split_hit(cursor, shape))) {
        None => (Bound::Unbounded, None),
        Some((_, Some((row, path)))) => (Bound::Included(row.to_vec()), Some((row.to_vec(), path.to_vec()))),
        // Not of this keyspace's shape: step past it as a plain key. ~keep
        Some((cursor, None)) => (Bound::Excluded(cursor.to_vec()), None),
    }
}

/// A row being expanded into its holders: `entries[next..end]` of the live keys are still to come.
struct Pending {
    row: IndexBytes,
    value: IndexBytes,
    next: usize,
    end: usize,
}

/// See the module docs.
pub(crate) struct ExpandHits<'a> {
    rows: KvIter<'a>,
    live: Arc<LiveKeys>,
    shape: NameRow,
    /// The cursor's row and path: holders of that row up to the path were on an earlier page.
    resume: Resume,
    pending: Option<Pending>,
}

impl<'a> ExpandHits<'a> {
    pub(crate) fn new(rows: KvIter<'a>, live: Arc<LiveKeys>, shape: NameRow, resume: Resume) -> Self {
        Self {
            rows,
            live,
            shape,
            resume,
            pending: None,
        }
    }
}

impl Iterator for ExpandHits<'_> {
    type Item = KvResult;

    fn next(&mut self) -> Option<KvResult> {
        loop {
            if let Some(pending) = &mut self.pending {
                if pending.next < pending.end {
                    let path = self.live.entries[pending.next].1.as_bytes();
                    pending.next += 1;
                    let key: Arc<[u8]> = pending.row.iter().chain(path).copied().collect();
                    return Some(Ok((IndexBytes::shared(key), pending.value.clone())));
                }
                self.pending = None;
            }
            let (row, value) = match self.rows.next()? {
                Ok(row) => row,
                Err(error) => return Some(Err(error)),
            };
            let content = split_hit(&row, self.shape)
                .filter(|(_, tail)| tail.is_empty())
                .and_then(|(row, _)| self.shape.content_of(row));
            let Some(content) = content else {
                return Some(Ok((row, value)));
            };
            let Some(&(start, end)) = self.live.runs.get(&content) else {
                if self.resume.as_ref().is_some_and(|(cursor_row, _)| *row <= **cursor_row) {
                    continue;
                }
                self.resume = None;
                return Some(Ok((row, value)));
            };
            let (mut next, end) = (start as usize, end as usize);
            if let Some((cursor_row, cursor_path)) = &self.resume {
                match (*row).cmp(cursor_row.as_slice()) {
                    Ordering::Less => continue,
                    Ordering::Equal => {
                        let holders = &self.live.entries[next..end];
                        next += holders.partition_point(|(_, path)| path.as_bytes() <= cursor_path.as_slice());
                        if next >= end {
                            continue;
                        }
                    }
                    Ordering::Greater => self.resume = None,
                }
            }
            self.pending = Some(Pending { row, value, next, end });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::keys::{call_by_callee, call_by_callee_hit, content_key, parse_call_by_callee};

    fn rows(keys: &[Vec<u8>]) -> KvIter<'static> {
        let rows: Vec<KvResult> = keys
            .iter()
            .map(|key| {
                Ok((
                    IndexBytes::shared(key.as_slice().into()),
                    IndexBytes::shared(Arc::from([])),
                ))
            })
            .collect();
        Box::new(rows.into_iter())
    }

    fn hits(iter: ExpandHits<'_>) -> Vec<Option<(String, RelPath, u32)>> {
        iter.map(|row| parse_call_by_callee(&row.expect("row").0)).collect()
    }

    #[test]
    fn a_row_expands_to_its_live_holders_and_a_dead_row_to_no_hit() {
        let shared = content_key("a", "rust", 1);
        // Rows come in key order: the dead row sorts after the shared one. ~keep
        let dead = ["b", "c", "d", "e", "f", "g", "h"]
            .into_iter()
            .map(|hash| content_key(hash, "rust", 1))
            .find(|key| *key > shared)
            .expect("a key above");
        let (x, y) = (RelPath::from("x.rs"), RelPath::from("y.rs"));
        let live = Arc::new(LiveKeys::from_entries([(y.clone(), shared), (x.clone(), shared)]));
        let keys = [
            call_by_callee("f", &shared, 1).unwrap(),
            call_by_callee("f", &dead, 1).unwrap(),
        ];

        let got = hits(ExpandHits::new(rows(&keys), Arc::clone(&live), NameRow::Call, None));

        assert_eq!(
            got,
            vec![
                Some(("f".to_string(), x.clone(), 1)),
                Some(("f".to_string(), y.clone(), 1)),
                None
            ]
        );

        let cursor = call_by_callee_hit("f", &shared, 1, &x).unwrap();
        let (lower, resume) = resume_after(Some(&cursor), NameRow::Call);
        assert_eq!(lower, Bound::Included(keys[0].clone()));
        let rest = hits(ExpandHits::new(rows(&keys), live, NameRow::Call, resume));
        assert_eq!(
            rest,
            vec![Some(("f".to_string(), y, 1)), None],
            "resumes inside the row"
        );
    }

    #[test]
    fn live_keys_map_both_ways() {
        let key = content_key("a", "rust", 1);
        let live = LiveKeys::from_entries([(RelPath::from("b.rs"), key), (RelPath::from("a.rs"), key)]);
        let paths: Vec<&RelPath> = live.holders(&key).iter().map(|(_, path)| path).collect();
        assert_eq!(paths, [&RelPath::from("a.rs"), &RelPath::from("b.rs")]);
        assert_eq!(live.key_of(&RelPath::from("b.rs")), Some(&key));
        assert_eq!(live.key_of(&RelPath::from("c.rs")), None);
        assert!(live.holders(&content_key("z", "rust", 1)).is_empty());
    }

    #[test]
    fn a_path_listed_twice_keeps_its_last_key_and_the_fingerprint_follows_the_mapping() {
        let (a, b) = (content_key("a", "rust", 1), content_key("b", "rust", 1));
        let rel = RelPath::from("x.rs");
        let live = LiveKeys::from_entries([(rel.clone(), a), (rel.clone(), b)]);
        assert_eq!(live.key_of(&rel), Some(&b));
        assert!(live.holders(&a).is_empty());
        let same = LiveKeys::from_entries([(rel.clone(), b)]);
        assert_eq!(live.fingerprint(), same.fingerprint());
        assert_ne!(same.fingerprint(), LiveKeys::from_entries([(rel, a)]).fingerprint());
        assert_ne!(
            same.fingerprint(),
            LiveKeys::from_entries([(RelPath::from("y.rs"), b)]).fingerprint()
        );
    }
}
