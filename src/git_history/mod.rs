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
//! ## Single-writer, and who holds the lock
//!
//! Fjall takes an exclusive per-directory process lock — even a read-only open takes it — so exactly
//! ONE process may hold this DB. Which process that is depends on the deployment:
//!
//! * **Standalone** (no daemon: `scan` / `rescan`, a `serve` that is itself the writer, a one-shot
//!   CLI query): that process opens the DB locally and, if it is a writer,
//!   builds it in-process.
//! * **Daemon** (a `comms` build, where the daemon is the machine's sole fjall writer): the DAEMON
//!   holds the DB and builds it. A session the daemon hosts shares the daemon's handle; a CLI command
//!   forwards its whole call to the daemon. No other process opens the DB: it cannot win a lock the
//!   daemon holds, so it would only stall on the retry ladder and then live-walk forever. And
//!   building it elsewhere would be doubly wrong — it would steal the lock the daemon needs, and it
//!   would run a multi-GB, minutes-long walk inside the process an agent is actively querying.
//!
//! ## Many heads, one store
//!
//! The commit store does not depend on a head: every commit reachable from any worktree HEAD is
//! stored once, under an ordinal assigned in append order, and a divergence never wipes anything. What
//! a head reaches is recorded per indexed tip as a [`reach::Reach`] bitset (`gh_meta` `tip/` rows), and
//! a query at head `X` ([`GitHistoryIndex::at`]) answers from the posting lists keeping only the
//! ordinals `X` reaches. A HEAD move appends only the commits no earlier sync stored. At most
//! [`MAX_TIPS`] tips are kept, the least recently synced dropping first; a head with no tip row is read
//! by the live walk until a sync adds it.
//!
//! A process that can reach neither keeps `git_history: None` and falls back to the live walk. The
//! index is a pure accelerator — tools use it only when HEAD is an indexed tip and otherwise
//! live-walk (reporting `partial: true`), so it can never serve stale or incorrect results. A build
//! or append marks the index in progress before it writes a row and clears the mark in the batch
//! that stamps the new head; while the mark stands the index reports no head, so a run cut off
//! between two batch commits is rebuilt by the next sync instead of being served with commits
//! missing from its posting lists.
//!
//! ## Partitions
//!
//! | partition | key | value |
//! |---|---|---|
//! | `gh_meta` | fixed byte-strings, `tip/ ‖ sha` | schema_ver, last_indexed_head, counters, fingerprint, one reachability bitset per indexed tip |
//! | `gh_commit_by_ord` | `ord:u32_be` | msgpack [`CommitMeta`] (interned file list) |
//! | `gh_ord_by_sha` | `sha:20 raw` | `ord:u32_be` |
//! | `gh_path_id_by_path` | `u16:len ‖ rel` | `path_id:u32_be` |
//! | `gh_path_by_id` | `path_id:u32_be` | raw `rel` bytes |
//! | `gh_path_to_ords` | `path_id:u32_be` | delta-varint ordinal posting list |
//! | `gh_commit_text_by_ord` | `ord:u32_be` | full commit message body (FTS) |
//! | `gh_term_to_ords` | `field:u8 ‖ term` | delta-varint ordinal posting list (FTS) |
//! | `gh_term_dict` | `field:u8 ‖ term` | empty; the term dictionary a typo-tolerant query scans |

pub mod builder;
pub mod encoding;
pub mod fts;
pub mod fuzzy;
pub mod keys;
mod maintenance;
pub mod proto;
pub mod reach;
pub mod reader;
use std::path::{Path, PathBuf};

use fjall::config::CompressionPolicy;
use fjall::{CompressionType, Database, Keyspace, KeyspaceCreateOptions, OwnedWriteBatch};
use thiserror::Error;

use crate::path::RelPath;

