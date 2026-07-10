//! Read side of the git-history index — the posting-list queries the MCP tools call instead of
//! walking history live. Every method returns the same [`CommitInfo`] shape the live walk produced,
//! so a tool only swaps its data source; response structs and pagination are unchanged.

use ahash::AHashMap;

use super::{CommitMeta, GitHistoryIndex, encoding, keys};
use crate::git::CommitInfo;
use crate::path::RelPath;

/// Per-call cache resolving `path_id → RelPath` so a window scan resolves each distinct path once.
type PathCache<'a> = AHashMap<u32, RelPath>;

impl GitHistoryIndex {
    /// Commits that touched `rel`, newest-first, after skipping `skip` and taking at most `take`.
    /// Files are omitted (parity with the live `commits_touching`, which passes `include_files=false`).
    pub fn commits_touching(&self, rel: &RelPath, skip: usize, take: usize) -> Vec<CommitInfo> {
        let Some(path_id) = self.path_id(rel) else {
            return Vec::new();
        };
        let Some(buf) = self.posting_bytes(path_id) else {
            return Vec::new();
        };
        let ords = encoding::decode_ords_head(&buf, skip.saturating_add(take));
        let mut cache = PathCache::new();
        ords.into_iter()
            .skip(skip)
            .take(take)
            .filter_map(|ord| self.commit_meta(ord, false))
            .map(|meta| self.meta_to_info(meta, &mut cache))
            .collect()
    }

    /// Newest-first global commit log (the source for `recent_changes`).
    pub fn recent_commits(&self, skip: usize, take: usize, include_files: bool) -> Vec<CommitInfo> {
        let mut cache = PathCache::new();
        self.commits_desc(include_files)
            .skip(skip)
            .take(take)
            .map(|(_, meta)| self.meta_to_info(meta, &mut cache))
            .collect()
    }

    /// The newest `window` commits, with files resolved — the input for `find_commits_by_path`
    /// (regex over paths) and `hot_files` (churn aggregation).
    pub fn window_commits(&self, window: usize) -> Vec<CommitInfo> {
        let mut cache = PathCache::new();
        self.commits_desc(true)
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
