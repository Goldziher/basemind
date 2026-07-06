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
//! - **Disk** — sha-keyed `.msgpack` files under `.basemind/git-cache/`. Optional;
//!   `GitCache::open(.., persist=false)` skips disk altogether for ephemeral
//!   `basemind cache` operations.
//!
//! Both layers are content-addressed by the inputs the agent passed; we never
//! invalidate, only roll off via LRU. The schema version is baked into every
//! payload and a mismatch on read treats the entry as a miss — the value is
//! recomputed from `gix` and the fresh-schema payload overwrites it on write, so
//! a schema bump rebuilds the git cache lazily without any destructive wipe.

use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lru::LruCache;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::git::{BlameResult, ChangeKind, CommitInfo, GitError, Repo};

/// Git-cache payload schema version, derived from [`crate::version::RELEASE_MINOR`] so it
/// moves in lock-step with the other on-disk caches (`crate::extract::SCHEMA_VER`,
/// `crate::index::INDEX_SCHEMA_VER`) on every minor-release bump. The `+1` offset mirrors
/// the index's `+2` convention: a fixed offset from `RELEASE_MINOR` that (a) changes
/// whenever `RELEASE_MINOR` changes and (b) differs from the historical hardcoded `1`, so
/// the next release invalidates stale git-cache payloads exactly once. A mismatch on read
/// is a cache miss — the value is recomputed from `gix` and rewritten, so this rebuilds
/// lazily and cheaply with no destructive wipe.
pub const GIT_CACHE_SCHEMA: u16 = crate::version::RELEASE_MINOR + 1;
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

// Read side: owned structs for deserialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommitFilesPayload {
    schema_ver: u16,
    files: Vec<(crate::path::RelPath, ChangeKind)>,
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

// Write side: borrow-only structs so the write helpers serialize directly from the
// caller's slice/reference without cloning the payload. The field names match the
// read-side structs above so msgpack round-trips via `rmp_serde::to_vec_named`.
#[derive(Serialize)]
struct CommitFilesOut<'a> {
    schema_ver: u16,
    files: &'a [(crate::path::RelPath, ChangeKind)],
}

#[derive(Serialize)]
struct LogOut<'a> {
    schema_ver: u16,
    commits: &'a [CommitInfo],
}

#[derive(Serialize)]
struct BlameOut<'a> {
    schema_ver: u16,
    result: &'a BlameResult,
}

// ─── cache keys ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlameKey {
    pub suspect_sha: String,
    pub path: crate::path::RelPath,
    pub range: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogKey {
    /// HEAD sha at the time the entry was computed. Two requests with the same head_sha
    /// always return the same walk; once HEAD moves, the new sha defines a new key.
    pub head_sha: String,
    /// Optional path filter (Some for `commits_touching`, None for `recent_changes`).
    pub path: Option<crate::path::RelPath>,
    pub limit: u32,
    pub include_files: bool,
}

// ─── main cache ──────────────────────────────────────────────────────────────

/// `(path, change_kind)` for one file in a commit's tree-against-parent diff.
type CommitFileChange = (crate::path::RelPath, ChangeKind);

pub struct GitCache {
    commit_files: Mutex<LruCache<String /* sha40 */, Arc<Vec<CommitFileChange>>>>,
    log: Mutex<LruCache<LogKey, Arc<Vec<CommitInfo>>>>,
    blame: Mutex<LruCache<BlameKey, Arc<BlameResult>>>,
    disk: Option<PathBuf>,
}

