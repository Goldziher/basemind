//! Precomputed git-history index — a separate, repo-level Fjall store that turns the
//! history MCP tools (`commits_touching`, `recent_changes`, `find_commits_by_path`,
//! `hot_files`, and the commit-walk of `symbol_history`) from live history walks into
//! posting-list lookups.
//!
//! ## Why a separate DB
//!
//! Lives at `.basemind/git-history.fjall/` — a sibling of `views/` and `git-cache/`, NOT inside a
//! per-view `index.fjall`. Git history is repo-global (identical across the working/staged/rev
//! views) and immortal/append-only as HEAD advances, so it carries its own [`GIT_HISTORY_SCHEMA`]
//! and survives an `INDEX_SCHEMA_VER` bump — a code-map schema change must never throw away the
//! expensive 200k-commit walk.
//!
//! ## Single-writer
//!
//! Fjall takes an exclusive per-directory process lock, so only the process holding
//! `.basemind/.lock` (a `scan`/`rescan`, or a writable `serve`) opens this DB. A read-only serve
//! keeps `git_history: None` and falls back to the (now-fast) live walk. The index is a pure
//! accelerator — tools use it only when `last_indexed_head == HEAD` and otherwise live-walk, so it
//! can never serve stale or incorrect results.
//!
//! ## Partitions
//!
//! | partition | key | value |
//! |---|---|---|
//! | `gh_meta` | fixed byte-strings | schema_ver, last_indexed_head, counters, fingerprint |
//! | `gh_commit_by_ord` | `ord:u32_be` | msgpack [`CommitMeta`] (interned file list) |
//! | `gh_ord_by_sha` | `sha:20 raw` | `ord:u32_be` |
//! | `gh_path_id_by_path` | `u16:len ‖ rel` | `path_id:u32_be` |
//! | `gh_path_by_id` | `path_id:u32_be` | raw `rel` bytes |
//! | `gh_path_to_ords` | `path_id:u32_be` | delta-varint ordinal posting list |
//! | `gh_commit_text_by_ord` | `ord:u32_be` | full commit message body (FTS) |
//! | `gh_term_to_ords` | `field:u8 ‖ term` | delta-varint ordinal posting list (FTS) |

pub mod builder;
pub mod encoding;
pub mod fts;
pub mod keys;
pub mod reader;

use std::path::{Path, PathBuf};

use fjall::{Database, Keyspace, KeyspaceCreateOptions, OwnedWriteBatch};
use thiserror::Error;

use crate::path::RelPath;

/// On-disk schema for the git-history store. Offset from [`crate::version::RELEASE_MINOR`] (the
/// code index owns `+2`, git_cache `+1`), so it moves with each minor release but is independent of
/// the code-map index schema. Mismatch on open wipes `git-history.fjall/` and the next scan rebuilds.
///
/// The offset is `+5`: bumped from `+4` when the stored commit-meta head gained an `author_email`
/// field (`sha ‖ time ‖ author ‖ email ‖ summary ‖ files`) for git-history full-text search. The
/// prior `+4` bump switched the posting-list byte format from ascending (full-scan tail) to
/// **newest-first** delta-varints (O(n) head decode). The format is part of this still-unreleased
/// feature, so the bump only forces in-flight dev indexes to rebuild.
pub const GIT_HISTORY_SCHEMA: u32 = crate::version::RELEASE_MINOR as u32 + 5;

/// On-disk directory name of the git-history index under `.basemind/`. `pub(crate)` so the cache
/// accounting in [`crate::store_gc::cache_stats`] can size it (it is a sibling of `views/`).
pub(crate) const GIT_HISTORY_DIR: &str = "git-history.fjall";

/// Resolve the per-workspace cache directory that should hold the git-history index for a checkout
/// rooted at `root`.
///
/// The index is derived entirely from the shared `.git` object database — every linked worktree of a
/// clone sees the identical commit graph — so a **linked worktree shares the MAIN worktree's index**
/// rather than rebuilding its own. It does this by keying the (now machine-global) workspace cache
/// on the MAIN worktree root, turning a per-worktree git-history rebuild (seconds to minutes on a
/// large repo) into a one-time cost shared across every worktree.
///
/// Falls back to `root`'s own workspace cache dir when `root` is not inside a git repository.
pub fn shared_history_basemind_dir(root: &std::path::Path) -> std::path::PathBuf {
    let base = match crate::git::Repo::discover(root) {
        Ok(repo) if repo.is_linked_worktree() => repo.main_worktree_root(),
        _ => root.to_path_buf(),
    };
    crate::store::workspace_cache_dir(&base)
}

