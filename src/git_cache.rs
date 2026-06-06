//! RAM LRU + disk cache for sha-keyed git artifacts.
//!
//! What lives in here is everything that is expensive to compute and either
//! (a) immutable forever (anything keyed by a commit sha — `commit_files`,
//! per-blame results, etc.) or (b) immutable per HEAD position (the
//! `log` walks, which we key by the resolved HEAD sha at the time of the
//! request so a stale entry still describes a valid past).
//!
//! Two layers:
//! - **RAM** — `lru::LruCache` per category, behind a `Mutex`. Bounded by
//!   capacity from `ServeArgs`.
//! - **Disk** — sha-keyed `.msgpack` files under `.gitmind/git-cache/`. Optional;
//!   `GitCache::open(.., persist=false)` skips disk altogether for ephemeral
//!   `gitmind cache` operations.
//!
//! Both layers are content-addressed by the inputs the agent passed; we never
//! invalidate, only roll off via LRU. The schema version is baked into every
//! payload and a mismatch on read wipes the on-disk dir.

use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lru::LruCache;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::git::{BlameResult, ChangeKind, CommitInfo, GitError, Repo};

/// Bump when any cached payload's shape changes. Mismatch on read wipes the on-disk dir.
pub const GIT_CACHE_SCHEMA: u16 = 1;
pub const GIT_CACHE_DIR: &str = "git-cache";

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("git error: {0}")]
    Git(#[from] GitError),
}