impl GitCache {
    /// Open the cache. When `persist=true`, the disk dir is created under `basemind_dir`;
    /// when `false`, only the RAM layer is used.
    pub fn open(basemind_dir: &Path, mem_capacity: usize, persist: bool) -> Result<Self, CacheError> {
        let disk = if persist {
            let root = basemind_dir.join(GIT_CACHE_DIR);
            ensure_subdir(&root, "commit_files")?;
            ensure_subdir(&root, "log")?;
            ensure_subdir(&root, "blame")?;
            // One-shot LRU sweep of the HEAD-anchored log cache. New `head_sha` after
            // every rebase/pull would otherwise grow this directory without bound. Sized
            // to fit comfortably in a typical project's `.basemind/` budget; tune via
            // `BASEMIND_GIT_CACHE_LOG_MAX_BYTES` (in bytes).
            evict_log_cache(&root, log_cache_max_bytes_from_env());
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
    pub fn commit_files(&self, repo: &Repo, commit_sha: &str) -> Result<Arc<Vec<CommitFileChange>>, CacheError> {
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
        path: Option<&crate::path::RelPath>,
        limit: u32,
        include_files: bool,
    ) -> Result<Arc<Vec<CommitInfo>>, CacheError> {
        let key = LogKey {
            head_sha: head_sha.to_string(),
            path: path.cloned(),
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
        path: &crate::path::RelPath,
        range: Option<(u32, u32)>,
    ) -> Result<Arc<BlameResult>, CacheError> {
        let key = BlameKey {
            suspect_sha: suspect_sha.to_string(),
            path: path.clone(),
            range,
        };
        if let Some(hit) = self.blame.lock().unwrap().get(&key).cloned() {
            return Ok(hit);
        }
        if let Some(disk) = self.read_blame_disk(&key) {
            let arc = Arc::new(disk);
            self.blame.lock().unwrap().put(key.clone(), Arc::clone(&arc));
            return Ok(arc);
        }
        let computed = repo.blame_file(suspect_sha, path, range)?;
        let arc = Arc::new(computed);
        self.blame.lock().unwrap().put(key.clone(), Arc::clone(&arc));
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

    fn read_commit_files_disk(&self, sha: &str) -> Option<Vec<CommitFileChange>> {
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

    fn write_commit_files_disk(&self, sha: &str, files: &[CommitFileChange]) {
        let Some(path) = self.commit_files_path(sha) else {
            return;
        };
        let payload = CommitFilesOut {
            schema_ver: GIT_CACHE_SCHEMA,
            files,
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
        let payload = LogOut {
            schema_ver: GIT_CACHE_SCHEMA,
            commits,
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
        let payload = BlameOut {
            schema_ver: GIT_CACHE_SCHEMA,
            result,
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
        Some(root.join("log").join(format!("{}__{}.msgpack", key.head_sha, scope)))
    }
}

fn ensure_subdir(root: &Path, sub: &str) -> Result<(), CacheError> {
    let path = root.join(sub);
    fs::create_dir_all(&path).map_err(|source| CacheError::Io { path, source })
}

/// Monotonic per-process counter that makes temp-file names unique. PID alone collides when
/// two threads in the *same* process write the same cache key concurrently — both would pick
/// `<key>.msgpack.<pid>.tmp` and clobber each other mid-write. The counter splits them.
static ATOMIC_WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let seq = ATOMIC_WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("msgpack.{}.{seq}.tmp", std::process::id()));
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

/// Default disk budget for the HEAD-keyed log subdirectory (256 MiB). Tuneable via
/// `BASEMIND_GIT_CACHE_LOG_MAX_BYTES`; setting it to 0 disables eviction.
const LOG_CACHE_DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

fn log_cache_max_bytes_from_env() -> u64 {
    std::env::var("BASEMIND_GIT_CACHE_LOG_MAX_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(LOG_CACHE_DEFAULT_MAX_BYTES)
}

/// Mtime-LRU sweep of `<git-cache>/log/`. Cheap one-shot pass at process start: stat every
/// file, sum sizes, and if over budget delete the oldest until under. The log subdir is the
/// only one with HEAD-derived keys (commit_files + blame are sha-derived and immortal).
/// Errors during traversal/removal are swallowed — cache size is best-effort, not load-bearing.
pub(crate) fn evict_log_cache(cache_root: &Path, max_bytes: u64) {
    if max_bytes == 0 {
        return;
    }
    let log_dir = cache_root.join("log");
    let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    if let Ok(rd) = fs::read_dir(&log_dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(md) = entry.metadata() else { continue };
            if !md.is_file() {
                continue;
            }
            let size = md.len();
            let mtime = md.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            total = total.saturating_add(size);
            entries.push((path, size, mtime));
        }
    }
    if total <= max_bytes {
        return;
    }
    entries.sort_by_key(|(_, _, mtime)| *mtime); // oldest first
    let mut over = total - max_bytes;
    for (path, size, _) in entries {
        if over == 0 {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            over = over.saturating_sub(size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::atomic_write;

    /// Concurrent same-process writers to the *same* destination key must not clobber each
    /// other's temp file mid-write: the final file is always one complete payload, never a
    /// torn/empty file. PID-only temp names broke this; the per-process sequence fixes it.
    #[test]
    fn concurrent_same_key_writes_never_tear() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("k.msgpack");
        // Distinct, equal-length payloads so a torn write would be detectable by content.
        let payloads: Vec<Vec<u8>> = (0..16u8).map(|i| vec![i; 4096]).collect();
        std::thread::scope(|scope| {
            for p in &payloads {
                let dest = dest.clone();
                scope.spawn(move || atomic_write(&dest, p).expect("atomic_write"));
            }
        });
        let got = std::fs::read(&dest).expect("dest exists");
        assert_eq!(got.len(), 4096, "final file must be a complete payload");
        let byte = got[0];
        assert!(
            got.iter().all(|&b| b == byte) && byte < 16,
            "final file must be exactly one writer's payload, not a mix"
        );
        // No stray temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be renamed away, not orphaned");
    }
}