/// On-disk schema for the git-history store. Offset from [`crate::version::RELEASE_MINOR`] (the
/// code index owns `+2`, git_cache `+1`), so it moves with each minor release but is independent of
/// the code-map index schema. An older on-disk schema wipes `git-history.fjall/` on open and the next
/// scan rebuilds; a newer one is refused ([`GitHistoryError::NewerSchema`]).
///
/// The offset is `+5`: bumped from `+4` when the stored commit-meta head gained an `author_email`
/// field (`sha ‖ time ‖ author ‖ email ‖ summary ‖ files`) for git-history full-text search. The
/// prior `+4` bump switched the posting-list byte format from ascending (full-scan tail) to
/// **newest-first** delta-varints (O(n) head decode). The format is part of this still-unreleased
/// feature, so the bump only forces in-flight dev indexes to rebuild.
///
/// `+6` switched every keyspace to LZ4 at all levels (fjall's default leaves L0/L1 raw and persists the
/// policy at keyspace creation, so existing stores must be recreated to pick it up).
///
/// `+7` rebuilds the stored file lists, which changed three ways: a move lists its source as
/// deleted, no directory is listed as modified, and a merge lists only what it changed against
/// every parent. An index built before keeps the old lists, and the history of a path read from it
/// would disagree with the live walk.
///
/// `+8` rebuilds them again: a root commit listed every directory of its tree as an added file.
///
/// `+9` recreates every keyspace at `KEYSPACE_MEMTABLE_BYTES`: fjall persists the memtable size
/// at keyspace creation, so a store created at the 64 MiB default would keep it.
///
/// `+10` rebuilds the stored authors: a name that is not UTF-8 was stored as `?` and its email as
/// empty, so every such author was one person. The byte format is unchanged.
///
/// `+11` rebuilds the file list of a shallow clone's boundary commit, which is now listed as a root
/// (its tree added), and stores the clone's `shallow` file so a deepened boundary is noticed.
///
/// `+12` records the indexed tips with their reachability, so one store answers for every worktree
/// HEAD; an index with a single stamped head has no tip rows.
///
/// `+13` adds the `gh_term_dict` keyspace and indexes identifier parts (`BulkConfig` → `bulk`,
/// `config`) beside the whole term, for typo-tolerant search.
pub const GIT_HISTORY_SCHEMA: u32 = crate::version::RELEASE_MINOR as u32 + 13;

/// How many tips the index keeps reachability for. Each is a bitset of one bit per indexed commit
/// (32 KiB at a quarter-million commits), so the bound is a few MiB; past it the tip synced least
/// recently is dropped, and a worktree still at it is read by the live walk until its next sync.
pub const MAX_TIPS: usize = 32;

/// Memtable ceiling of each keyspace, in bytes. At fjall's 64 MiB default, eight keyspaces with up
/// to four sealed memtables queued behind each would let one database (the daemon keeps one per
/// repository) hold gigabytes; 8 MiB is the floor of fjall's recommended band.
const KEYSPACE_MEMTABLE_BYTES: u64 = 8 * 1024 * 1024;

/// Block cache, in bytes, against fjall's 32 MiB default: an indexed query reads a few posting
/// lists and commit heads.
const CACHE_BYTES: u64 = 16 * 1024 * 1024;

/// Ceiling of the journal on disk, in bytes: fjall's minimum, against a 512 MiB default.
const JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

/// Open-file cache of one database, against fjall's 150 (macOS) / 900 (Linux) default. Counted by
/// the daemon's descriptor budget ([`crate::comms::fd_budget`]).
pub(crate) const CACHED_FILES: usize = 64;

/// The database at `dir` under this module's budgets.
fn open_database(dir: &Path) -> Result<Database, fjall::Error> {
    Database::builder(dir)
        .worker_threads(1)
        .cache_size(CACHE_BYTES)
        .max_journaling_size(JOURNAL_BYTES)
        .max_cached_files(Some(CACHED_FILES))
        .open()
}

/// The memtable ceiling, then LZ4 on data and index blocks at every level; see [`crate::index`]'s
/// `index_keyspace_options`. Fjall persists both when a keyspace is created, which is why the
/// ceiling shipped with a [`GIT_HISTORY_SCHEMA`] bump.
fn keyspace_options() -> KeyspaceCreateOptions {
    KeyspaceCreateOptions::default()
        .max_memtable_size(KEYSPACE_MEMTABLE_BYTES)
        .data_block_compression_policy(CompressionPolicy::all(CompressionType::Lz4))
        .index_block_compression_policy(CompressionPolicy::all(CompressionType::Lz4))
}

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
/// Falls back to `root`'s own workspace cache dir when `root` is not inside a git repository, and
/// equally when discovery had to ascend to reach one — a subdirectory workspace must not be handed
/// an ancestor repository's cache directory (see [`history_scope_ok`]).
pub fn shared_history_basemind_dir(root: &std::path::Path) -> std::path::PathBuf {
    let base = match crate::git::Repo::discover(root) {
        Ok(repo) if repo.origin_is_workdir() && repo.is_linked_worktree() => repo.main_worktree_root(),
        _ => root.to_path_buf(),
    };
    crate::store::workspace_cache_dir(&base)
}

/// Escape hatch restoring the pre-#62 behaviour: index an ancestor repository's history for a
/// workspace root that is merely a subdirectory of it.
pub const DISCOVER_PARENTS_ENV: &str = "BASEMIND_GIT_DISCOVER_PARENTS";

/// `0` turns the git-history index off.
pub const HISTORY_INDEX_ENV: &str = "BASEMIND_GH_INDEX";

