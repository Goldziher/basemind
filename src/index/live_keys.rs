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
//! each site of each row (site key ‖ path), in path order within the site; a packed row's sites in
//! start order. A site no live path holds is yielded as its bare site key, which names no holder
//! and so decodes to no hit: it still counts toward a scan's examined-key cap, and a scan that stops
//! on it resumes after it.

use std::cmp::Ordering;
use std::ops::Bound;
use std::sync::Arc;

use super::keys::{ContentKey, NameRow, split_hit};
use super::view::{IndexBytes, KvIter, KvResult};
use crate::path::RelPath;

/// The content keys a view holds. See the module docs.
///
/// One copy of each path: `entries` holds every `(key, path)` sorted by key then path, so a key's
/// holders are one run of it, and the two lookups index into it. The distinct keys are a sorted
/// array indexed by their leading bits, not a hash map: content keys are uniform hashes, so a
/// lookup reads one bucket bound and about one key, with no hashing ([`ExpandHits`] does one per
/// row it reads).
#[derive(Debug, Default)]
pub struct LiveKeys {
    entries: Box<[(ContentKey, RelPath)]>,
    /// Each distinct live key with its run in `entries`, ascending by key: one cache line per key.
    runs: Box<[Run]>,
    /// `buckets[b]` is the first index into `runs` whose key's leading `bucket_bits` bits are at
    /// least `b`; one trailing entry holds the key count.
    buckets: Box<[u32]>,
    bucket_bits: u32,
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
        let mut runs = Vec::new();
        let mut start = 0;
        for run in entries.chunk_by(|a, b| a.0 == b.0) {
            let end = start + run.len();
            runs.push(Run {
                key: u128::from_be_bytes(run[0].0),
                start: index(start),
                end: index(end),
            });
            start = end;
        }
        let (buckets, bucket_bits) = bucket_index(&runs);
        let mut by_path: Vec<u32> = (0..entries.len()).map(index).collect();
        by_path.sort_unstable_by(|a, b| entries[*a as usize].1.cmp(&entries[*b as usize].1));
        let fingerprint = fingerprint_of(by_path.iter().map(|at| {
            let (key, path) = &entries[*at as usize];
            (path.as_bytes(), *key)
        }));
        Self {
            entries: entries.into_boxed_slice(),
            runs: runs.into_boxed_slice(),
            buckets,
            bucket_bits,
            by_path: by_path.into_boxed_slice(),
            fingerprint,
        }
    }

    /// The view's paths holding `key`, sorted, each with the key; empty when the view holds none.
    pub fn holders(&self, key: &ContentKey) -> &[(ContentKey, RelPath)] {
        self.find_run(u128::from_be_bytes(*key))
            .map_or(&[], |(start, end)| &self.entries[start as usize..end as usize])
    }

    /// The run of `key` in `entries`: searched within the bucket of its leading bits.
    fn find_run(&self, key: u128) -> Option<(u32, u32)> {
        let run = self.runs.get(self.first_run_at_or_above(key))?;
        (run.key == key).then_some((run.start, run.end))
    }

    /// [`Self::find_run`] for a [`LiveCursor`] at `at`, which it moves to the first run not below
    /// `key`.
    fn find_run_from(&self, at: &mut usize, key: u128) -> Option<(u32, u32)> {
        let runs = &self.runs;
        let ahead = runs.get(*at..).unwrap_or_default();
        let behind = *at > 0 && runs[*at - 1].key >= key;
        let near = match behind {
            true => None,
            false => ahead.iter().take(CURSOR_STEPS).position(|run| run.key >= key),
        };
        *at = match near {
            Some(step) => *at + step,
            None if !behind && ahead.len() <= CURSOR_STEPS => runs.len(),
            None => self.first_run_at_or_above(key),
        };
        let run = runs.get(*at)?;
        (run.key == key).then_some((run.start, run.end))
    }

    /// Index into `runs` of the first run whose key is not below `key`: searched within the bucket
    /// of its leading bits; `runs.len()` when there is none.
    fn first_run_at_or_above(&self, key: u128) -> usize {
        let bucket = bucket_of(key, self.bucket_bits);
        // A default (never published) set has no buckets. ~keep
        let (Some(from), Some(to)) = (self.buckets.get(bucket), self.buckets.get(bucket + 1)) else {
            return self.runs.len();
        };
        let (from, to) = (*from as usize, *to as usize);
        from + self.runs[from..to].partition_point(|run| run.key < key)
    }

    /// A cursor for looking keys up in ascending order, as a name scan meets them.
    pub(crate) fn cursor(&self) -> LiveCursor<'_> {
        LiveCursor { live: self, at: 0 }
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
        self.runs.iter().map(|run| &self.entries[run.start as usize].0)
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

