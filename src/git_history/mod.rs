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
//!   CLI query): that process opens the DB locally ([`Backend::Local`]) and, if it is a writer,
//!   builds it in-process.
//! * **Daemon** (a `comms` build, where the daemon is the machine's sole fjall writer): the DAEMON
//!   holds the DB and builds it, and EVERY front-end — a `daemon_writer` serve, and equally the
//!   one-shot CLI — holds a [`Backend::Remote`] proxy that forwards each history query over the
//!   socket. A front-end must not try to open the DB: it cannot win a lock the daemon holds, so it
//!   would only stall on the retry ladder and then live-walk forever. And building it in a front-end
//!   would be doubly wrong — it would steal the lock the daemon (and every peer session) needs, and
//!   it would run a multi-GB, minutes-long walk inside the process an agent is actively querying.
//!
//! A process that can reach neither keeps `git_history: None` and falls back to the live walk. The
//! index is a pure accelerator — tools use it only when `last_indexed_head == HEAD` and otherwise
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
pub mod proto;
pub mod reader;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod remote;

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
pub const GIT_HISTORY_SCHEMA: u32 = crate::version::RELEASE_MINOR as u32 + 7;

/// LZ4 on data and index blocks at every level; see [`crate::index`]'s `index_keyspace_options`.
fn keyspace_options() -> KeyspaceCreateOptions {
    KeyspaceCreateOptions::default()
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
const DISCOVER_PARENTS_ENV: &str = "BASEMIND_GIT_DISCOVER_PARENTS";

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
    repo.origin_is_workdir() || std::env::var_os(DISCOVER_PARENTS_ENV).is_some_and(|v| v != "0")
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
    /// A write (or a builder read) was attempted against a DAEMON-BACKED handle. The daemon is the
    /// sole holder of the git-history database in that deployment; front-ends forward their sync
    /// request to it instead of building locally. Never reached from a standalone process.
    #[error("git-history index is daemon-backed; this process cannot write it")]
    NotLocal,
    /// The git-history index is disabled by configuration (`BASEMIND_GH_INDEX=0`). Returned by the
    /// daemon's in-process runner so both the forwarded and the hosted path report it uniformly.
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
        && let Err(error) = crate::store_base::sync_directory(parent)
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

/// Where a [`GitHistoryIndex`]'s data actually lives. See the module docs for which process holds
/// the (single-holder) fjall database in each deployment.
#[derive(Clone)]
enum Backend {
    /// This process holds the fjall database.
    Local(LocalDb),
    /// Another process holds it; every read is forwarded to it.
    Remote(std::sync::Arc<dyn HistoryForwarder>),
}

/// The read side of a daemon-backed index: forwards each query to the process that holds the
/// database. Implemented outside the core by the transport (`remote::RemoteHistory`), so this module
/// names no transport, runtime or caller identity; the implementor carries its own. Synchronous: an
/// implementor that waits on IO blocks inside the call. `Any` lets the transport recover its own type
/// from a handle.
pub trait HistoryForwarder: Send + Sync + std::any::Any {
    /// The HEAD the holder's index is synced to, or `None` when unbuilt or unreachable. `None` makes
    /// the caller's freshness check fail closed: it live-walks.
    fn indexed_head(&self) -> Option<String>;

    /// Run one commit-returning op against the holder's index. Unreachable or a reply of another
    /// shape yields an empty page.
    fn commits(&self, op: proto::GitHistoryOp) -> Vec<crate::git::CommitInfo>;

    /// Whether the holder is this process. Such a handle must never be asked to sync over the
    /// holder's socket, which would dial itself; required so no implementor can leave it out.
    fn hosts_in_process(&self) -> bool;
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
}

/// The git-history index as its callers see it: the same read API whether the data sits in a fjall
/// database this process holds or behind the daemon that holds it.
#[derive(Clone)]
pub struct GitHistoryIndex {
    backend: Backend,
}

impl GitHistoryIndex {
    /// Open (or create) `.basemind/git-history.fjall/` LOCALLY, taking fjall's exclusive lock. Only
    /// the process that owns the index may call this: a standalone `scan` / `rescan` / writable
    /// `serve`, or the daemon. An OLDER on-disk schema wipes the directory and recreates it empty (the
    /// caller rebuilds via the builder); a NEWER one is left untouched and fails with
    /// [`GitHistoryError::NewerSchema`]. Returns `Err` if another process holds the Fjall lock —
    /// read-only callers swallow that to `None`.
    pub fn open(basemind_dir: &Path) -> Result<Self, GitHistoryError> {
        sweep_discarded(basemind_dir);
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

    /// Whether the index under `basemind_dir` exists. Only a build creates it: a reader that finds
    /// none answers as an unbuilt index (history tools live-walk) instead of creating an empty one.
    pub fn exists(basemind_dir: &Path) -> bool {
        basemind_dir.join(GIT_HISTORY_DIR).is_dir()
    }

    /// A handle backed by an index another process holds (the DAEMON's), reached through
    /// `forwarder`. Takes no lock and performs no IO: this is what every front-end running under a
    /// live daemon holds — a `daemon_writer` serve, and the one-shot CLI — instead of opening the
    /// database the daemon has locked.
    pub fn forwarded(forwarder: std::sync::Arc<dyn HistoryForwarder>) -> Self {
        Self {
            backend: Backend::Remote(forwarder),
        }
    }

    /// True when this handle forwards to the daemon rather than holding the database. The write side
    /// keys off it: the builder can only run in the process that owns the lock, so a daemon-backed
    /// session must ASK the daemon to build instead of calling the builder itself.
    pub fn is_daemon_backed(&self) -> bool {
        matches!(self.backend, Backend::Remote(_))
    }

    /// The forwarder of a daemon-backed handle, `None` for a local one.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) fn forwarder(&self) -> Option<&std::sync::Arc<dyn HistoryForwarder>> {
        match &self.backend {
            Backend::Remote(forwarder) => Some(forwarder),
            Backend::Local(_) => None,
        }
    }

    /// The local database, or `None` for a daemon-backed handle. Every write path (the builder) and
    /// every low-level point read goes through this.
    pub(crate) fn local(&self) -> Option<&LocalDb> {
        match &self.backend {
            Backend::Local(db) => Some(db),
            Backend::Remote(_) => None,
        }
    }

    /// The local database, or [`GitHistoryError::NotLocal`] — the writer-side accessor.
    pub(crate) fn require_local(&self) -> Result<&LocalDb, GitHistoryError> {
        self.local().ok_or(GitHistoryError::NotLocal)
    }

    fn open_at(dir: &Path) -> Result<Self, GitHistoryError> {
        std::fs::create_dir_all(dir).map_err(|source| GitHistoryError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let mut db = Database::builder(dir).worker_threads(1).open()?;
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
            db = Database::builder(dir).worker_threads(1).open()?;
            meta = db.keyspace("gh_meta", keyspace_options)?;
        }
        let commit_by_ord = db.keyspace("gh_commit_by_ord", keyspace_options)?;
        let ord_by_sha = db.keyspace("gh_ord_by_sha", keyspace_options)?;
        let path_id_by_path = db.keyspace("gh_path_id_by_path", keyspace_options)?;
        let path_by_id = db.keyspace("gh_path_by_id", keyspace_options)?;
        let path_to_ords = db.keyspace("gh_path_to_ords", keyspace_options)?;
        let commit_text_by_ord = db.keyspace("gh_commit_text_by_ord", keyspace_options)?;
        let term_to_ords = db.keyspace("gh_term_to_ords", keyspace_options)?;
        meta.insert(keys::META_SCHEMA_VER, GIT_HISTORY_SCHEMA.to_be_bytes())?;
        Ok(Self {
            backend: Backend::Local(LocalDb {
                db,
                meta,
                commit_by_ord,
                ord_by_sha,
                path_id_by_path,
                path_by_id,
                path_to_ords,
                commit_text_by_ord,
                term_to_ords,
            }),
        })
    }

    /// Drop all data and reset to an empty index at the current schema. Used by `revalidate` when a
    /// history rewrite is detected and by the manual force-rebuild path. Safe because only the
    /// `.basemind/.lock` holder ever opens this DB, so no other process holds a handle.
    pub fn clear(&self) -> Result<(), GitHistoryError> {
        self.require_local()?.clear()
    }

    /// Record that a build or append for `head` failed at `unix_secs`. Best-effort: a record that
    /// cannot be written costs one more rebuild.
    pub(crate) fn record_failed_build(&self, head: &[u8; 20], unix_secs: u64) {
        if let Some(db) = self.local() {
            let record = [&head[..], &unix_secs.to_be_bytes()].concat();
            if let Err(error) = db.meta.insert(keys::META_FAILED_BUILD, record) {
                tracing::debug!(%error, "could not record a failed git-history build");
            }
        }
    }

    /// When a build or append for `head` last failed, in unix seconds, or `None`.
    pub(crate) fn failed_build_at(&self, head: &[u8; 20]) -> Option<u64> {
        let record = self.local()?.meta.get(keys::META_FAILED_BUILD).ok().flatten()?;
        let (sha, secs) = record.split_at_checked(20)?;
        (sha == head).then(|| <[u8; 8]>::try_from(secs).ok().map(u64::from_be_bytes))?
    }

    /// A batched writer over the underlying Fjall database. Only a local index has one.
    pub fn writer(&self) -> Result<GitHistoryWriter, GitHistoryError> {
        let db = self.require_local()?;
        Ok(GitHistoryWriter {
            db: db.clone(),
            batch: db.db.batch(),
            staged: 0,
        })
    }

    /// Last HEAD the index was synced to (20 raw sha bytes), or `None` if never built. Local only —
    /// the raw form is a builder-side detail; the forwarded path uses
    /// [`last_indexed_head_hex`](Self::last_indexed_head_hex).
    pub fn last_indexed_head(&self) -> Option<[u8; 20]> {
        self.local()?.indexed_head()
    }

    /// Whether a build or append started and never stamped its head. Local only.
    pub fn build_interrupted(&self) -> bool {
        self.local().is_some_and(LocalDb::build_in_progress)
    }

    /// Last indexed HEAD as a 40-char hex string — the freshness key the MCP tools compare HEAD to.
    /// On a daemon-backed handle this is one forwarded round trip; an unreachable daemon answers
    /// `None`, which fails the freshness check closed (the caller live-walks).
    pub fn last_indexed_head_hex(&self) -> Option<String> {
        match &self.backend {
            Backend::Local(db) => db.indexed_head().map(|s| keys::sha_raw_to_hex(&s)),
            Backend::Remote(remote) => remote.indexed_head(),
        }
    }

    /// Next free commit ordinal (also the count of indexed commits).
    pub fn next_ord(&self) -> u32 {
        self.local().map(|db| db.meta_u32(keys::META_NEXT_ORD)).unwrap_or(0)
    }

    /// Next free path id.
    pub fn next_path_id(&self) -> u32 {
        self.local().map(|db| db.meta_u32(keys::META_NEXT_PATH_ID)).unwrap_or(0)
    }

    /// Fingerprint: sha of the oldest reachable (root) commit at build time.
    pub fn root_sha(&self) -> Option<[u8; 20]> {
        self.local()?.meta_sha(keys::META_ROOT_SHA)
    }

    /// Fingerprint: number of commits indexed.
    pub fn commit_count(&self) -> u32 {
        self.local().map(|db| db.meta_u32(keys::META_COMMIT_COUNT)).unwrap_or(0)
    }

    /// True when nothing has been indexed yet, or a build or append was cut off.
    pub fn is_empty(&self) -> bool {
        self.last_indexed_head_hex().is_none()
    }

    /// Builder-side point reads. Local only: the builder runs in the process that holds the
    /// database (a daemon-backed handle is rejected up front by [`builder::sync`]), so a `None` here
    /// means "not indexed", never "ask the daemon".
    pub(crate) fn ord_for_sha(&self, sha20: &[u8; 20]) -> Option<u32> {
        self.local()?.ord_for_sha(sha20)
    }

    pub(crate) fn path_id(&self, rel: &RelPath) -> Option<u32> {
        self.local()?.path_id(rel)
    }

    pub(crate) fn posting_bytes(&self, path_id: u32) -> Option<fjall::Slice> {
        self.local()?.posting_bytes(path_id)
    }

    pub(crate) fn term_posting_bytes(&self, term_key: &[u8]) -> Option<fjall::Slice> {
        self.local()?.term_posting_bytes(term_key)
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
        ] {
            ks.clear()?;
        }
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
    db: LocalDb,
    batch: OwnedWriteBatch,
    staged: usize,
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
        let mut meta = self.db.db.batch();
        meta.insert(&self.db.meta, keys::META_NEXT_ORD, next_ord.to_be_bytes());
        meta.insert(&self.db.meta, keys::META_NEXT_PATH_ID, next_path_id.to_be_bytes());
        meta.insert(&self.db.meta, keys::META_COMMIT_COUNT, commit_count.to_be_bytes());
        meta.insert(&self.db.meta, keys::META_ROOT_SHA, root.to_vec());
        meta.insert(&self.db.meta, keys::META_LAST_HEAD, head.to_vec());
        meta.remove(&self.db.meta, keys::META_BUILD_IN_PROGRESS);
        meta.remove(&self.db.meta, keys::META_FAILED_BUILD);
        meta.commit()?;
        self.db.compact()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp_schema_ver(basemind_dir: &Path, ver: u32) {
        let index = GitHistoryIndex::open(basemind_dir).expect("open index");
        let db = index.local().expect("local handle");
        db.meta.insert(keys::META_ROOT_SHA, vec![7u8; 20]).expect("write a row");
        db.meta
            .insert(keys::META_SCHEMA_VER, ver.to_be_bytes())
            .expect("stamp schema version");
    }

    fn raw_meta(basemind_dir: &Path, key: &[u8]) -> Option<Vec<u8>> {
        let db = Database::builder(basemind_dir.join(GIT_HISTORY_DIR))
            .worker_threads(1)
            .open()
            .expect("open raw index");
        let meta = db.keyspace("gh_meta", keyspace_options).expect("meta keyspace");
        meta.get(key).expect("read meta").map(|value| value.to_vec())
    }

    #[test]
    fn open_refuses_an_index_stamped_by_a_newer_basemind_and_leaves_it_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let newer = GIT_HISTORY_SCHEMA + 1;
        stamp_schema_ver(dir.path(), newer);

        let Err(error) = GitHistoryIndex::open(dir.path()) else {
            panic!("a newer git-history index must not open");
        };

        assert!(
            matches!(
                error,
                GitHistoryError::NewerSchema { stored, current } if stored == newer && current == GIT_HISTORY_SCHEMA
            ),
            "expected NewerSchema, got: {error}"
        );
        assert!(
            error.to_string().contains("upgrade basemind"),
            "the error must be actionable, got: {error}"
        );
        assert_eq!(
            raw_meta(dir.path(), keys::META_SCHEMA_VER),
            Some(newer.to_be_bytes().to_vec()),
            "the newer index keeps its stamp: it was neither wiped nor restamped"
        );
        assert_eq!(
            raw_meta(dir.path(), keys::META_ROOT_SHA),
            Some(vec![7u8; 20]),
            "the newer index keeps its rows"
        );
    }

    #[test]
    fn open_sweeps_a_directory_an_interrupted_wipe_set_aside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let aside = dir.path().join(format!("{DISCARD_PREFIX}1-0"));
        std::fs::create_dir_all(aside.join("journals")).expect("a half-deleted leftover");

        drop(GitHistoryIndex::open(dir.path()).expect("open"));

        assert!(!aside.exists(), "the leftover is deleted on the next open");
    }

    #[test]
    fn clear_drops_the_head_with_every_row_and_keeps_the_stamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = GitHistoryIndex::open(dir.path()).expect("open");
        let sha = "11".repeat(20);
        let sha20 = keys::sha_hex_to_raw(&sha).expect("a valid sha");
        let mut writer = index.writer().expect("writer");
        writer.begin().expect("mark the build");
        let meta = CommitMeta {
            sha,
            summary: "c1".to_owned(),
            author: "t".to_owned(),
            author_email: "t@e.x".to_owned(),
            author_time_unix: 0,
            files: Vec::new(),
        };
        writer.put_commit_meta(0, &meta).expect("commit row");
        writer.put_ord_for_sha(&sha20, 0).expect("sha row");
        writer.finish_meta(&sha20, &sha20, 1, 0, 1).expect("stamp the head");
        assert_eq!(index.last_indexed_head(), Some(sha20));

        index.clear().expect("clear");

        assert_eq!(index.last_indexed_head(), None);
        assert!(!index.build_interrupted());
        assert_eq!(index.ord_for_sha(&sha20), None);
        assert_eq!(index.commit_count(), 0);
        drop(index);
        assert_eq!(
            raw_meta(dir.path(), keys::META_SCHEMA_VER),
            Some(GIT_HISTORY_SCHEMA.to_be_bytes().to_vec())
        );
    }

    /// An older patch binary of the same minor shares this index and knows no in-progress mark:
    /// had the head survived a cut-off append, it would append past the rows the cut left.
    #[test]
    fn a_build_in_progress_leaves_no_head_on_disk_for_a_binary_that_ignores_the_mark() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = GitHistoryIndex::open(dir.path()).expect("open");
        let sha20 = [0x22u8; 20];
        index
            .writer()
            .expect("writer")
            .finish_meta(&sha20, &sha20, 0, 0, 0)
            .expect("stamp a head");
        index.writer().expect("writer").begin().expect("mark an append");
        drop(index);

        assert_eq!(raw_meta(dir.path(), keys::META_BUILD_IN_PROGRESS), Some(vec![1u8]));
        assert_eq!(
            raw_meta(dir.path(), keys::META_LAST_HEAD),
            None,
            "no head to append from"
        );
    }

    #[test]
    fn open_wipes_an_index_stamped_by_an_older_basemind() {
        let dir = tempfile::tempdir().expect("tempdir");
        stamp_schema_ver(dir.path(), GIT_HISTORY_SCHEMA - 1);

        drop(GitHistoryIndex::open(dir.path()).expect("an older index is wiped and reopened"));

        assert_eq!(
            raw_meta(dir.path(), keys::META_SCHEMA_VER),
            Some(GIT_HISTORY_SCHEMA.to_be_bytes().to_vec()),
            "the rebuilt index carries the current stamp"
        );
        assert_eq!(
            raw_meta(dir.path(), keys::META_ROOT_SHA),
            None,
            "rows written under the older schema are gone"
        );
        let leftovers = std::fs::read_dir(dir.path())
            .expect("read the workspace dir")
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(DISCARD_PREFIX))
            .count();
        assert_eq!(leftovers, 0, "the wiped database was set aside and deleted");
    }
}