/// Retry budget + backoff for a transient fjall `Locked` when opening the git-history DB. The
/// caller is writer-gated, so any `Locked` is a short-lived concurrent open that clears — mirrors
/// `store::INDEX_OPEN_RETRIES` / `INDEX_OPEN_BACKOFF`.
const GH_OPEN_RETRIES: u32 = 5;
const GH_OPEN_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

/// Whether the git-history index is enabled for this process. On by default; set
/// `BASEMIND_GH_INDEX=0` to disable it (the history tools then fall back to the live walk). The
/// `scan` / `rescan` CLIs additionally honor a `--no-git-history` flag.
pub fn index_enabled() -> bool {
    std::env::var("BASEMIND_GH_INDEX").map(|v| v != "0").unwrap_or(true)
}

#[derive(Debug, Error)]
pub enum GitHistoryError {
    #[error("fjall error: {0}")]
    Fjall(#[from] fjall::Error),
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("msgpack encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("msgpack decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("git error: {0}")]
    Git(#[from] crate::git::GitError),
}

/// Per-commit metadata stored in `gh_commit_by_ord`. File paths are interned to `path_id` (u32) so
/// the change edges are not duplicated as full path strings (the key size control on a monorepo).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommitMeta {
    /// 40-char hex sha (read-hot — kept hex so views don't re-encode on every query).
    pub sha: String,
    pub summary: String,
    pub author: String,
    /// Author email — a head field alongside `author`, for git-history full-text search.
    pub author_email: String,
    pub author_time_unix: i64,
    /// `(path_id, change_kind_byte)` for each path the commit changed.
    pub files: Vec<(u32, u8)>,
}

/// Handle to every git-history partition. Cloned cheaply (each `Keyspace` is `Arc`'d by Fjall).
#[derive(Clone)]
pub struct GitHistoryIndex {
    db: Database,
    meta: Keyspace,
    commit_by_ord: Keyspace,
    ord_by_sha: Keyspace,
    path_id_by_path: Keyspace,
    path_by_id: Keyspace,
    path_to_ords: Keyspace,
    /// `gh_commit_text_by_ord`: ord → full commit message body. Kept out of `gh_commit_by_ord` so
    /// the head-decode hot path (`recent_changes` / `commits_touching`) never loads body bytes;
    /// read only when a full-text search returns a commit.
    commit_text_by_ord: Keyspace,
    /// `gh_term_to_ords`: `field:u8 ‖ term` → newest-first ordinal posting list. Backs
    /// `search_git_history`. See [`fts`].
    term_to_ords: Keyspace,
}

impl GitHistoryIndex {
    /// Open (or create) `.basemind/git-history.fjall/`. On schema mismatch the directory is wiped
    /// and recreated empty (the caller rebuilds via the builder). Returns `Err` if another process
    /// holds the Fjall lock — read-only callers swallow that to `None`.
    pub fn open(basemind_dir: &Path) -> Result<Self, GitHistoryError> {
        let dir = basemind_dir.join(GIT_HISTORY_DIR);
        let mut attempt = 0;
        loop {
            match Self::open_at(&dir) {
                Ok(index) => return Ok(index),
                Err(GitHistoryError::Fjall(fjall::Error::Locked)) if attempt < GH_OPEN_RETRIES => {
                    attempt += 1;
                    std::thread::sleep(GH_OPEN_BACKOFF);
                }
                Err(other) => return Err(other),
            }
        }
    }