/// The bucket of `key` among `2^bits` buckets: its leading `bits` bits.
fn bucket_of(key: u128, bits: u32) -> usize {
    key.checked_shr(128 - bits).unwrap_or(0) as usize
}

/// One distinct live key and its run `entries[start..end]`.
#[derive(Debug)]
struct Run {
    key: u128,
    start: u32,
    end: u32,
}

/// The bucket bounds of `runs`, sorted by key: about one key per bucket.
fn bucket_index(runs: &[Run]) -> (Box<[u32]>, u32) {
    let bits = usize::BITS - runs.len().leading_zeros();
    let mut buckets = vec![0u32; (1usize << bits) + 1];
    let mut at = 0;
    for (bucket, bound) in buckets.iter_mut().enumerate() {
        while at < runs.len() && bucket_of(runs[at].key, bits) < bucket {
            at += 1;
        }
        *bound = index(at);
    }
    (buckets.into_boxed_slice(), bits)
}

/// Runs a [`LiveCursor`] steps over before it looks a key up by its bucket instead.
const CURSOR_STEPS: usize = 8;

/// Looks up the live keys a name scan meets: the rows of one name (and symbol kind) come in
/// ascending content-key order, so the next key is usually a few runs past the last one, on the
/// same or the next cache line. A key behind the cursor, or too far ahead, is looked up by its
/// bucket and the cursor moves there. Answers exactly what [`LiveKeys::holders`] answers.
pub(crate) struct LiveCursor<'a> {
    live: &'a LiveKeys,
    /// Index into `runs` of the first run not below the last key looked up.
    at: usize,
}

impl<'a> LiveCursor<'a> {
    /// The view's paths holding `key`, sorted, each with the key; empty when the view holds none.
    pub(crate) fn holders(&mut self, key: &ContentKey) -> &'a [(ContentKey, RelPath)] {
        let live = self.live;
        live.find_run_from(&mut self.at, u128::from_be_bytes(*key))
            .map_or(&[], |(start, end)| &live.entries[start as usize..end as usize])
    }
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
        if let Some(own) = &self.group {
            own.group.settle_pending(own.view_id);
        }
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

/// The site key and path a cursor names: holders of that site up to the path were already listed.
pub(crate) type Resume = Option<(Vec<u8>, Vec<u8>)>;

/// Where a scan of a name-led keyspace resumes after `cursor`, a hit key or a bare site key: the
/// lower bound in stored row keys, and the site and path the cursor names within it.
pub(crate) fn resume_after(cursor: Option<&[u8]>, shape: NameRow) -> (Bound<Vec<u8>>, Resume) {
    match cursor.map(|cursor| (cursor, split_hit(cursor, shape))) {
        None => (Bound::Unbounded, None),
        Some((_, Some((site, path)))) => (
            Bound::Included(shape.row_of(site).to_vec()),
            Some((site.to_vec(), path.to_vec())),
        ),
        // Not of this keyspace's shape: step past it as a plain key. ~keep
        Some((cursor, None)) => (Bound::Excluded(cursor.to_vec()), None),
    }
}

/// Bytes of one start in a site key or a packed row value.
const START_LEN: usize = 4;

/// A stored row being expanded into its sites, and the current site into its holders.
struct Row {
    /// Which row of the scan this is: a buffer that already begins with its head keeps it.
    id: u64,
    /// The stored row key; its first `head_len` bytes begin every site key of the row.
    key: IndexBytes,
    value: IndexBytes,
    head_len: usize,
    /// Whether the starts are packed in the value; otherwise the key's last four bytes are the one.
    packed: bool,
    /// Offset of the next start in the value or key.
    at: usize,
    /// The holders of the row's content, `None` when the view holds none.
    run: Option<(u32, u32)>,
    /// The current site's start, and its holders still to come: `entries[next..end]`.
    start: [u8; START_LEN],
    next: usize,
    end: usize,
}

impl Row {
    /// Step to the row's next site; false once its sites are done.
    fn next_site(&mut self) -> bool {
        let source: &[u8] = if self.packed { &self.value } else { &self.key };
        match source.get(self.at..self.at + START_LEN) {
            Some(start) => {
                self.start.copy_from_slice(start);
                self.at += START_LEN;
                true
            }
            None => false,
        }
    }

