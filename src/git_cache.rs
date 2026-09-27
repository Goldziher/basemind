//! RAM LRU + disk cache for sha-keyed git artifacts.
//!
//! What lives in here is everything that is expensive to compute and either
//! (a) immutable forever (anything keyed by a commit sha — `commit_files`,
//! per-blame results, etc.) or (b) immutable per HEAD position (the
//! `log` walks, which we key by the resolved HEAD sha at the time of the
//! request so a stale entry still describes a valid past).
//!
//! Two layers:
//! - **RAM** — `lru::LruCache` per category, behind a `Mutex`. Bounded by entry
//!   count (capacity from `ServeArgs`) *and* by bytes
//!   (`CATEGORY_MEM_BUDGET_BYTES`), because these values have no natural size.
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

/// `(path, change_kind)` for one file in a commit's tree-against-parent diff.
type CommitFileChange = (crate::path::RelPath, ChangeKind);

/// Byte ceiling for each RAM category (32 MiB), the bound an entry cap cannot express.
///
/// `mem_capacity` bounds how MANY values a category holds, never how large they are, and none of
/// these values has a natural size: one `CommitInfo` carries the full commit body plus one entry per
/// path the commit touched, and one `BlameResult` is a hunk per contiguous run of lines in a whole
/// file. A lockfile merge is orders of magnitude bigger than a typo fix, so the same 1024 entries
/// are a few megabytes on one repo and gigabytes on the next — `crate::mcp::l1_cache` reached the
/// same conclusion about decoded outlines: an entry-count LRU is the same unbounded structure with a
/// different constant.
///
/// 32 MiB per category is 96 MiB per workspace. A typical entry is single-digit KB, so on any
/// ordinary session the entry cap still binds first and the byte ceiling never evicts anything; it
/// exists for the pathological tail (huge merges, blames of generated files) where 1024 entries
/// would otherwise be unbounded. It stays well under the 256 MiB one workspace's outline cache is
/// already allowed (`[resources] max_map_cache_mb`), which keeps git artifacts from becoming the
/// dominant term in a daemon holding 16 hot workspaces.
const CATEGORY_MEM_BUDGET_BYTES: u64 = 32 * 1024 * 1024;

/// One category's LRU plus the running byte charge over its values — the `crate::mcp::l1_cache`
/// shape, generalised over the three value types this cache holds.
///
/// The `LruCache` is `unbounded` and the entry cap is enforced here so that EVERY eviction passes
/// through the byte accounting: `LruCache::put` on a capacity-bounded map drops the LRU entry
/// silently without handing it back, which would leave its bytes charged forever and make the
/// running total drift up until the cache evicted on every insert.
struct Charged<K, V> {
    lru: LruCache<K, V>,
    /// Max entries, the pre-existing `mem_capacity` bound.
    entries: usize,
    /// Byte ceiling; `0` means unbounded, matching `L1Cache`'s sentinel.
    budget_bytes: u64,
    charged: u64,
    /// Approximate resident cost of one value. A `fn` pointer rather than a trait so the three
    /// estimators stay plain functions a unit test can call directly.
    heap_bytes: fn(&V) -> u64,
}

impl<K: std::hash::Hash + Eq, V> Charged<K, V> {
    fn new(entries: usize, budget_bytes: u64, heap_bytes: fn(&V) -> u64) -> Self {
        Self {
            lru: LruCache::unbounded(),
            entries: entries.max(1),
            budget_bytes,
            charged: 0,
            heap_bytes,
        }
    }

    /// Insert `value`, then evict least-recently-used entries until both bounds hold.
    ///
    /// The entry just admitted is never the one evicted, even when it alone exceeds the budget: a
    /// blame of a generated file must still be returnable under any ceiling, or a cache that exists
    /// to save work would start withholding answers.
    fn admit(&mut self, key: K, value: V) {
        let bytes = (self.heap_bytes)(&value);
        if let Some(previous) = self.lru.put(key, value) {
            self.charged = self.charged.saturating_sub((self.heap_bytes)(&previous));
        }
        self.charged = self.charged.saturating_add(bytes);
        while self.lru.len() > self.entries {
            self.evict_lru();
        }
        if self.budget_bytes == 0 {
            return;
        }
        while self.charged > self.budget_bytes && self.lru.len() > 1 {
            self.evict_lru();
        }
    }

    fn evict_lru(&mut self) {
        if let Some((_, evicted)) = self.lru.pop_lru() {
            self.charged = self.charged.saturating_sub((self.heap_bytes)(&evicted));
        }
    }

