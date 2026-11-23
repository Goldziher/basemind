//! Read side of the git-history index — the posting-list queries the MCP tools call instead of
//! walking history live. Every method returns the same [`CommitInfo`] shape the live walk produced,
//! so a tool only swaps its data source; response structs and pagination are unchanged.
//!
//! Each public method answers from the fjall posting lists of the database this process holds.

use ahash::AHashMap;

use super::reach::Reach;
use super::{CommitMeta, GitHistoryError, GitHistoryIndex, LocalDb, encoding, keys};
use crate::git::CommitInfo;
use crate::path::RelPath;

/// Per-call cache resolving `path_id → RelPath` so a window scan resolves each distinct path once.
type PathCache<'a> = AHashMap<u32, RelPath>;

/// The index as one head sees it: the stored commits that head reaches, newest-first. Obtained from
/// [`GitHistoryIndex::at`]; answers match the live walk at that head.
pub struct HistoryAt<'a> {
    db: &'a LocalDb,
    head: [u8; 20],
    reach: Reach,
}

impl GitHistoryIndex {
    /// The history as `head` (a 40-char hex sha) sees it, or `None` when the index cannot answer for
    /// it in the live walk's order: `head` is not an indexed tip (a worktree the index has not synced
    /// yet, a head dropped past [`MAX_TIPS`](super::MAX_TIPS), or a build in progress), or it is a tip
    /// whose order the index could not prove. The caller reads by the live walk.
    ///
    /// The index lists a head's commits by descending ordinal, the order they were appended in, while
    /// the live walk lists them newest commit time first. A sync marks a tip ordered only when the
    /// two are provably the same list:
    /// - after a walk from `head` alone (a full build, or a head with no indexed ancestor tip), when
    ///   the ordinals of that very walk, read in walk order, strictly descend; or
    /// - after an extension of an ordered tip, when the commits added are a linear chain of
    ///   single-parent commits from `head` down to that tip, none stored before. The walk of `head`
    ///   then has a one-commit frontier throughout: the chain, then the walk of the tip, and the added
    ///   ordinals all exceed the tip's.
    ///
    /// A head reached after a branch switch or a merge, or that adds a merge, fails both and is read
    /// live: the order of an index that serves many heads is not a property of the head.
    pub fn at(&self, head: &str) -> Option<HistoryAt<'_>> {
        let head = keys::sha_hex_to_raw(head)?;
        if !self.db.tip_is_ordered(&head) {
            return None;
        }
        let reach = self.db.reach_of(&head)?;
        Some(HistoryAt {
            db: &self.db,
            head,
            reach,
        })
    }

    /// The history as the most recently synced tip sees it.
    pub fn latest(&self) -> Option<HistoryAt<'_>> {
        self.at(&self.last_indexed_head_hex()?)
    }

    /// [`HistoryAt::commits_touching`] at the most recently synced tip: empty when there is none.
    pub fn commits_touching(
        &self,
        rel: &RelPath,
        skip: usize,
        take: usize,
    ) -> Result<Vec<CommitInfo>, GitHistoryError> {
        self.latest()
            .map_or_else(|| Ok(Vec::new()), |view| view.commits_touching(rel, skip, take))
    }

    /// [`HistoryAt::recent_commits`] at the most recently synced tip: empty when there is none.
    pub fn recent_commits(&self, skip: usize, take: usize, include_files: bool) -> Vec<CommitInfo> {
        self.latest()
            .map_or_else(Vec::new, |view| view.recent_commits(skip, take, include_files))
    }

    /// [`HistoryAt::window_commits`] at the most recently synced tip: empty when there is none.
    pub fn window_commits(&self, window: usize) -> Vec<CommitInfo> {
        self.latest().map_or_else(Vec::new, |view| view.window_commits(window))
    }
}