    fn head(&self) -> &[u8] {
        &self.key[..self.head_len]
    }
}

/// See the module docs.
pub(crate) struct ExpandHits<'a> {
    rows: KvIter<'a>,
    live: Arc<LiveKeys>,
    shape: NameRow,
    /// The cursor's site and path: holders of that site up to the path were on an earlier page.
    resume: Resume,
    row: Option<Row>,
    /// Where the next content is looked up from in the live keys: see [`LiveCursor`].
    cursor: usize,
    /// Two buffers hit keys are composed in. A reader that dropped the key it was handed leaves its
    /// buffer to be written again, so a scan allocates per hit only while its reader keeps keys.
    buffers: [Arc<Vec<u8>>; 2],
    /// The row each buffer begins with the head of (see [`Row::id`]).
    heads: [u64; 2],
    /// Rows opened so far: the id of the next.
    opened: u64,
}

impl<'a> ExpandHits<'a> {
    pub(crate) fn new(rows: KvIter<'a>, live: Arc<LiveKeys>, shape: NameRow, resume: Resume) -> Self {
        Self {
            rows,
            live,
            shape,
            resume,
            row: None,
            cursor: 0,
            buffers: [Arc::new(Vec::new()), Arc::new(Vec::new())],
            heads: [u64::MAX; 2],
            opened: 0,
        }
    }

    /// The run of `content`'s holders in the live keys.
    fn run_of(&mut self, content: ContentKey) -> Option<(u32, u32)> {
        self.live.find_run_from(&mut self.cursor, u128::from_be_bytes(content))
    }

    /// Open a stored row, or hand it back when its key is not of this keyspace's shape.
    fn open(&mut self, key: IndexBytes, value: IndexBytes) -> Result<Row, (IndexBytes, IndexBytes)> {
        let Some((head, content)) = self.shape.split_row(&key) else {
            return Err((key, value));
        };
        let head_len = head.len();
        let run = self.run_of(content);
        let packed = self.shape.packed();
        let id = self.opened;
        self.opened += 1;
        Ok(Row {
            id,
            key,
            value,
            head_len,
            packed,
            at: if packed { 0 } else { head_len },
            run,
            start: [0; START_LEN],
            next: 0,
            end: 0,
        })
    }
}

/// Where hit keys are composed: two buffers, and the row each begins with the head of.
struct Buffers<'b> {
    buffers: &'b mut [Arc<Vec<u8>>; 2],
    heads: &'b mut [u64; 2],
}

impl Buffers<'_> {
    /// `row`'s head, `start` and `path` composed into one key, in a buffer the reader no longer
    /// holds when there is one. A buffer that already begins with the row's head keeps it.
    fn compose(&mut self, row: &Row, path: &[u8]) -> IndexBytes {
        let head = row.head();
        let slot = match (0..2).find(|slot| Arc::get_mut(&mut self.buffers[*slot]).is_some()) {
            Some(slot) => slot,
            None => {
                self.buffers.swap(0, 1);
                self.heads.swap(0, 1);
                self.buffers[0] = Arc::new(Vec::with_capacity(head.len() + START_LEN + path.len()));
                self.heads[0] = u64::MAX;
                0
            }
        };
        let kept = self.heads[slot] == row.id;
        let bytes = Arc::get_mut(&mut self.buffers[slot]).expect("a buffer only this scan holds");
        if kept {
            bytes.truncate(head.len());
        } else {
            bytes.clear();
            bytes.extend_from_slice(head);
        }
        self.heads[slot] = row.id;
        bytes.extend_from_slice(&row.start);
        bytes.extend_from_slice(path);
        IndexBytes::buffer(Arc::clone(&self.buffers[slot]))
    }
}

/// How the site `head ‖ start` orders against the site key `site`.
fn cmp_site(head: &[u8], start: &[u8; START_LEN], site: &[u8]) -> Ordering {
    head.iter().chain(start.iter()).cmp(site.iter())
}