    fn open_at(dir: &Path) -> Result<Self, GitHistoryError> {
        std::fs::create_dir_all(dir).map_err(|source| GitHistoryError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let mut db = Database::builder(dir).open()?;
        let mut meta = db.keyspace("gh_meta", KeyspaceCreateOptions::default)?;
        let on_disk_ver = meta.get(keys::META_SCHEMA_VER)?.and_then(|b| keys::parse_u32(&b));
        if matches!(on_disk_ver, Some(ver) if ver != GIT_HISTORY_SCHEMA) {
            drop(meta);
            drop(db);
            std::fs::remove_dir_all(dir).map_err(|source| GitHistoryError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
            std::fs::create_dir_all(dir).map_err(|source| GitHistoryError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
            db = Database::builder(dir).open()?;
            meta = db.keyspace("gh_meta", KeyspaceCreateOptions::default)?;
        }
        let commit_by_ord = db.keyspace("gh_commit_by_ord", KeyspaceCreateOptions::default)?;
        let ord_by_sha = db.keyspace("gh_ord_by_sha", KeyspaceCreateOptions::default)?;
        let path_id_by_path = db.keyspace("gh_path_id_by_path", KeyspaceCreateOptions::default)?;
        let path_by_id = db.keyspace("gh_path_by_id", KeyspaceCreateOptions::default)?;
        let path_to_ords = db.keyspace("gh_path_to_ords", KeyspaceCreateOptions::default)?;
        let commit_text_by_ord = db.keyspace("gh_commit_text_by_ord", KeyspaceCreateOptions::default)?;
        let term_to_ords = db.keyspace("gh_term_to_ords", KeyspaceCreateOptions::default)?;
        meta.insert(keys::META_SCHEMA_VER, GIT_HISTORY_SCHEMA.to_be_bytes())?;
        Ok(Self {
            db,
            meta,
            commit_by_ord,
            ord_by_sha,
            path_id_by_path,
            path_by_id,
            path_to_ords,
            commit_text_by_ord,
            term_to_ords,
        })
    }

    /// Drop all data and reset to an empty index at the current schema. Used by `revalidate` when a
    /// history rewrite is detected and by the manual force-rebuild path. Safe because only the
    /// `.basemind/.lock` holder ever opens this DB, so no other process holds a handle.
    pub fn clear(&self, basemind_dir: &Path) -> Result<(), GitHistoryError> {
        let dir = basemind_dir.join(GIT_HISTORY_DIR);
        for ks in [
            &self.commit_by_ord,
            &self.ord_by_sha,
            &self.path_id_by_path,
            &self.path_by_id,
            &self.path_to_ords,
            &self.commit_text_by_ord,
            &self.term_to_ords,
            &self.meta,
        ] {
            let keys: Vec<_> = ks.iter().filter_map(|g| g.into_inner().ok().map(|(k, _)| k)).collect();
            for k in keys {
                ks.remove(k)?;
            }
        }
        let _ = dir;
        self.meta
            .insert(keys::META_SCHEMA_VER, GIT_HISTORY_SCHEMA.to_be_bytes())?;
        Ok(())
    }

    /// Flush every keyspace's memtable to a disk segment and major-compact it. After a bulk build
    /// the data otherwise lingers in the write-ahead journal (hundreds of MB on a deep repo); this
    /// reclaims it into minimal, query-ready segments. One-time cost on a full rebuild; cheap on an
    /// incremental append.
    fn compact(&self) -> Result<(), GitHistoryError> {
        for keyspace in [
            &self.meta,
            &self.commit_by_ord,
            &self.ord_by_sha,
            &self.path_id_by_path,
            &self.path_by_id,
            &self.path_to_ords,
            &self.commit_text_by_ord,
            &self.term_to_ords,
        ] {
            keyspace.rotate_memtable_and_wait()?;
            keyspace.major_compact()?;
        }
        Ok(())
    }

    /// A batched writer over the underlying Fjall database.
    pub fn writer(&self) -> GitHistoryWriter {
        GitHistoryWriter {
            index: self.clone(),
            batch: self.db.batch(),
            staged: 0,
        }
    }

    fn meta_u32(&self, key: &[u8]) -> u32 {
        self.meta
            .get(key)
            .ok()
            .flatten()
            .and_then(|b| keys::parse_u32(&b))
            .unwrap_or(0)
    }

    fn meta_sha(&self, key: &[u8]) -> Option<[u8; 20]> {
        let bytes = self.meta.get(key).ok().flatten()?;
        <[u8; 20]>::try_from(bytes.as_ref()).ok()
    }

    /// Last HEAD the index was synced to (20 raw sha bytes), or `None` if never built.
    pub fn last_indexed_head(&self) -> Option<[u8; 20]> {
        self.meta_sha(keys::META_LAST_HEAD)
    }

    /// Last indexed HEAD as a 40-char hex string — the freshness key the MCP tools compare HEAD to.
    pub fn last_indexed_head_hex(&self) -> Option<String> {
        self.last_indexed_head().map(|s| keys::sha_raw_to_hex(&s))
    }

    /// Next free commit ordinal (also the count of indexed commits).
    pub fn next_ord(&self) -> u32 {
        self.meta_u32(keys::META_NEXT_ORD)
    }

    /// Next free path id.
    pub fn next_path_id(&self) -> u32 {
        self.meta_u32(keys::META_NEXT_PATH_ID)
    }

    /// Fingerprint: sha of the oldest reachable (root) commit at build time.
    pub fn root_sha(&self) -> Option<[u8; 20]> {
        self.meta_sha(keys::META_ROOT_SHA)
    }

    /// Fingerprint: number of commits indexed.
    pub fn commit_count(&self) -> u32 {
        self.meta_u32(keys::META_COMMIT_COUNT)
    }

    /// True when nothing has been indexed yet.
    pub fn is_empty(&self) -> bool {
        self.last_indexed_head().is_none()
    }

    /// Point-read one commit's stored metadata. `want_files=false` decodes only the head fields
    /// (sha / time / author / summary), skipping the changed-file list and its allocation — the
    /// reader passes it for `include_files=false` tools like `commits_touching`.
    pub(crate) fn commit_meta(&self, ord: u32, want_files: bool) -> Option<CommitMeta> {
        let bytes = self.commit_by_ord.get(keys::u32_key(ord)).ok().flatten()?;
        decode_commit_value(&bytes, want_files)
    }

    pub(crate) fn ord_for_sha(&self, sha20: &[u8; 20]) -> Option<u32> {
        let bytes = self.ord_by_sha.get(sha20).ok().flatten()?;
        keys::parse_u32(&bytes)
    }

    pub(crate) fn path_id(&self, rel: &RelPath) -> Option<u32> {
        let key = keys::path_id_by_path_key(rel)?;
        let bytes = self.path_id_by_path.get(&key).ok().flatten()?;
        keys::parse_u32(&bytes)
    }

    pub(crate) fn path_for_id(&self, path_id: u32) -> Option<RelPath> {
        let bytes = self.path_by_id.get(keys::u32_key(path_id)).ok().flatten()?;
        Some(RelPath::from(bytes.as_ref()))
    }

    pub(crate) fn posting_bytes(&self, path_id: u32) -> Option<fjall::Slice> {
        self.path_to_ords.get(keys::u32_key(path_id)).ok().flatten()
    }

    /// Raw newest-first posting bytes for one `(field, term)` search key, or `None` if unindexed.
    pub(crate) fn term_posting_bytes(&self, term_key: &[u8]) -> Option<fjall::Slice> {
        self.term_to_ords.get(term_key).ok().flatten()
    }

    /// The stored full message body for `ord`, or `None` when the commit had a summary-only message
    /// (no body row is written for those).
    pub(crate) fn commit_text(&self, ord: u32) -> Option<String> {
        let bytes = self.commit_text_by_ord.get(keys::u32_key(ord)).ok().flatten()?;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Iterate `gh_commit_by_ord` descending (newest ordinal first) — the source for the global
    /// `recent_changes` / `hot_files` / `find_commits_by_path` window scans. `want_files=false`
    /// skips the changed-file decode for the callers that don't need it.
    pub(crate) fn commits_desc(&self, want_files: bool) -> impl Iterator<Item = (u32, CommitMeta)> + '_ {
        self.commit_by_ord.iter().rev().filter_map(move |g| {
            let (k, v) = g.into_inner().ok()?;
            let ord = keys::parse_u32(&k)?;
            let meta = decode_commit_value(&v, want_files)?;
            Some((ord, meta))
        })
    }
}

/// Decode a stored `gh_commit_by_ord` value into the in-memory [`CommitMeta`] (hex sha + owned
/// strings). Shared by the point-read and the descending-scan paths so they can never diverge.
/// `want_files=false` decodes only the head fields and leaves `files` empty, skipping the per-edge
/// delta loop and the `Vec` allocation for the `include_files=false` read paths.
fn decode_commit_value(bytes: &[u8], want_files: bool) -> Option<CommitMeta> {
    if want_files {
        let decoded = encoding::decode_commit_meta(bytes)?;
        Some(CommitMeta {
            sha: keys::sha_raw_to_hex(&decoded.sha20),
            summary: String::from_utf8_lossy(decoded.summary).into_owned(),
            author: String::from_utf8_lossy(decoded.author).into_owned(),
            author_email: String::from_utf8_lossy(decoded.author_email).into_owned(),
            author_time_unix: decoded.author_time_unix,
            files: decoded.files,
        })
    } else {
        let head = encoding::decode_commit_meta_head(bytes)?;
        Some(CommitMeta {
            sha: keys::sha_raw_to_hex(&head.sha20),
            summary: String::from_utf8_lossy(head.summary).into_owned(),
            author: String::from_utf8_lossy(head.author).into_owned(),
            author_email: String::from_utf8_lossy(head.author_email).into_owned(),
            author_time_unix: head.author_time_unix,
            files: Vec::new(),
        })
    }
}

/// Batched writer for the git-history index. Commits the accumulated batch every
/// `COMMIT_BATCH` staged operations so a 200k-commit rebuild doesn't hold the whole write set in
/// memory. Callers must call `GitHistoryWriter::commit` at the end to flush the tail.
pub struct GitHistoryWriter {
    index: GitHistoryIndex,
    batch: OwnedWriteBatch,
    staged: usize,
}

/// Flush the Fjall batch every N staged ops (mirrors the scanner's `INDEX_COMMIT_BATCH`).
const COMMIT_BATCH: usize = 4096;

impl GitHistoryWriter {
    pub fn put_commit_meta(&mut self, ord: u32, meta: &CommitMeta) -> Result<(), GitHistoryError> {
        let sha20 = keys::sha_hex_to_raw(&meta.sha).unwrap_or([0u8; 20]);
        let value = encoding::encode_commit_meta(
            &sha20,
            meta.author_time_unix,
            meta.author.as_bytes(),
            meta.author_email.as_bytes(),
            meta.summary.as_bytes(),
            &meta.files,
        );
        self.batch.insert(&self.index.commit_by_ord, keys::u32_key(ord), value);
        self.maybe_flush()
    }

    pub fn put_ord_for_sha(&mut self, sha20: &[u8; 20], ord: u32) -> Result<(), GitHistoryError> {
        self.batch.insert(&self.index.ord_by_sha, *sha20, keys::u32_key(ord));
        self.maybe_flush()
    }

    pub fn put_path(&mut self, rel: &RelPath, path_id: u32) -> Result<(), GitHistoryError> {
        if let Some(key) = keys::path_id_by_path_key(rel) {
            self.batch
                .insert(&self.index.path_id_by_path, key, keys::u32_key(path_id));
        }
        self.batch
            .insert(&self.index.path_by_id, keys::u32_key(path_id), rel.as_bytes().to_vec());
        self.maybe_flush()
    }

    pub fn put_posting(&mut self, path_id: u32, encoded: &[u8]) -> Result<(), GitHistoryError> {
        self.batch
            .insert(&self.index.path_to_ords, keys::u32_key(path_id), encoded.to_vec());
        self.maybe_flush()
    }

    /// Store one commit's full message body (skipped by the caller when empty).
    pub fn put_commit_text(&mut self, ord: u32, body: &[u8]) -> Result<(), GitHistoryError> {
        self.batch
            .insert(&self.index.commit_text_by_ord, keys::u32_key(ord), body.to_vec());
        self.maybe_flush()
    }

    /// Store the newest-first posting list for one `(field, term)` search key.
    pub fn put_term_posting(&mut self, term_key: &[u8], encoded: &[u8]) -> Result<(), GitHistoryError> {
        self.batch
            .insert(&self.index.term_to_ords, term_key.to_vec(), encoded.to_vec());
        self.maybe_flush()
    }

    fn maybe_flush(&mut self) -> Result<(), GitHistoryError> {
        self.staged += 1;
        if self.staged >= COMMIT_BATCH {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), GitHistoryError> {
        let batch = std::mem::replace(&mut self.batch, self.index.db.batch());
        batch.commit()?;
        self.staged = 0;
        Ok(())
    }

    /// Write the meta fingerprint + counters and flush everything. Call LAST: `last_indexed_head`
    /// becoming visible is the commit point — a crash before this leaves the index looking unbuilt,
    /// so the next revalidate triggers an idempotent full rebuild.
    pub fn finish_meta(
        mut self,
        head: &[u8; 20],
        root: &[u8; 20],
        next_ord: u32,
        next_path_id: u32,
        commit_count: u32,
    ) -> Result<(), GitHistoryError> {
        self.flush()?;
        let mut meta = self.index.db.batch();
        meta.insert(&self.index.meta, keys::META_NEXT_ORD, next_ord.to_be_bytes());
        meta.insert(&self.index.meta, keys::META_NEXT_PATH_ID, next_path_id.to_be_bytes());
        meta.insert(&self.index.meta, keys::META_COMMIT_COUNT, commit_count.to_be_bytes());
        meta.insert(&self.index.meta, keys::META_ROOT_SHA, root.to_vec());
        meta.insert(&self.index.meta, keys::META_LAST_HEAD, head.to_vec());
        meta.commit()?;
        self.index.compact()?;
        Ok(())
    }
}