impl HistoryAt<'_> {
    /// Commits that touched `rel`, newest-first, after skipping `skip` and taking at most `take`.
    /// Files are omitted (parity with the live `commits_touching`, which passes `include_files=false`).
    /// An empty list is the answer for a path no indexed commit touches; a lookup that failed is an
    /// error, never an empty list.
    pub fn commits_touching(
        &self,
        rel: &RelPath,
        skip: usize,
        take: usize,
    ) -> Result<Vec<CommitInfo>, GitHistoryError> {
        self.db.commits_touching(&self.reach, rel, skip, take)
    }

    /// Newest-first global commit log (the source for `recent_changes`).
    pub fn recent_commits(&self, skip: usize, take: usize, include_files: bool) -> Vec<CommitInfo> {
        self.db.recent_commits(&self.reach, skip, take, include_files)
    }

    /// The newest `window` commits, with files resolved — the input for `find_commits_by_path`
    /// (regex over paths) and `hot_files` (churn aggregation).
    pub fn window_commits(&self, window: usize) -> Vec<CommitInfo> {
        self.db.window_commits(&self.reach, window)
    }

    /// Whether the index still holds this view's head as the tip it was read from. The reads take no
    /// lock, so a sync that wipes and rebuilds the index while they run can make a path with history
    /// read as untouched: an answer counts only when this still holds after it.
    pub fn is_current(&self) -> bool {
        self.db.reach_of(&self.head).as_ref() == Some(&self.reach)
    }

    /// The ordinals this head reaches, so the full-text search can keep only its commits.
    pub(super) fn parts(&self) -> (&LocalDb, &Reach) {
        (self.db, &self.reach)
    }
}

/// Decode `buf`'s ordinals newest-first, keeping those `reach` holds, until `want` are kept or the
/// list ends. The window grows geometrically: a head that reaches nearly every indexed commit decodes
/// about `want` varints; one that reaches few decodes on until it has them.
fn reachable_ords(buf: &[u8], reach: &Reach, want: usize) -> Option<Vec<u32>> {
    let mut window = want.max(MIN_WINDOW);
    loop {
        let ords = encoding::try_decode_ords_head(buf, window)?;
        let exhausted = ords.len() < window;
        let mut kept: Vec<u32> = ords.into_iter().filter(|ord| reach.contains(*ord)).collect();
        if exhausted || kept.len() >= want {
            kept.truncate(want);
            return Some(kept);
        }
        window = window.saturating_mul(WINDOW_GROWTH);
    }
}

/// Smallest posting window decoded, and the factor it grows by while the head reaches too few.
const MIN_WINDOW: usize = 16;
const WINDOW_GROWTH: usize = 4;

impl LocalDb {
    fn commits_touching(
        &self,
        reach: &Reach,
        rel: &RelPath,
        skip: usize,
        take: usize,
    ) -> Result<Vec<CommitInfo>, GitHistoryError> {
        let Some(path_id) = self.stored_path_id(rel)? else {
            return Ok(Vec::new());
        };
        let Some(buf) = self.stored_posting(path_id)? else {
            return Ok(Vec::new());
        };
        let ords = reachable_ords(&buf, reach, skip.saturating_add(take))
            .ok_or_else(|| GitHistoryError::Corrupt(format!("the posting list of `{rel}` does not decode")))?;
        let mut cache = PathCache::new();
        ords.into_iter()
            .skip(skip)
            .take(take)
            .map(|ord| {
                self.stored_commit(ord, false)
                    .map(|meta| self.meta_to_info(meta, &mut cache))
            })
            .collect()
    }

    fn recent_commits(&self, reach: &Reach, skip: usize, take: usize, include_files: bool) -> Vec<CommitInfo> {
        let mut cache = PathCache::new();
        // The skipped commits are decoded head-only; only the page's file lists are decoded.
        self.commits_desc(false)
            .filter(|(ord, _)| reach.contains(*ord))
            .skip(skip)
            .take(take)
            .filter_map(|(ord, head)| {
                if include_files {
                    self.commit_meta(ord, true)
                } else {
                    Some(head)
                }
            })
            .map(|meta| self.meta_to_info(meta, &mut cache))
            .collect()
    }