/// Whether `repo`'s history may be indexed for the workspace it was discovered from.
///
/// The predicate is **`repo.workdir() == the root discovery started from`**, not "the root has a
/// `.git`". They agree on the reported shape (a subdirectory with no `.git` of its own) and the
/// stronger form also covers the cases a `.git` probe would wave through: a `.git` file pointing at
/// a linked worktree elsewhere, a `GIT_DIR`/`GIT_WORK_TREE` environment override, a `.git` directory
/// that is not actually this path's repository. It is also the only form that needs no filesystem
/// probe of its own — `Repo::discover` already knows both halves.
///
/// Why this and not a change to [`Repo::discover`](crate::git::Repo::discover): ascent is what lets
/// `config::discover_root_with_basemind` lift `basemind scan src/` to the repository root, and every
/// CLI verb invoked from a subdirectory depends on it. So the *integration* is gated, not discovery.
///
/// Refusing costs a subdirectory workspace nothing it could correctly have had: the commits it would
/// have indexed are the ancestor repository's, addressed by paths that do not exist under this root.
/// What it saves is the whole ancestor object database — the git-history build walks every reachable
/// commit and diffs every tree, so a 4.4k-file subdirectory of a monorepo was driving gix over
/// 275 MiB of packs and 101 655 objects (issue #62). Set `BASEMIND_GIT_DISCOVER_PARENTS=1` to opt
/// back in.
pub fn history_scope_ok(repo: &crate::git::Repo) -> bool {
    repo.origin_is_workdir() || crate::config::engine::with(|config| config.git.discover_parents)
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
    crate::config::engine::with(|config| config.git.history_index)
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
    /// A stored row is missing or does not decode where the index's own keys say it is.
    #[error("git-history index is corrupt: {0}")]
    Corrupt(String),
    /// The git-history index is disabled by configuration (`BASEMIND_GH_INDEX=0`). Returned by the
    /// daemon's runner for a forwarded op.
    #[error("git-history index disabled (BASEMIND_GH_INDEX=0)")]
    Disabled,
    /// The on-disk index carries a schema newer than [`GIT_HISTORY_SCHEMA`]. Never wiped: it belongs
    /// to a newer basemind sharing this cache.
    #[error(
        "git-history index written by a newer basemind (schema {stored}); this binary understands \
         {current} — upgrade basemind"
    )]
    NewerSchema { stored: u32, current: u32 },
    /// The workspace root a client asked history for is not one basemind will index (issue #62).
    /// The payload is the full operator-facing guidance from
    /// [`refusal_message`](crate::config::root_guard::refusal_message).
    #[error("{0}")]
    RootRefused(String),
    /// The workspace root is a subdirectory of the repository git discovery landed on, so indexing
    /// its history would index an ancestor repository (issue #62). See [`history_scope_ok`].
    #[error(
        "refusing to index git history for {root}: it is a subdirectory of the repository at \
         {workdir}, not a repository of its own. Those commits are addressed by paths outside this \
         root, and walking them costs the whole ancestor object database. Run `git init` here, or \
         set BASEMIND_GIT_DISCOVER_PARENTS=1 to index the ancestor repository's history anyway."
    )]
    ForeignHistory { root: PathBuf, workdir: PathBuf },
    /// A forwarded read named a head the index cannot answer for in the live walk's order: the
    /// index was never built, the head is no indexed tip, or it is a tip whose order is unproven
    /// (see [`GitHistoryIndex::at`]). `None` is the latest indexed tip. The caller live-walks.
    #[error(
        "the git-history index has no ordered view of {}; read the history by the live walk",
        .head.as_deref().unwrap_or("the latest indexed tip")
    )]
    NoOrderedView { head: Option<String> },
}

/// Name prefix of a git-history directory set aside by [`discard_dir`] and not yet deleted.
const DISCARD_PREFIX: &str = "git-history.fjall.discard-";