impl Iterator for ExpandHits<'_> {
    type Item = KvResult;

    fn next(&mut self) -> Option<KvResult> {
        loop {
            let Some(row) = &mut self.row else {
                let (key, value) = match self.rows.next()? {
                    Ok(row) => row,
                    Err(error) => return Some(Err(error)),
                };
                match self.open(key, value) {
                    Ok(row) => self.row = Some(row),
                    Err(unparsed) => return Some(Ok(unparsed)),
                }
                continue;
            };
            if row.next < row.end {
                let path = self.live.entries[row.next].1.as_bytes();
                row.next += 1;
                let mut buffers = Buffers {
                    buffers: &mut self.buffers,
                    heads: &mut self.heads,
                };
                return Some(Ok((buffers.compose(row, path), IndexBytes::empty())));
            }
            if !row.next_site() {
                self.row = None;
                continue;
            }
            let Some((first, end)) = row.run else {
                if let Some((site, _)) = &self.resume
                    && cmp_site(row.head(), &row.start, site) != Ordering::Greater
                {
                    continue;
                }
                self.resume = None;
                let key = if row.packed {
                    let mut buffers = Buffers {
                        buffers: &mut self.buffers,
                        heads: &mut self.heads,
                    };
                    buffers.compose(row, &[])
                } else {
                    row.key.clone()
                };
                return Some(Ok((key, IndexBytes::empty())));
            };
            let (mut next, end) = (first as usize, end as usize);
            if let Some((site, path)) = &self.resume {
                match cmp_site(row.head(), &row.start, site) {
                    Ordering::Less => next = end,
                    Ordering::Equal => {
                        let holders = &self.live.entries[next..end];
                        next += holders.partition_point(|(_, held)| held.as_bytes() <= path.as_slice());
                    }
                    Ordering::Greater => self.resume = None,
                }
            }
            row.next = next;
            row.end = end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::SymbolKind;
    use crate::index::keys::{
        call_by_callee, call_by_callee_hit, call_name_row, content_key, pack_starts, parse_call_by_callee,
        symbol_by_name, symbol_by_name_hit,
    };

    fn rows(rows: &[(Vec<u8>, Vec<u8>)]) -> KvIter<'static> {
        let rows: Vec<KvResult> = rows
            .iter()
            .map(|(key, value)| {
                Ok((
                    IndexBytes::buffer(Arc::new(key.clone())),
                    IndexBytes::buffer(Arc::new(value.clone())),
                ))
            })
            .collect();
        Box::new(rows.into_iter())
    }

    fn hits(iter: ExpandHits<'_>) -> Vec<Option<(String, RelPath, u32)>> {
        iter.map(|row| parse_call_by_callee(&row.expect("row").0)).collect()
    }

    /// The keys a scan yields, as bytes: a reader that keeps every key it is handed.
    fn keys(iter: ExpandHits<'_>) -> Vec<Vec<u8>> {
        let held: Vec<IndexBytes> = iter.map(|row| row.expect("row").0).collect();
        held.iter().map(|key| key.to_vec()).collect()
    }

    #[test]
    fn a_packed_row_expands_to_each_site_per_live_holder_and_a_dead_row_to_bare_sites() {
        let shared = content_key("a", "rust", 1);
        // Rows come in key order: the dead row sorts after the shared one. ~keep
        let dead = ["b", "c", "d", "e", "f", "g", "h"]
            .into_iter()
            .map(|hash| content_key(hash, "rust", 1))
            .find(|key| *key > shared)
            .expect("a key above");
        let (x, y) = (RelPath::from("x.rs"), RelPath::from("y.rs"));
        let live = Arc::new(LiveKeys::from_entries([(y.clone(), shared), (x.clone(), shared)]));
        let stored = [
            (call_name_row("f", &shared).unwrap(), pack_starts([5, 1])),
            (call_name_row("f", &dead).unwrap(), pack_starts([2, 1])),
        ];
        let hit = |rel: &RelPath, start| Some(("f".to_string(), rel.clone(), start));

        let got = hits(ExpandHits::new(rows(&stored), Arc::clone(&live), NameRow::Call, None));
        assert_eq!(got, vec![hit(&x, 1), hit(&y, 1), hit(&x, 5), hit(&y, 5), None, None]);
        let all = keys(ExpandHits::new(rows(&stored), Arc::clone(&live), NameRow::Call, None));
        assert_eq!(
            all,
            vec![
                call_by_callee_hit("f", &shared, 1, &x).unwrap(),
                call_by_callee_hit("f", &shared, 1, &y).unwrap(),
                call_by_callee_hit("f", &shared, 5, &x).unwrap(),
                call_by_callee_hit("f", &shared, 5, &y).unwrap(),
                call_by_callee("f", &dead, 1).unwrap(),
                call_by_callee("f", &dead, 2).unwrap(),
            ],
            "hit keys are site key and path; a dead site is its bare site key; kept keys stay intact"
        );

        let mut last_only: Vec<Vec<u8>> = Vec::new();
        let mut held: Option<IndexBytes> = None;
        for row in ExpandHits::new(rows(&stored), Arc::clone(&live), NameRow::Call, None) {
            let key = row.expect("row").0;
            last_only.push(key.to_vec());
            held = Some(key);
        }
        drop(held);
        assert_eq!(last_only, all, "a reader holding the last key gets the same keys");

        let cursor = call_by_callee_hit("f", &shared, 1, &x).unwrap();
        let (lower, resume) = resume_after(Some(&cursor), NameRow::Call);
        assert_eq!(lower, Bound::Included(stored[0].0.clone()));
        let rest = hits(ExpandHits::new(rows(&stored), Arc::clone(&live), NameRow::Call, resume));
        assert_eq!(
            rest,
            vec![hit(&y, 1), hit(&x, 5), hit(&y, 5), None, None],
            "resumes inside the row"
        );

        let cursor = call_by_callee("f", &dead, 1).unwrap();
        let (lower, resume) = resume_after(Some(&cursor), NameRow::Call);
        assert_eq!(lower, Bound::Included(stored[1].0.clone()));
        let rest = keys(ExpandHits::new(rows(&stored[1..]), live, NameRow::Call, resume));
        assert_eq!(
            rest,
            vec![call_by_callee("f", &dead, 2).unwrap()],
            "a bare site cursor resumes after that site"
        );
    }

    #[test]
    fn a_symbol_row_is_its_own_site() {
        let content = content_key("a", "rust", 1);
        let (x, y) = (RelPath::from("x.rs"), RelPath::from("y.rs"));
        let live = Arc::new(LiveKeys::from_entries([(x.clone(), content), (y.clone(), content)]));
        let stored = [(
            symbol_by_name("alpha", SymbolKind::Function, &content, 7).unwrap(),
            Vec::new(),
        )];
        let got = keys(ExpandHits::new(rows(&stored), live, NameRow::Symbol, None));
        assert_eq!(
            got,
            vec![
                symbol_by_name_hit("alpha", SymbolKind::Function, &content, 7, &x).unwrap(),
                symbol_by_name_hit("alpha", SymbolKind::Function, &content, 7, &y).unwrap(),
            ]
        );
    }

    #[test]
    fn a_bucketed_lookup_finds_exactly_the_live_keys() {
        assert!(LiveKeys::default().holders(&content_key("a", "rust", 1)).is_empty());
        for live_count in [0, 1, 2, 3, 7, 64, 100] {
            let keys: Vec<ContentKey> = (0..live_count * 2)
                .map(|at| content_key(&format!("h{at}"), "rust", 1))
                .collect();
            let live = LiveKeys::from_entries(
                keys.iter()
                    .step_by(2)
                    .enumerate()
                    .map(|(at, key)| (RelPath::from(format!("f{at}.rs").as_str()), *key)),
            );
            for (at, key) in keys.iter().enumerate() {
                let held = live.holders(key);
                assert_eq!(held.len(), usize::from(at % 2 == 0), "{live_count} live, key {at}");
                assert!(held.iter().all(|(found, _)| found == key));
            }
            for edge in [[0u8; 16], [0xFF; 16]] {
                assert!(live.holders(&edge).is_empty());
            }
        }
    }

    /// A cursor answers what a lookup answers, whatever order the keys come in: ascending (one name's
    /// rows), restarting lower (the next symbol kind), or skipping far ahead.
    #[test]
    fn a_cursor_finds_exactly_what_a_lookup_finds() {
        let keys: Vec<ContentKey> = (0..400).map(|at| content_key(&format!("h{at}"), "rust", 1)).collect();
        let live = LiveKeys::from_entries(
            keys.iter()
                .enumerate()
                .filter(|(at, _)| at % 3 != 0)
                .map(|(at, key)| (RelPath::from(format!("f{at}.rs").as_str()), *key)),
        );
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        let sparse: Vec<ContentKey> = sorted.iter().step_by(37).copied().collect();
        let orders = [
            sorted.clone(),
            sorted.iter().rev().copied().collect(),
            [&sorted[200..], &sorted[..200]].concat(),
            sparse,
            keys.clone(),
            vec![[0u8; 16], [0xFF; 16], [0u8; 16]],
        ];
        for (at, order) in orders.iter().enumerate() {
            let mut cursor = live.cursor();
            for key in order {
                assert_eq!(cursor.holders(key), live.holders(key), "order {at}");
            }
        }
        let mut empty = LiveKeys::default();
        assert!(empty.cursor().holders(&keys[0]).is_empty());
        empty = LiveKeys::from_entries([]);
        assert!(empty.cursor().holders(&keys[0]).is_empty());
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