    fn clear(&mut self) {
        self.lru.clear();
        self.charged = 0;
    }
}

pub struct GitCache {
    commit_files: Mutex<Charged<String, Arc<Vec<CommitFileChange>>>>,
    log: Mutex<Charged<LogKey, Arc<Vec<CommitInfo>>>>,
    blame: Mutex<Charged<BlameKey, Arc<BlameResult>>>,
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
            evict_log_cache(&root, log_cache_max_bytes_from_env());
            Some(root)
        } else {
            None
        };
        Ok(Self::with_budget(disk, mem_capacity, CATEGORY_MEM_BUDGET_BYTES))
    }

    /// Assemble the three charged LRUs. Split out of [`GitCache::open`] so a test can pick a tiny
    /// budget instead of synthesising 32 MiB of git history; production always passes
    /// [`CATEGORY_MEM_BUDGET_BYTES`].
    fn with_budget(disk: Option<PathBuf>, mem_capacity: usize, budget_bytes: u64) -> Self {
        Self {
            commit_files: Mutex::new(Charged::new(
                mem_capacity,
                budget_bytes,
                |files: &Arc<Vec<CommitFileChange>>| commit_files_heap_bytes(files),
            )),
            log: Mutex::new(Charged::new(
                mem_capacity,
                budget_bytes,
                |commits: &Arc<Vec<CommitInfo>>| commits_heap_bytes(commits),
            )),
            blame: Mutex::new(Charged::new(mem_capacity, budget_bytes, |result: &Arc<BlameResult>| {
                blame_heap_bytes(result)
            })),
            disk,
        }
    }

    /// Look up (or compute) the per-file change list for a commit. Sha-keyed: result is
    /// immutable, so any hit is correct forever.
    pub fn commit_files(&self, repo: &Repo, commit_sha: &str) -> Result<Arc<Vec<CommitFileChange>>, CacheError> {
        if let Some(hit) = self.commit_files.lock().unwrap().lru.get(commit_sha).cloned() {
            return Ok(hit);
        }
        if let Some(disk) = self.read_commit_files_disk(commit_sha) {
            let arc = Arc::new(disk);
            self.commit_files
                .lock()
                .unwrap()
                .admit(commit_sha.to_string(), Arc::clone(&arc));
            return Ok(arc);
        }
        let computed = repo.commit_files_uncached(commit_sha)?;
        let arc = Arc::new(computed);
        self.commit_files
            .lock()
            .unwrap()
            .admit(commit_sha.to_string(), Arc::clone(&arc));
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
        if let Some(hit) = self.log.lock().unwrap().lru.get(&key).cloned() {
            return Ok(hit);
        }
        if let Some(disk) = self.read_log_disk(&key) {
            let arc = Arc::new(disk);
            self.log.lock().unwrap().admit(key.clone(), Arc::clone(&arc));
            return Ok(arc);
        }
        let commits = match path {
            Some(p) => repo.log_for_path(p, limit as usize)?,
            None => repo.log_paths(limit as usize, include_files)?,
        };
        let arc = Arc::new(commits);
        self.log.lock().unwrap().admit(key.clone(), Arc::clone(&arc));
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
        if let Some(hit) = self.blame.lock().unwrap().lru.get(&key).cloned() {
            return Ok(hit);
        }
        if let Some(disk) = self.read_blame_disk(&key) {
            let arc = Arc::new(disk);
            self.blame.lock().unwrap().admit(key.clone(), Arc::clone(&arc));
            return Ok(arc);
        }
        let computed = repo.blame_file(suspect_sha, path, range)?;
        let arc = Arc::new(computed);
        self.blame.lock().unwrap().admit(key.clone(), Arc::clone(&arc));
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

/// Approximate resident cost of one cached commit-files list, in bytes.
///
/// Approximate on purpose: the charge shapes eviction, and an exact walk would cost more than the
/// eviction it informs. What it must not do is undercount the heap, which is where the size actually
/// lives — a `RelPath` is a `BString`, so the path bytes are counted on top of the tuple's own
/// footprint.
fn commit_files_heap_bytes(files: &[CommitFileChange]) -> u64 {
    let mut total = std::mem::size_of::<Vec<CommitFileChange>>() as u64;
    total += std::mem::size_of_val(files) as u64;
    total += files.iter().map(|(path, _)| path.as_bytes().len() as u64).sum::<u64>();
    total
}

/// Approximate resident cost of one cached log walk, in bytes.
///
/// Every owned `String` on a `CommitInfo` is counted, `body` above all: a commit message is
/// unbounded, and a walk of 1000 commits with long bodies is the case the byte ceiling exists for.
fn commits_heap_bytes(commits: &[CommitInfo]) -> u64 {
    let mut total = std::mem::size_of::<Vec<CommitInfo>>() as u64;
    total += std::mem::size_of_val(commits) as u64;
    for commit in commits {
        total += (commit.sha.len()
            + commit.short_sha.len()
            + commit.summary.len()
            + commit.author.len()
            + commit.author_email.len()
            + commit.body.len()) as u64;
        total += std::mem::size_of_val(commit.files.as_slice()) as u64;
        total += commit
            .files
            .iter()
            .map(|(path, _)| path.as_bytes().len() as u64)
            .sum::<u64>();
    }
    total
}

/// Approximate resident cost of one cached blame, in bytes.
///
/// Scales with the hunk count, which scales with the blamed file: a file whose every line came from
/// a different commit is one hunk per line, each carrying its own sha, author and summary.
fn blame_heap_bytes(result: &BlameResult) -> u64 {
    let mut total = std::mem::size_of::<BlameResult>() as u64;
    total += result.path.as_bytes().len() as u64;
    total += result.suspect_sha.len() as u64;
    total += result.truncated_reason.as_ref().map_or(0, String::len) as u64;
    total += std::mem::size_of_val(result.hunks.as_slice()) as u64;
    for hunk in &result.hunks {
        total += (hunk.commit_sha.len() + hunk.short_sha.len() + hunk.author.len() + hunk.summary.len()) as u64;
        total += hunk.source_path.as_ref().map_or(0, |path| path.as_bytes().len()) as u64;
    }
    total
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
    entries.sort_by_key(|(_, _, mtime)| *mtime);
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
    use super::*;
    use crate::git::BlameHunk;
    use crate::path::RelPath;

    /// One log entry whose charge is dominated by `body_len`: a commit message is unbounded, and it
    /// is exactly the kind of payload an entry cap cannot see.
    fn commits(sha: &str, body_len: usize) -> Arc<Vec<CommitInfo>> {
        Arc::new(vec![CommitInfo {
            sha: sha.to_string(),
            short_sha: sha.chars().take(7).collect(),
            summary: "summary line".to_string(),
            author: "author".to_string(),
            author_email: "author@example.com".to_string(),
            author_time_unix: 0,
            body: "b".repeat(body_len),
            files: vec![(RelPath::from("src/git_cache.rs"), ChangeKind::Modified)],
        }])
    }

    fn log_key(head_sha: &str) -> LogKey {
        LogKey {
            head_sha: head_sha.to_string(),
            path: None,
            limit: 10,
            include_files: false,
        }
    }

    /// A blame of a file whose every line came from a different commit — one hunk per line, the
    /// shape that makes a single blame arbitrarily large.
    fn blame_result(hunks: usize) -> BlameResult {
        BlameResult {
            path: RelPath::from("src/git_cache.rs"),
            suspect_sha: "a".repeat(40),
            hunks: (0..hunks)
                .map(|i| BlameHunk {
                    commit_sha: format!("{i:040x}"),
                    short_sha: format!("{i:07x}"),
                    start_line: 1,
                    len: 1,
                    source_start_line: 1,
                    author: "author".to_string(),
                    author_time_unix: 0,
                    summary: "one line changed".to_string(),
                    source_path: None,
                })
                .collect(),
            truncated_reason: None,
        }
    }

    /// The charge must scale with a value's own heap, not with the entry count — the whole reason
    /// the second bound is in bytes. A 100 KB commit body charges ~100 KB more than a 1-byte one,
    /// and 1000 blame hunks charge at least their own element storage more than one hunk.
    #[test]
    fn heap_bytes_track_payload_not_entry_count() {
        let small = commits_heap_bytes(&commits("aaa", 1));
        let large = commits_heap_bytes(&commits("aaa", 100_000));
        assert!(large > small + 99_000, "small={small} large={large}");

        let one_hunk = blame_heap_bytes(&blame_result(1));
        let many_hunks = blame_heap_bytes(&blame_result(1000));
        let element_storage = (1000 * std::mem::size_of::<BlameHunk>()) as u64;
        assert!(
            many_hunks > one_hunk + element_storage,
            "one={one_hunk} many={many_hunks}"
        );

        // Path bytes live on the heap behind each `RelPath`, so a long-path commit must charge more
        // than the tuple array alone.
        let files: Vec<CommitFileChange> = (0..500)
            .map(|i| {
                (
                    RelPath::from(format!("crates/pack/src/module_{i:04}/deeply/nested/file.rs")),
                    ChangeKind::Added,
                )
            })
            .collect();
        let tuples = (500 * std::mem::size_of::<CommitFileChange>()) as u64;
        assert!(
            commit_files_heap_bytes(&files) > tuples + 500 * 40,
            "paths must be charged"
        );
    }

    /// Under budget nothing is evicted: the byte ceiling is an additional bound, not a tighter one,
    /// so a category the entry cap allows and the budget covers keeps every entry.
    #[test]
    fn under_budget_keeps_every_entry() {
        let cache = GitCache::with_budget(None, 64, 1024 * 1024);
        for i in 0..8 {
            let value = commits(&format!("sha{i}"), 1024);
            cache.log.lock().unwrap().admit(log_key(&format!("head{i}")), value);
        }
        let mut guard = cache.log.lock().unwrap();
        assert_eq!(guard.lru.len(), 8);
        assert!(guard.charged < 1024 * 1024, "charged={}", guard.charged);
        for i in 0..8 {
            assert!(
                guard.lru.get(&log_key(&format!("head{i}"))).is_some(),
                "entry {i} survives"
            );
        }
    }

    /// Past the byte budget the least-recently-used entry is the one that goes, with the entry cap
    /// nowhere near binding — which is the defect this bound closes.
    #[test]
    fn byte_budget_evicts_least_recently_used() {
        let cache = GitCache::with_budget(None, 64, 50_000);
        let (a, b, c) = (log_key("aaa"), log_key("bbb"), log_key("ccc"));
        cache.log.lock().unwrap().admit(a.clone(), commits("aaa", 20_000));
        cache.log.lock().unwrap().admit(b.clone(), commits("bbb", 20_000));
        // Reading `a` back makes `b` the oldest, so the next insert must take `b` and not `a`.
        assert!(cache.log.lock().unwrap().lru.get(&a).is_some());
        cache.log.lock().unwrap().admit(c.clone(), commits("ccc", 20_000));

        let mut guard = cache.log.lock().unwrap();
        assert!(guard.charged <= 50_000, "charged={}", guard.charged);
        assert!(guard.lru.get(&a).is_some(), "the recently-read entry survives");
        assert!(guard.lru.get(&c).is_some(), "the newest entry survives");
        assert!(
            guard.lru.get(&b).is_none(),
            "the least-recently-used entry is the one evicted"
        );
    }

    /// An entry larger than the whole budget is admitted anyway — a blame of a generated file must
    /// still be returnable — and then yields the moment anything else needs the room.
    #[test]
    fn oversized_entry_is_admitted_then_displaced() {
        let cache = GitCache::with_budget(None, 64, 8192);
        let huge = log_key("huge");
        cache
            .log
            .lock()
            .unwrap()
            .admit(huge.clone(), commits("huge", 1_000_000));
        {
            let mut guard = cache.log.lock().unwrap();
            assert!(
                guard.lru.get(&huge).is_some(),
                "a value over budget is still returnable"
            );
            assert_eq!(guard.lru.len(), 1);
        }
        let small = log_key("small");
        cache.log.lock().unwrap().admit(small.clone(), commits("small", 16));

        let mut guard = cache.log.lock().unwrap();
        assert!(guard.lru.get(&small).is_some());
        assert!(
            guard.lru.get(&huge).is_none(),
            "the oversized entry goes on the next insert"
        );
        assert!(guard.charged <= 8192, "charged={}", guard.charged);
    }

    /// An eviction forced by the ENTRY cap must discharge its bytes too. `LruCache::put` on a
    /// capacity-bounded map drops the LRU value without returning it, so charging around that
    /// instead of through it would leak the charge until the budget evicted on every insert.
    #[test]
    fn entry_cap_eviction_still_discharges_bytes() {
        let cache = GitCache::with_budget(None, 2, 1024 * 1024);
        for i in 0..8 {
            let value = commits(&format!("sha{i}"), 4096);
            cache.log.lock().unwrap().admit(log_key(&format!("head{i}")), value);
        }
        let guard = cache.log.lock().unwrap();
        assert_eq!(guard.lru.len(), 2, "the entry cap still binds");
        assert!(
            guard.charged < 3 * 4096,
            "charge must describe the two live entries only, got {}",
            guard.charged
        );
    }

    /// Concurrent same-process writers to the *same* destination key must not clobber each
    /// other's temp file mid-write: the final file is always one complete payload, never a
    /// torn/empty file. PID-only temp names broke this; the per-process sequence fixes it.
    #[test]
    fn concurrent_same_key_writes_never_tear() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("k.msgpack");
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
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be renamed away, not orphaned");
    }
}