/// Delete the database directory `dir` so a crash at any point leaves the whole database or none of
/// it at `dir`. A half-deleted one fails every later open, which left the history tools on the live
/// walk for good. Renamed aside first, the rename made durable, then deleted; a deletion that does
/// not finish is completed by [`sweep_discarded`] on a later open.
fn discard_dir(dir: &Path) -> Result<(), GitHistoryError> {
    static SEQUENCE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let aside = dir.with_file_name(format!(
        "{DISCARD_PREFIX}{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::rename(dir, &aside).map_err(|source| GitHistoryError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    if let Some(parent) = dir.parent()
        && let Err(error) = crate::store_layout::sync_directory(parent)
    {
        tracing::warn!(%error, "could not fsync the workspace directory after setting git history aside");
    }
    if let Err(error) = std::fs::remove_dir_all(&aside) {
        tracing::warn!(%error, path = %aside.display(), "git history not fully deleted; the next open retries");
    }
    Ok(())
}

/// Delete what an interrupted [`discard_dir`] left under `basemind_dir`. Best-effort: a leftover
/// costs disk space only.
fn sweep_discarded(basemind_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(basemind_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(DISCARD_PREFIX)
            && let Err(error) = std::fs::remove_dir_all(entry.path())
        {
            tracing::warn!(%error, path = %entry.path().display(), "could not delete discarded git history");
        }
    }
}

/// Bytes of the sync time at the head of a tip row's value.
const TIP_STAMP_BYTES: usize = 8;

/// The tips to drop so that `tips` plus the tip being written (`keep`, which is never dropped) stay
/// within `cap`: the ones synced least recently.
fn evicted_tips(mut tips: Vec<([u8; 20], u64)>, keep: &[u8; 20], cap: usize) -> Vec<[u8; 20]> {
    tips.retain(|(head, _)| head != keep);
    let excess = (tips.len() + 1).saturating_sub(cap);
    tips.sort_by_key(|&(head, stamp)| (stamp, head));
    tips.into_iter().take(excess).map(|(head, _)| head).collect()
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
pub(crate) struct LocalDb {
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
    /// `gh_term_dict`: the keys of `gh_term_to_ords` with empty values, so a typo-tolerant query reads
    /// the vocabulary without reading the posting lists. See [`fuzzy`].
    term_dict: Keyspace,
    /// Serializes syncs of this database (see [`builder::sync`]): every holder of a clone of the
    /// handle (the daemon's socket path, each session it hosts) shares it, so N sessions asking at
    /// once produce one history walk and the rest find the index fresh.
    build_lock: std::sync::Arc<std::sync::Mutex<()>>,
}

/// The git-history index: a handle on the fjall database this process holds.
#[derive(Clone)]
pub struct GitHistoryIndex {
    db: LocalDb,
}

impl GitHistoryIndex {
    /// Open (or create) `.basemind/git-history.fjall/` LOCALLY, taking fjall's exclusive lock. Only
    /// the process that owns the index may call this: a standalone `scan` / `rescan` / writable
    /// `serve`, or the daemon. An OLDER on-disk schema wipes the directory and recreates it empty (the
    /// caller rebuilds via the builder); a NEWER one is left untouched and fails with
    /// [`GitHistoryError::NewerSchema`]. Returns `Err` if another process holds the Fjall lock —
    /// read-only callers swallow that to `None`.
    pub fn open(basemind_dir: &Path) -> Result<Self, GitHistoryError> {
        Self::open_retrying(basemind_dir, GH_OPEN_RETRIES)
    }

    /// [`GitHistoryIndex::open`] with one attempt, for a one-shot query that holds no writer lock:
    /// a `Locked` there is another process that keeps the index, not an open that clears, so the
    /// retry ladder would only delay the live walk the caller falls back to.
    pub fn open_once(basemind_dir: &Path) -> Result<Self, GitHistoryError> {
        Self::open_retrying(basemind_dir, 0)
    }

    fn open_retrying(basemind_dir: &Path, retries: u32) -> Result<Self, GitHistoryError> {
        sweep_discarded(basemind_dir);
        let dir = basemind_dir.join(GIT_HISTORY_DIR);
        let mut attempt = 0;
        loop {
            match Self::open_at(&dir) {
                Ok(index) => return Ok(index),
                Err(GitHistoryError::Fjall(fjall::Error::Locked)) if attempt < retries => {
                    attempt += 1;
                    std::thread::sleep(GH_OPEN_BACKOFF);
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// Whether the index under `basemind_dir` exists. Only a build creates it: a reader that finds
    /// none answers as an unbuilt index (history tools live-walk) instead of creating an empty one.
    pub fn exists(basemind_dir: &Path) -> bool {
        basemind_dir.join(GIT_HISTORY_DIR).is_dir()
    }

    /// The database. Every write path (the builder) and every low-level point read goes through
    /// this.
    pub(crate) fn local(&self) -> &LocalDb {
        &self.db
    }

    /// Hold this database's build lock, so a test can keep a sync waiting on it.
    #[cfg(all(test, feature = "comms", any(unix, windows)))]
    pub(crate) fn hold_build_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.db
            .build_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn open_at(dir: &Path) -> Result<Self, GitHistoryError> {
        std::fs::create_dir_all(dir).map_err(|source| GitHistoryError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let mut db = open_database(dir)?;
        let mut meta = db.keyspace("gh_meta", keyspace_options)?;
        let on_disk_ver = meta.get(keys::META_SCHEMA_VER)?.and_then(|b| keys::parse_u32(&b));
        if let Some(stored) = on_disk_ver.filter(|ver| *ver > GIT_HISTORY_SCHEMA) {
            return Err(GitHistoryError::NewerSchema {
                stored,
                current: GIT_HISTORY_SCHEMA,
            });
        }
        if matches!(on_disk_ver, Some(ver) if ver != GIT_HISTORY_SCHEMA) {
            drop(meta);
            drop(db);
            discard_dir(dir)?;
            std::fs::create_dir_all(dir).map_err(|source| GitHistoryError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
            db = open_database(dir)?;
            meta = db.keyspace("gh_meta", keyspace_options)?;
        }
        let commit_by_ord = db.keyspace("gh_commit_by_ord", keyspace_options)?;
        let ord_by_sha = db.keyspace("gh_ord_by_sha", keyspace_options)?;
        let path_id_by_path = db.keyspace("gh_path_id_by_path", keyspace_options)?;
        let path_by_id = db.keyspace("gh_path_by_id", keyspace_options)?;
        let path_to_ords = db.keyspace("gh_path_to_ords", keyspace_options)?;
        let commit_text_by_ord = db.keyspace("gh_commit_text_by_ord", keyspace_options)?;
        let term_to_ords = db.keyspace("gh_term_to_ords", keyspace_options)?;
        let term_dict = db.keyspace("gh_term_dict", keyspace_options)?;
        meta.insert(keys::META_SCHEMA_VER, GIT_HISTORY_SCHEMA.to_be_bytes())?;
        Ok(Self {
            db: LocalDb {
                db,
                meta,
                commit_by_ord,
                ord_by_sha,
                path_id_by_path,
                path_by_id,
                path_to_ords,
                commit_text_by_ord,
                term_to_ords,
                term_dict,
                build_lock: std::sync::Arc::default(),
            },
        })
    }

    /// Drop all data and reset to an empty index at the current schema. Used by `revalidate` when a
    /// history rewrite is detected and by the manual force-rebuild path. Safe because only the
    /// `.basemind/.lock` holder ever opens this DB, so no other process holds a handle.
    pub fn clear(&self) -> Result<(), GitHistoryError> {
        self.db.clear()
    }

    /// Record that a build or append for `head` failed at `unix_secs`. Best-effort: a record that
    /// cannot be written costs one more rebuild.
    pub(crate) fn record_failed_build(&self, head: &[u8; 20], unix_secs: u64) {
        let record = [&head[..], &unix_secs.to_be_bytes()].concat();
        if let Err(error) = self.db.meta.insert(keys::META_FAILED_BUILD, record) {
            tracing::debug!(%error, "could not record a failed git-history build");
        }
    }

    /// When a build or append for `head` last failed, in unix seconds, or `None`.
    pub(crate) fn failed_build_at(&self, head: &[u8; 20]) -> Option<u64> {
        let record = self.db.meta.get(keys::META_FAILED_BUILD).ok().flatten()?;
        let (sha, secs) = record.split_at_checked(20)?;
        (sha == head).then(|| <[u8; 8]>::try_from(secs).ok().map(u64::from_be_bytes))?
    }

    /// A batched writer over the underlying Fjall database.
    pub fn writer(&self) -> Result<GitHistoryWriter, GitHistoryError> {
        let db = &self.db;
        Ok(GitHistoryWriter {
            db: db.clone(),
            batch: db.db.batch(),
            staged: 0,
            ordered: false,
        })
    }

    /// Last HEAD the index was synced to (20 raw sha bytes), or `None` if never built. The raw form
    /// is a builder-side detail; readers use [`last_indexed_head_hex`](Self::last_indexed_head_hex).
    pub fn last_indexed_head(&self) -> Option<[u8; 20]> {
        self.db.indexed_head()
    }

    /// Whether a build or append started and never stamped its head.
    pub fn build_interrupted(&self) -> bool {
        self.db.build_in_progress()
    }

    /// Last indexed HEAD as a 40-char hex string — the freshness key the MCP tools compare HEAD to.
    pub fn last_indexed_head_hex(&self) -> Option<String> {
        self.db.indexed_head().map(|s| keys::sha_raw_to_hex(&s))
    }

    /// Whether `head` (a 40-char hex sha) is an indexed tip the index answers for. False while a build
    /// or append is in progress.
    pub fn has_tip(&self, head: &str) -> bool {
        keys::sha_hex_to_raw(head).is_some_and(|head20| self.db.reach_of(&head20).is_some())
    }

    /// Whether the indexed tip `head` returns its commits in the order the live walk does: the one
    /// condition under which a read may answer from the index (see [`HistoryAt`](reader::HistoryAt)).
    /// False for a head that is no tip.
    pub fn tip_is_ordered(&self, head: &[u8; 20]) -> bool {
        self.db.tip_is_ordered(head)
    }

    /// The indexed tips, most recently synced first, as 40-char hex shas.
    pub fn tips(&self) -> Vec<String> {
        let mut tips = self.db.tips();
        tips.sort_by_key(|&(head, stamp)| std::cmp::Reverse((stamp, head)));
        tips.into_iter().map(|(head, _)| keys::sha_raw_to_hex(&head)).collect()
    }

    /// The reachability of the indexed tip `head`, as the builder extends it for a descendant.
    pub(crate) fn reach_of(&self, head: &[u8; 20]) -> Option<reach::Reach> {
        self.db.reach_of(head)
    }

    /// Marks the indexed tip `head` as the one just synced: it becomes the latest head and its sync
    /// time is renewed, so a sync that finds it already indexed keeps it out of the eviction order
    /// and reads at no head see it. Walks no commit. A `head` that is not an indexed tip is left
    /// alone.
    pub(crate) fn touch_tip(&self, head: &[u8; 20]) -> Result<(), GitHistoryError> {
        let meta = &self.db.meta;
        let Some(row) = meta.get(keys::tip_key(head))? else {
            return Ok(());
        };
        let Some((_, bits)) = row.split_at_checked(TIP_STAMP_BYTES) else {
            return Ok(());
        };
        let mut batch = self.db.db.batch();
        batch.insert(
            meta,
            keys::tip_key(head),
            [builder::now_unix().to_be_bytes().as_slice(), bits].concat(),
        );
        batch.insert(meta, keys::META_LAST_HEAD, head.to_vec());
        batch.commit()?;
        Ok(())
    }

    /// Next free commit ordinal (also the count of indexed commits).
    pub fn next_ord(&self) -> u32 {
        self.db.meta_u32(keys::META_NEXT_ORD)
    }

    /// Next free path id.
    pub fn next_path_id(&self) -> u32 {
        self.db.meta_u32(keys::META_NEXT_PATH_ID)
    }

    /// The clone's `shallow` file contents at build time; empty when it was not shallow.
    pub fn shallow_boundary(&self) -> Vec<u8> {
        self.db
            .meta
            .get(keys::META_SHALLOW)
            .ok()
            .flatten()
            .map_or_else(Vec::new, |bytes| bytes.to_vec())
    }

    /// Fingerprint: sha of the oldest reachable (root) commit at build time.
    pub fn root_sha(&self) -> Option<[u8; 20]> {
        self.db.meta_sha(keys::META_ROOT_SHA)
    }

    /// Fingerprint: number of commits indexed.
    pub fn commit_count(&self) -> u32 {
        self.db.meta_u32(keys::META_COMMIT_COUNT)
    }

    /// True when nothing has been indexed yet, or a build or append was cut off.
    pub fn is_empty(&self) -> bool {
        self.last_indexed_head_hex().is_none()
    }

    /// Builder-side point reads. A `None` here means "not indexed".
    pub(crate) fn ord_for_sha(&self, sha20: &[u8; 20]) -> Option<u32> {
        self.db.ord_for_sha(sha20)
    }

    pub(crate) fn path_id(&self, rel: &RelPath) -> Option<u32> {
        self.db.path_id(rel)
    }

    pub(crate) fn posting_bytes(&self, path_id: u32) -> Option<fjall::Slice> {
        self.db.posting_bytes(path_id)
    }

    pub(crate) fn term_posting_bytes(&self, term_key: &[u8]) -> Option<fjall::Slice> {
        self.db.term_posting_bytes(term_key)
    }
}

impl LocalDb {
    /// Drop all data and reset to an empty index at the current schema. `gh_meta` goes first, and the
    /// journal is synced before the rest: fjall writes a cleared keyspace's empty version durably at
    /// once but journals the clear itself buffered, and recovery replays a keyspace's journaled writes
    /// over an empty version. Unsynced, a power loss could bring the head back over postings whose
    /// clear did reach the disk. Each keyspace is one journaled clear, not a delete per key, so the
    /// cost does not grow with the index.
    fn clear(&self) -> Result<(), GitHistoryError> {
        self.meta.clear()?;
        self.db.persist(fjall::PersistMode::SyncAll)?;
        for ks in [
            &self.commit_by_ord,
            &self.ord_by_sha,
            &self.path_id_by_path,
            &self.path_by_id,
            &self.path_to_ords,
            &self.commit_text_by_ord,
            &self.term_to_ords,
            &self.term_dict,
        ] {
            ks.clear()?;
        }
        self.meta
            .insert(keys::META_SCHEMA_VER, GIT_HISTORY_SCHEMA.to_be_bytes())?;
        Ok(())
    }

    fn meta_u32(&self, key: &[u8]) -> u32 {
        self.meta
            .get(key)
            .ok()
            .flatten()
            .and_then(|b| keys::parse_u32(&b))
            .unwrap_or(0)
    }

    /// The stamped head, or `None` while a build or append is in progress or was cut off.
    fn indexed_head(&self) -> Option<[u8; 20]> {
        if self.build_in_progress() {
            return None;
        }
        self.meta_sha(keys::META_LAST_HEAD)
    }

    /// The reachability of `head`, or `None` while a build or append is in progress or was cut off
    /// and when `head` is not an indexed tip.
    fn reach_of(&self, head: &[u8; 20]) -> Option<reach::Reach> {
        if self.build_in_progress() {
            return None;
        }
        let row = self.meta.get(keys::tip_key(head)).ok().flatten()?;
        let (_, bits) = row.split_at_checked(TIP_STAMP_BYTES)?;
        Some(reach::Reach::from_bytes(bits))
    }

    fn tip_is_ordered(&self, head: &[u8; 20]) -> bool {
        self.meta.contains_key(keys::ordered_key(head)).unwrap_or(false)
    }

    /// Every stored tip with the unix second it was last synced.
    fn tips(&self) -> Vec<([u8; 20], u64)> {
        self.meta
            .prefix(keys::META_TIP_PREFIX)
            .filter_map(|guard| {
                let (key, value) = guard.into_inner().ok()?;
                let stamp = value.get(..TIP_STAMP_BYTES)?;
                Some((keys::parse_tip_key(&key)?, u64::from_be_bytes(stamp.try_into().ok()?)))
            })
            .collect()
    }

    /// A read error counts as in progress, so the index fails closed.
    fn build_in_progress(&self) -> bool {
        self.meta.contains_key(keys::META_BUILD_IN_PROGRESS).unwrap_or(true)
    }

    fn meta_sha(&self, key: &[u8]) -> Option<[u8; 20]> {
        let bytes = self.meta.get(key).ok().flatten()?;
        <[u8; 20]>::try_from(bytes.as_ref()).ok()
    }

    /// Point-read one commit's stored metadata. `want_files=false` decodes only the head fields
    /// (sha / time / author / summary), skipping the changed-file list and its allocation — the
    /// reader passes it for `include_files=false` tools like `commits_touching`.
    pub(crate) fn commit_meta(&self, ord: u32, want_files: bool) -> Option<CommitMeta> {
        self.stored_commit(ord, want_files).ok()
    }

    /// [`LocalDb::commit_meta`] for an ordinal the index's own keys name, where a missing or
    /// undecodable row is an error rather than an absent commit.
    pub(crate) fn stored_commit(&self, ord: u32, want_files: bool) -> Result<CommitMeta, GitHistoryError> {
        let bytes = self
            .commit_by_ord
            .get(keys::u32_key(ord))?
            .ok_or_else(|| GitHistoryError::Corrupt(format!("commit ordinal {ord} has no row")))?;
        decode_commit_value(&bytes, want_files)
            .ok_or_else(|| GitHistoryError::Corrupt(format!("commit ordinal {ord} does not decode")))
    }

    pub(crate) fn ord_for_sha(&self, sha20: &[u8; 20]) -> Option<u32> {
        let bytes = self.ord_by_sha.get(sha20).ok().flatten()?;
        keys::parse_u32(&bytes)
    }

    pub(crate) fn path_id(&self, rel: &RelPath) -> Option<u32> {
        self.stored_path_id(rel).ok().flatten()
    }

    /// The id `rel` was interned under, `Ok(None)` when no indexed commit touches it, and an error
    /// when the lookup failed.
    pub(crate) fn stored_path_id(&self, rel: &RelPath) -> Result<Option<u32>, GitHistoryError> {
        let Some(key) = keys::path_id_by_path_key(rel) else {
            return Ok(None);
        };
        let Some(bytes) = self.path_id_by_path.get(&key)? else {
            return Ok(None);
        };
        keys::parse_u32(&bytes)
            .map(Some)
            .ok_or_else(|| GitHistoryError::Corrupt(format!("the path id of `{rel}` does not decode")))
    }

    pub(crate) fn path_for_id(&self, path_id: u32) -> Option<RelPath> {
        let bytes = self.path_by_id.get(keys::u32_key(path_id)).ok().flatten()?;
        Some(RelPath::from(bytes.as_ref()))
    }

    pub(crate) fn posting_bytes(&self, path_id: u32) -> Option<fjall::Slice> {
        self.stored_posting(path_id).ok().flatten()
    }

    /// The posting list of `path_id`, `Ok(None)` when it has none, and an error when the lookup
    /// failed.
    pub(crate) fn stored_posting(&self, path_id: u32) -> Result<Option<fjall::Slice>, GitHistoryError> {
        Ok(self.path_to_ords.get(keys::u32_key(path_id))?)
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
    db: LocalDb,
    batch: OwnedWriteBatch,
    staged: usize,
    ordered: bool,
}

/// Flush the Fjall batch every N staged ops (mirrors the scanner's `INDEX_COMMIT_BATCH`).
const COMMIT_BATCH: usize = 4096;

impl GitHistoryWriter {
    /// Mark the index in progress before the first row is staged. Written straight to the journal,
    /// so it precedes every batch this writer commits: no surviving row can outlive the mark.
    ///
    /// The head goes in the same batch: an older binary of the same minor shares this index and
    /// knows no mark, and would otherwise append from the old head past the rows a cut-off run left.
    pub fn begin(&mut self) -> Result<(), GitHistoryError> {
        let mut mark = self.db.db.batch();
        mark.insert(&self.db.meta, keys::META_BUILD_IN_PROGRESS, [1u8]);
        mark.remove(&self.db.meta, keys::META_LAST_HEAD);
        mark.commit()?;
        Ok(())
    }

    /// Whether the tip [`finish_meta`](Self::finish_meta) stamps returns its commits in the live
    /// walk's order. Unset, the tip is read by the live walk.
    pub fn set_ordered(&mut self, ordered: bool) {
        self.ordered = ordered;
    }

    /// Record the `shallow` file this build read, so a later fetch that moves a boundary is seen.
    pub fn put_shallow_boundary(&mut self, boundary: &[u8]) -> Result<(), GitHistoryError> {
        if boundary.is_empty() {
            self.db.meta.remove(keys::META_SHALLOW)?;
        } else {
            self.db.meta.insert(keys::META_SHALLOW, boundary)?;
        }
        Ok(())
    }

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
        self.batch.insert(&self.db.commit_by_ord, keys::u32_key(ord), value);
        self.maybe_flush()
    }

    pub fn put_ord_for_sha(&mut self, sha20: &[u8; 20], ord: u32) -> Result<(), GitHistoryError> {
        self.batch.insert(&self.db.ord_by_sha, *sha20, keys::u32_key(ord));
        self.maybe_flush()
    }

    pub fn put_path(&mut self, rel: &RelPath, path_id: u32) -> Result<(), GitHistoryError> {
        if let Some(key) = keys::path_id_by_path_key(rel) {
            self.batch.insert(&self.db.path_id_by_path, key, keys::u32_key(path_id));
        }
        self.batch
            .insert(&self.db.path_by_id, keys::u32_key(path_id), rel.as_bytes().to_vec());
        self.maybe_flush()
    }

    pub fn put_posting(&mut self, path_id: u32, encoded: &[u8]) -> Result<(), GitHistoryError> {
        self.batch
            .insert(&self.db.path_to_ords, keys::u32_key(path_id), encoded.to_vec());
        self.maybe_flush()
    }

    /// Store one commit's full message body (skipped by the caller when empty).
    pub fn put_commit_text(&mut self, ord: u32, body: &[u8]) -> Result<(), GitHistoryError> {
        self.batch
            .insert(&self.db.commit_text_by_ord, keys::u32_key(ord), body.to_vec());
        self.maybe_flush()
    }

    /// Store the newest-first posting list for one `(field, term)` search key.
    pub fn put_term_posting(&mut self, term_key: &[u8], encoded: &[u8]) -> Result<(), GitHistoryError> {
        self.batch
            .insert(&self.db.term_to_ords, term_key.to_vec(), encoded.to_vec());
        self.batch.insert(&self.db.term_dict, term_key.to_vec(), Vec::new());
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
        let batch = std::mem::replace(&mut self.batch, self.db.db.batch());
        batch.commit()?;
        self.staged = 0;
        Ok(())
    }

    /// Write the meta fingerprint + counters and the tip `head` with the commits it `reach`es, commit
    /// everything and sync the journal. Call LAST: the tip becoming visible is the commit point — a crash before this leaves the
    /// index looking unbuilt, so the next revalidate triggers an idempotent full rebuild. Compacts
    /// nothing unless the write buffer or a small keyspace's tables have grown; a full rebuild
    /// compacts afterwards.
    pub fn finish_meta(
        mut self,
        head: &[u8; 20],
        reach: &reach::Reach,
        root: &[u8; 20],
        next_ord: u32,
        next_path_id: u32,
        commit_count: u32,
    ) -> Result<(), GitHistoryError> {
        self.flush()?;
        let mut meta = self.db.db.batch();
        meta.insert(&self.db.meta, keys::META_NEXT_ORD, next_ord.to_be_bytes());
        meta.insert(&self.db.meta, keys::META_NEXT_PATH_ID, next_path_id.to_be_bytes());
        meta.insert(&self.db.meta, keys::META_COMMIT_COUNT, commit_count.to_be_bytes());
        meta.insert(&self.db.meta, keys::META_ROOT_SHA, root.to_vec());
        meta.insert(&self.db.meta, keys::META_LAST_HEAD, head.to_vec());
        let stamp = builder::now_unix().to_be_bytes();
        meta.insert(
            &self.db.meta,
            keys::tip_key(head),
            [stamp.as_slice(), reach.as_bytes()].concat(),
        );
        if self.ordered {
            meta.insert(&self.db.meta, keys::ordered_key(head), [1u8]);
        } else {
            meta.remove(&self.db.meta, keys::ordered_key(head));
        }
        for stale in evicted_tips(self.db.tips(), head, MAX_TIPS) {
            meta.remove(&self.db.meta, keys::tip_key(&stale));
            meta.remove(&self.db.meta, keys::ordered_key(&stale));
        }
        meta.remove(&self.db.meta, keys::META_BUILD_IN_PROGRESS);
        meta.remove(&self.db.meta, keys::META_FAILED_BUILD);
        meta.commit()?;
        self.db.settle_append(maintenance::APPEND_WRITE_BUFFER_BYTES);
        Ok(())
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