    fn window_commits(&self, reach: &Reach, window: usize) -> Vec<CommitInfo> {
        let mut cache = PathCache::new();
        self.commits_desc(true)
            .filter(|(ord, _)| reach.contains(*ord))
            .take(window)
            .map(|(_, meta)| self.meta_to_info(meta, &mut cache))
            .collect()
    }

    /// Reconstruct a [`CommitInfo`] from stored metadata, resolving interned path ids on demand.
    /// Consumes `meta` by value so the decoded `sha` / `summary` / `author` strings move straight
    /// into the result instead of being cloned (they were just allocated by the decode). `files` is
    /// whatever the decode produced — empty when the value was decoded head-only.
    pub(crate) fn meta_to_info(&self, meta: CommitMeta, cache: &mut PathCache) -> CommitInfo {
        let short_sha = meta.sha[..7.min(meta.sha.len())].to_string();
        let files = meta
            .files
            .iter()
            .filter_map(|&(path_id, kind_byte)| {
                self.resolve_path(path_id, cache)
                    .map(|rel| (rel, keys::change_kind_from_byte(kind_byte)))
            })
            .collect();
        CommitInfo {
            sha: meta.sha,
            short_sha,
            summary: meta.summary,
            author: meta.author,
            author_email: meta.author_email,
            author_time_unix: meta.author_time_unix,
            body: String::new(),
            files,
        }
    }

    fn resolve_path(&self, path_id: u32, cache: &mut PathCache) -> Option<RelPath> {
        if let Some(rel) = cache.get(&path_id) {
            return Some(rel.clone());
        }
        let rel = self.path_for_id(path_id)?;
        cache.insert(path_id, rel.clone());
        Some(rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_posting_whose_head_does_not_decode_is_an_error_not_a_short_history() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = GitHistoryIndex::open(dir.path()).expect("open the index");
        let rel = RelPath::from("src/lib.rs");
        let head = [1u8; 20];
        let mut writer = index.writer().expect("writer");
        writer.set_ordered(true);
        writer
            .finish_meta(&head, &Reach::from_ords([3, 4]), &head, 5, 2, 2)
            .expect("stamp an ordered tip that reaches both ordinals");
        let db = index.local();
        let key = keys::path_id_by_path_key(&rel).expect("a path key");
        db.path_id_by_path
            .insert(key, keys::u32_key(1))
            .expect("intern the path");
        for posting in [vec![0x80], [encoding::encode_ords(&[3, 4]), vec![0x80]].concat()] {
            db.path_to_ords
                .insert(keys::u32_key(1), posting.as_slice())
                .expect("write the posting");
            let Err(GitHistoryError::Corrupt(message)) = index.commits_touching(&rel, 0, 5) else {
                panic!("a truncated posting must be reported as corrupt");
            };
            assert_eq!(message, "the posting list of `src/lib.rs` does not decode");
        }
    }

    #[test]
    fn a_posting_is_read_down_to_the_commits_the_head_reaches() {
        let ords: Vec<u32> = (0..200).collect();
        let buf = encoding::encode_ords(&ords);
        let even = Reach::from_ords((0..200).filter(|ord| ord % 2 == 0));

        let page = reachable_ords(&buf, &even, 5).expect("decodes");
        assert_eq!(page, vec![198, 196, 194, 192, 190], "newest-first, reachable only");
        let none = Reach::from_ords([1_000]);
        assert_eq!(
            reachable_ords(&buf, &none, 5),
            Some(Vec::new()),
            "a head reaching none of it"
        );
        let all = reachable_ords(&buf, &Reach::from_ords(0..200), usize::MAX).expect("decodes");
        assert_eq!(all, ords.iter().rev().copied().collect::<Vec<_>>());
    }
}