// ─── disk payload wrappers ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommitFilesPayload {
    schema_ver: u16,
    files: Vec<(String, ChangeKind)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LogPayload {
    schema_ver: u16,
    commits: Vec<CommitInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BlamePayload {
    schema_ver: u16,
    result: BlameResult,
}

// ─── cache keys ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlameKey {
    pub suspect_sha: String,
    pub path: String,
    pub range: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogKey {
    /// HEAD sha at the time the entry was computed. Two requests with the same head_sha
    /// always return the same walk; once HEAD moves, the new sha defines a new key.
    pub head_sha: String,
    /// Optional path filter (Some for `commits_touching`, None for `recent_changes`).
    pub path: Option<String>,
    pub limit: u32,
    pub include_files: bool,
}

// ─── main cache ──────────────────────────────────────────────────────────────

pub struct GitCache {
    commit_files: Mutex<LruCache<String /* sha40 */, Arc<Vec<(String, ChangeKind)>>>>,
    log: Mutex<LruCache<LogKey, Arc<Vec<CommitInfo>>>>,
    blame: Mutex<LruCache<BlameKey, Arc<BlameResult>>>,
    disk: Option<PathBuf>,
}

impl GitCache {
    /// Open the cache. When `persist=true`, the disk dir is created under `gitmind_dir`;
    /// when `false`, only the RAM layer is used.
    pub fn open(
        gitmind_dir: &Path,
        mem_capacity: usize,
        persist: bool,
    ) -> Result<Self, CacheError> {
        let disk = if persist {
            let root = gitmind_dir.join(GIT_CACHE_DIR);
            ensure_subdir(&root, "commit_files")?;
            ensure_subdir(&root, "log")?;
            ensure_subdir(&root, "blame")?;
            Some(root)
        } else {
            None
        };
        let cap = NonZeroUsize::new(mem_capacity.max(1)).expect("capacity > 0");
        Ok(Self {
            commit_files: Mutex::new(LruCache::new(cap)),
            log: Mutex::new(LruCache::new(cap)),
            blame: Mutex::new(LruCache::new(cap)),
            disk,
        })
    }

    /// Look up (or compute) the per-file change list for a commit. Sha-keyed: result is
    /// immutable, so any hit is correct forever.
    pub fn commit_files(
        &self,
        repo: &Repo,
        commit_sha: &str,
    ) -> Result<Arc<Vec<(String, ChangeKind)>>, CacheError> {
        if let Some(hit) = self.commit_files.lock().unwrap().get(commit_sha).cloned() {
            return Ok(hit);
        }
        if let Some(disk) = self.read_commit_files_disk(commit_sha) {
            let arc = Arc::new(disk);
            self.commit_files
                .lock()
                .unwrap()
                .put(commit_sha.to_string(), Arc::clone(&arc));
            return Ok(arc);
        }
        let computed = repo.commit_files_uncached(commit_sha)?;
        let arc = Arc::new(computed);
        self.commit_files
            .lock()
            .unwrap()
            .put(commit_sha.to_string(), Arc::clone(&arc));
        self.write_commit_files_disk(commit_sha, &arc);
        Ok(arc)
    }

    /// Look up (or compute) a log slice. Keyed by (head_sha, path, limit, include_files);
    /// commits past `head_sha` are immutable, so the cached walk stays valid.
    pub fn log(
        &self,
        repo: &Repo,
        head_sha: &str,
        path: Option<&str>,
        limit: u32,
        include_files: bool,
    ) -> Result<Arc<Vec<CommitInfo>>, CacheError> {
        let key = LogKey {
            head_sha: head_sha.to_string(),
            path: path.map(str::to_string),
            limit,
            include_files,
        };
        if let Some(hit) = self.log.lock().unwrap().get(&key).cloned() {
            return Ok(hit);
        }
        if let Some(disk) = self.read_log_disk(&key) {
            let arc = Arc::new(disk);
            self.log.lock().unwrap().put(key.clone(), Arc::clone(&arc));
            return Ok(arc);
        }
        let commits = match path {
            Some(p) => repo.log_for_path(p, limit as usize)?,
            None => repo.log_paths(limit as usize, include_files)?,
        };
        let arc = Arc::new(commits);
        self.log.lock().unwrap().put(key.clone(), Arc::clone(&arc));
        self.write_log_disk(&key, &arc);
        Ok(arc)
    }

    /// Look up (or compute) a blame for `(suspect_sha, path, range)`. Sha-keyed: caches
    /// forever. Cost of cold compute scales with file history size.
    pub fn blame(
        &self,
        repo: &Repo,
        suspect_sha: &str,
        path: &str,
        range: Option<(u32, u32)>,
    ) -> Result<Arc<BlameResult>, CacheError> {
        let key = BlameKey {
            suspect_sha: suspect_sha.to_string(),
            path: path.to_string(),
            range,
        };
        if let Some(hit) = self.blame.lock().unwrap().get(&key).cloned() {
            return Ok(hit);
        }
        if let Some(disk) = self.read_blame_disk(&key) {
            let arc = Arc::new(disk);
            self.blame
                .lock()
                .unwrap()
                .put(key.clone(), Arc::clone(&arc));
            return Ok(arc);
        }
        let computed = repo.blame_file(suspect_sha, path, range)?;
        let arc = Arc::new(computed);
        self.blame
            .lock()
            .unwrap()
            .put(key.clone(), Arc::clone(&arc));
        self.write_blame_disk(&key, &arc);
        Ok(arc)
    }

    /// Drop the on-disk cache + reset RAM. Returns the number of disk files removed.
    pub fn clear(&self) -> Result<usize, CacheError> {
        let mut removed = 0usize;
        if let Some(root) = &self.disk
            && root.exists()
        {
            removed += count_files(root);
            fs::remove_dir_all(root).map_err(|source| CacheError::Io {
                path: root.clone(),
                source,
            })?;
            fs::create_dir_all(root).map_err(|source| CacheError::Io {
                path: root.clone(),
                source,
            })?;
        }
        self.commit_files.lock().unwrap().clear();
        self.log.lock().unwrap().clear();
        self.blame.lock().unwrap().clear();
        Ok(removed)
    }

    // ─── disk helpers ───────────────────────────────────────────────────

    fn read_commit_files_disk(&self, sha: &str) -> Option<Vec<(String, ChangeKind)>> {
        let path = self.commit_files_path(sha)?;
        if !path.exists() {
            return None;
        }
        let bytes = fs::read(&path).ok()?;
        let payload: CommitFilesPayload = rmp_serde::from_slice(&bytes).ok()?;
        if payload.schema_ver != GIT_CACHE_SCHEMA {
            return None;
        }
        Some(payload.files)
    }

    fn write_commit_files_disk(&self, sha: &str, files: &[(String, ChangeKind)]) {
        let Some(path) = self.commit_files_path(sha) else {
            return;
        };
        let payload = CommitFilesPayload {
            schema_ver: GIT_CACHE_SCHEMA,
            files: files.to_vec(),
        };
        let Ok(bytes) = rmp_serde::to_vec_named(&payload) else {
            return;
        };
        let _ = atomic_write(&path, &bytes);
    }

    fn read_log_disk(&self, key: &LogKey) -> Option<Vec<CommitInfo>> {
        let path = self.log_path(key)?;
        if !path.exists() {
            return None;
        }
        let bytes = fs::read(&path).ok()?;
        let payload: LogPayload = rmp_serde::from_slice(&bytes).ok()?;
        if payload.schema_ver != GIT_CACHE_SCHEMA {
            return None;
        }
        Some(payload.commits)
    }

    fn write_log_disk(&self, key: &LogKey, commits: &[CommitInfo]) {
        let Some(path) = self.log_path(key) else {
            return;
        };
        let payload = LogPayload {
            schema_ver: GIT_CACHE_SCHEMA,
            commits: commits.to_vec(),
        };
        let Ok(bytes) = rmp_serde::to_vec_named(&payload) else {
            return;
        };
        let _ = atomic_write(&path, &bytes);
    }

    fn read_blame_disk(&self, key: &BlameKey) -> Option<BlameResult> {
        let path = self.blame_path(key)?;
        if !path.exists() {
            return None;
        }
        let bytes = fs::read(&path).ok()?;
        let payload: BlamePayload = rmp_serde::from_slice(&bytes).ok()?;
        if payload.schema_ver != GIT_CACHE_SCHEMA {
            return None;
        }
        Some(payload.result)
    }

    fn write_blame_disk(&self, key: &BlameKey, result: &BlameResult) {
        let Some(path) = self.blame_path(key) else {
            return;
        };
        let payload = BlamePayload {
            schema_ver: GIT_CACHE_SCHEMA,
            result: result.clone(),
        };
        let Ok(bytes) = rmp_serde::to_vec_named(&payload) else {
            return;
        };
        let _ = atomic_write(&path, &bytes);
    }

    fn blame_path(&self, key: &BlameKey) -> Option<PathBuf> {
        let root = self.disk.as_ref()?;
        let path_hash = blake3::hash(key.path.as_bytes());
        let range_tag = match key.range {
            None => "all".to_string(),
            Some((lo, hi)) => format!("{lo}-{hi}"),
        };
        Some(root.join("blame").join(format!(
            "{}__{}__{range_tag}.msgpack",
            key.suspect_sha,
            hex::encode(&path_hash.as_bytes()[..8])
        )))
    }

    fn commit_files_path(&self, sha: &str) -> Option<PathBuf> {
        let root = self.disk.as_ref()?;
        Some(root.join("commit_files").join(format!("{sha}.msgpack")))
    }

    fn log_path(&self, key: &LogKey) -> Option<PathBuf> {
        let root = self.disk.as_ref()?;
        // Path filter and limit baked into the filename; head sha leads so the dir lists
        // chronologically-ish when sorted.
        let scope = match &key.path {
            None => format!("all-{}-{}", key.limit, key.include_files as u8),
            Some(p) => {
                let h = blake3::hash(p.as_bytes());
                format!("path-{}-{}", hex::encode(&h.as_bytes()[..8]), key.limit)
            }
        };
        Some(
            root.join("log")
                .join(format!("{}__{}.msgpack", key.head_sha, scope)),
        )
    }
}

fn ensure_subdir(root: &Path, sub: &str) -> Result<(), CacheError> {
    let path = root.join(sub);
    fs::create_dir_all(&path).map_err(|source| CacheError::Io { path, source })
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("msgpack.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

fn count_files(dir: &Path) -> usize {
    let mut count = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                count += 1;
            }
        }
    }
    count
}
