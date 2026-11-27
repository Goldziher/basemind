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
//!   `basemind cache` operations. Each subdirectory is held to a byte budget by background sweeps
//!   (`crate::git_cache_disk`).
//!
//! Both layers are content-addressed by the inputs the agent passed; we never
//! invalidate, only roll off via LRU. That holds only for a complete history: a shallow clone
//! answers differently for the same sha (blame stops at the graft, a boundary commit's files are
//! its whole tree, a walk ends at the boundary), and the answer changes once the clone is
//! deepened. So nothing computed while the clone is shallow is cached in either layer. The schema version is baked into every
//! payload and a mismatch on read treats the entry as a miss — the value is
//! recomputed from `gix` and the fresh-schema payload overwrites it on write, so
//! a schema bump rebuilds the git cache lazily without any destructive wipe.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use lru::LruCache;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::git::{BlameResult, ChangeKind, CommitInfo, GitError, Repo};
pub use crate::git_cache_disk::LOG_CACHE_DEFAULT_MAX_BYTES;
use crate::git_cache_disk::{DiskTier, Subdir};

/// Git-cache payload schema version, derived from [`crate::version::RELEASE_MINOR`] so it
/// moves in lock-step with the other on-disk caches (`crate::extract::SCHEMA_VER`,
/// `crate::index::INDEX_SCHEMA_VER`) on every minor-release bump. The offset started
/// from the index's `+2` convention: a fixed offset from `RELEASE_MINOR` that (a) changes
/// whenever `RELEASE_MINOR` changes and (b) differs from the historical hardcoded `1`, so
/// the next release invalidates stale git-cache payloads exactly once. A mismatch on read
/// is a cache miss — the value is recomputed from `gix` and rewritten, so this rebuilds
/// lazily and cheaply with no destructive wipe.
///
/// The offset is `+2` since commit file lists stopped naming directories and started naming a
/// merge's files only when they differ from every parent: the cached lists of earlier builds are
/// misses. `+3` since a root commit's list stopped naming the directories of its tree. `+4` since an
/// author name or email that is not UTF-8 is decoded instead of cached as `?` or empty. `+5` since
/// answers computed in a shallow clone stopped being cached: earlier builds' payloads may hold one,
/// and would outlive `git fetch --unshallow`.
pub const GIT_CACHE_SCHEMA: u16 = crate::version::RELEASE_MINOR + 5;
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
/// are a few megabytes on one repo and gigabytes on the next — `crate::engine::l1_cache` reached the
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

/// One category's LRU plus the running byte charge over its values — the `crate::engine::l1_cache`
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

    /// Raise the entry cap to `entries` when that is larger, so a category shared by several
    /// workspaces holds the most any of them asks for, never their sum.
    fn widen_entries(&mut self, entries: usize) {
        self.entries = self.entries.max(entries);
    }
}

/// The guard of one category's lock. A panic while the lock was held may have left the byte charge
/// out of step with the entries, so a poisoned category is emptied and its poison cleared: every
/// entry is recomputable, and one panicking query must not fail every later one for the process's
/// life.
fn category<K: std::hash::Hash + Eq, V>(mutex: &Mutex<Charged<K, V>>) -> MutexGuard<'_, Charged<K, V>> {
    mutex.lock().unwrap_or_else(|poisoned| {
        let mut guard = poisoned.into_inner();
        guard.clear();
        mutex.clear_poison();
        guard
    })
}

type Category<K, V> = Arc<Mutex<Charged<K, V>>>;

/// The three RAM categories. Every value is a pure function of its key within one repository (a
/// commit sha, or a head sha plus path), so the worktrees of a repository can read each other's
/// entries; see [`SharedRam`].
struct RamTiers {
    commit_files: Category<String, Arc<Vec<CommitFileChange>>>,
    log: Category<LogKey, Arc<Vec<CommitInfo>>>,
    blame: Category<BlameKey, Arc<BlameResult>>,
}

impl RamTiers {
    fn new(mem_capacity: usize, budget_bytes: u64) -> Self {
        Self {
            commit_files: Arc::new(Mutex::new(Charged::new(
                mem_capacity,
                budget_bytes,
                |files: &Arc<Vec<CommitFileChange>>| commit_files_heap_bytes(files),
            ))),
            log: Arc::new(Mutex::new(Charged::new(
                mem_capacity,
                budget_bytes,
                |commits: &Arc<Vec<CommitInfo>>| commits_heap_bytes(commits),
            ))),
            blame: Arc::new(Mutex::new(Charged::new(
                mem_capacity,
                budget_bytes,
                |result: &Arc<BlameResult>| blame_heap_bytes(result),
            ))),
        }
    }

    fn widen_entries(&self, mem_capacity: usize) {
        category(&self.commit_files).widen_entries(mem_capacity);
        category(&self.log).widen_entries(mem_capacity);
        category(&self.blame).widen_entries(mem_capacity);
    }
}

/// The live shared RAM tiers of this process, by the repository's shared directory. Weak, so the
/// last cache of a repository to close frees them whole. Each repository's tiers are held to
/// [`CATEGORY_MEM_BUDGET_BYTES`] per category however many worktrees read them, so the daemon's
/// git RAM grows with repositories, not worktrees.
static SHARED_RAM: Mutex<Vec<(PathBuf, std::sync::Weak<RamTiers>)>> = Mutex::new(Vec::new());

/// The live shared tiers for `shared_dir`, widened to `mem_capacity` entries, or new ones.
fn shared_ram(shared_dir: &Path, mem_capacity: usize) -> Arc<RamTiers> {
    let mut open = SHARED_RAM.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    open.retain(|(_, tiers)| tiers.strong_count() > 0);
    if let Some(tiers) = open
        .iter()
        .find(|(dir, _)| dir == shared_dir)
        .and_then(|(_, tiers)| tiers.upgrade())
    {
        tiers.widen_entries(mem_capacity);
        return tiers;
    }
    let tiers = Arc::new(RamTiers::new(mem_capacity, CATEGORY_MEM_BUDGET_BYTES));
    open.push((shared_dir.to_path_buf(), Arc::downgrade(&tiers)));
    tiers
}

pub struct GitCache {
    commit_files: Category<String, Arc<Vec<CommitFileChange>>>,
    log: Category<LogKey, Arc<Vec<CommitInfo>>>,
    blame: Category<BlameKey, Arc<BlameResult>>,
    /// Keeps the shared RAM tiers alive while this cache reads them; `None` for a private cache.
    _ram: Option<Arc<RamTiers>>,
    disk: Option<Arc<DiskTier>>,
    /// The tier holding `commit_files/`. Those payloads are keyed by commit sha and so identical in
    /// every worktree of a repository: this is the tier of the repository's shared directory, which
    /// is `disk` itself for a main worktree or a cache opened without a shared directory.
    commit_files_disk: Option<Arc<DiskTier>>,
}

impl GitCache {
    /// Open the cache. When `persist=true`, the disk dir is created under `basemind_dir` and swept
    /// down to its budget on a background thread; when `false`, only the RAM layer is used.
    pub fn open(basemind_dir: &Path, mem_capacity: usize, persist: bool) -> Result<Self, CacheError> {
        Self::open_shared(basemind_dir, basemind_dir, mem_capacity, persist)
    }

    /// Like [`GitCache::open`], with the sha-keyed `commit_files` tier kept under `shared_dir`
    /// instead of `basemind_dir`. Pass [`crate::git_history::shared_history_basemind_dir`] so the
    /// worktrees of one repository read and write one `commit_files` entry per commit; `log` and
    /// `blame` stay per workspace.
    pub fn open_shared(
        basemind_dir: &Path,
        shared_dir: &Path,
        mem_capacity: usize,
        persist: bool,
    ) -> Result<Self, CacheError> {
        if !persist {
            return Ok(Self::with_budget(None, mem_capacity, CATEGORY_MEM_BUDGET_BYTES));
        }
        let max_bytes = log_cache_max_bytes();
        let disk = open_disk_once(&basemind_dir.join(GIT_CACHE_DIR), max_bytes)?;
        let commit_files_disk = open_disk_once(&shared_dir.join(GIT_CACHE_DIR), max_bytes)?;
        let ram = shared_ram(shared_dir, mem_capacity);
        let mut cache = Self::with_ram(Some(disk), &ram);
        cache._ram = Some(ram);
        cache.commit_files_disk = Some(commit_files_disk);
        Ok(cache)
    }

    /// Assemble the three charged LRUs. Split out of [`GitCache::open`] so a test can pick a tiny
    /// budget instead of synthesising 32 MiB of git history; production always passes
    /// [`CATEGORY_MEM_BUDGET_BYTES`].
    fn with_budget(disk: Option<Arc<DiskTier>>, mem_capacity: usize, budget_bytes: u64) -> Self {
        Self::with_ram(disk, &RamTiers::new(mem_capacity, budget_bytes))
    }

    /// A cache reading and writing `ram`'s categories; the caller keeps `ram` alive through `_ram`.
    fn with_ram(disk: Option<Arc<DiskTier>>, ram: &RamTiers) -> Self {
        Self {
            commit_files: Arc::clone(&ram.commit_files),
            log: Arc::clone(&ram.log),
            blame: Arc::clone(&ram.blame),
            _ram: None,
            commit_files_disk: disk.clone(),
            disk,
        }
    }

    /// Look up (or compute) the per-file change list for a commit. Sha-keyed: result is
    /// immutable, so any hit is correct forever. Computed in a shallow clone, it is not cached.
    pub fn commit_files(&self, repo: &Repo, commit_sha: &str) -> Result<Arc<Vec<CommitFileChange>>, CacheError> {
        if let Some(hit) = category(&self.commit_files).lru.get(commit_sha).cloned() {
            return Ok(hit);
        }
        if let Some(disk) = self.read_commit_files_disk(commit_sha) {
            let arc = Arc::new(disk);
            category(&self.commit_files).admit(commit_sha.to_string(), Arc::clone(&arc));
            return Ok(arc);
        }
        let shallow = repo.is_shallow();
        let arc = Arc::new(repo.commit_files_uncached(commit_sha)?);
        if shallow {
            return Ok(arc);
        }
        category(&self.commit_files).admit(commit_sha.to_string(), Arc::clone(&arc));
        self.write_commit_files_disk(commit_sha, &arc);
        Ok(arc)
    }

    /// Look up (or compute) a log slice. Keyed by (head_sha, path, limit, include_files);
    /// commits past `head_sha` are immutable, so the cached walk stays valid. A walk of a shallow
    /// clone ends at its boundary and is not cached.
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
        if let Some(hit) = category(&self.log).lru.get(&key).cloned() {
            return Ok(hit);
        }
        if let Some(disk) = self.read_log_disk(&key) {
            let arc = Arc::new(disk);
            category(&self.log).admit(key.clone(), Arc::clone(&arc));
            return Ok(arc);
        }
        let shallow = repo.is_shallow();
        let commits = match path {
            Some(p) => repo.log_for_path_at(head_sha, p, limit as usize)?,
            None => repo.log_paths_at(head_sha, limit as usize, include_files)?,
        };
        let arc = Arc::new(commits);
        if shallow {
            return Ok(arc);
        }
        category(&self.log).admit(key.clone(), Arc::clone(&arc));
        self.write_log_disk(&key, &arc);
        Ok(arc)
    }

    /// Look up (or compute) a blame for `(suspect_sha, path, range)`. Sha-keyed: caches
    /// forever. Cost of cold compute scales with file history size. A blame in a shallow clone
    /// stops at the graft and is not cached.
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
        if let Some(hit) = category(&self.blame).lru.get(&key).cloned() {
            return Ok(hit);
        }
        if let Some(disk) = self.read_blame_disk(&key) {
            let arc = Arc::new(disk);
            category(&self.blame).admit(key.clone(), Arc::clone(&arc));
            return Ok(arc);
        }
        let shallow = repo.is_shallow();
        let arc = Arc::new(repo.blame_file(suspect_sha, path, range)?);
        if shallow {
            return Ok(arc);
        }
        category(&self.blame).admit(key.clone(), Arc::clone(&arc));
        self.write_blame_disk(&key, &arc);
        Ok(arc)
    }

    /// Drop the on-disk cache + reset RAM. Returns the number of disk files removed.
    pub fn clear(&self) -> Result<usize, CacheError> {
        let mut removed = 0usize;
        if let Some(disk) = &self.disk
            && disk.root().exists()
        {
            let root = disk.root();
            removed += count_files(root);
            fs::remove_dir_all(root).map_err(|source| CacheError::Io {
                path: root.to_path_buf(),
                source,
            })?;
            for sub in Subdir::ALL {
                ensure_subdir(root, sub.name())?;
            }
            disk.reset();
        }
        if let Some(shared) = self
            .commit_files_disk
            .as_ref()
            .filter(|shared| self.is_foreign_tier(shared))
        {
            let dir = shared.dir(Subdir::CommitFiles);
            if dir.exists() {
                removed += count_files(&dir);
                fs::remove_dir_all(&dir).map_err(|source| CacheError::Io {
                    path: dir.clone(),
                    source,
                })?;
            }
            ensure_subdir(shared.root(), Subdir::CommitFiles.name())?;
        }
        category(&self.commit_files).clear();
        category(&self.log).clear();
        category(&self.blame).clear();
        Ok(removed)
    }

    /// Whether `tier` is not this workspace's own, so a clear of the own tier has not covered it.
    fn is_foreign_tier(&self, tier: &Arc<DiskTier>) -> bool {
        self.disk.as_ref().is_none_or(|own| !Arc::ptr_eq(own, tier))
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
        let (Some(disk), Some(path)) = (&self.commit_files_disk, self.commit_files_path(sha)) else {
            return;
        };
        let payload = CommitFilesOut {
            schema_ver: GIT_CACHE_SCHEMA,
            files,
        };
        let Ok(bytes) = rmp_serde::to_vec_named(&payload) else {
            return;
        };
        disk.write(Subdir::CommitFiles, &path, &bytes);
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
        let (Some(disk), Some(path)) = (&self.disk, self.log_path(key)) else {
            return;
        };
        let payload = LogOut {
            schema_ver: GIT_CACHE_SCHEMA,
            commits,
        };
        let Ok(bytes) = rmp_serde::to_vec_named(&payload) else {
            return;
        };
        disk.write(Subdir::Log, &path, &bytes);
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
        let (Some(disk), Some(path)) = (&self.disk, self.blame_path(key)) else {
            return;
        };
        let payload = BlameOut {
            schema_ver: GIT_CACHE_SCHEMA,
            result,
        };
        let Ok(bytes) = rmp_serde::to_vec_named(&payload) else {
            return;
        };
        disk.write(Subdir::Blame, &path, &bytes);
    }

    fn blame_path(&self, key: &BlameKey) -> Option<PathBuf> {
        let dir = self.disk.as_ref()?.dir(Subdir::Blame);
        let path_hash = blake3::hash(key.path.as_bytes());
        let range_tag = match key.range {
            None => "all".to_string(),
            Some((lo, hi)) => format!("{lo}-{hi}"),
        };
        Some(dir.join(format!(
            "{}__{}__{range_tag}.msgpack",
            key.suspect_sha,
            hex::encode(&path_hash.as_bytes()[..8])
        )))
    }

    fn commit_files_path(&self, sha: &str) -> Option<PathBuf> {
        let dir = self.commit_files_disk.as_ref()?.dir(Subdir::CommitFiles);
        Some(dir.join(format!("{sha}.msgpack")))
    }

    fn log_path(&self, key: &LogKey) -> Option<PathBuf> {
        let dir = self.disk.as_ref()?.dir(Subdir::Log);
        let scope = match &key.path {
            None => format!("all-{}-{}", key.limit, key.include_files as u8),
            Some(p) => {
                let h = blake3::hash(p.as_bytes());
                format!("path-{}-{}", hex::encode(&h.as_bytes()[..8]), key.limit)
            }
        };
        Some(dir.join(format!("{}__{}.msgpack", key.head_sha, scope)))
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

fn log_cache_max_bytes() -> u64 {
    crate::config::engine::with(|config| config.git.cache_max_bytes)
}

/// Create the disk tier's subdirectories under `root` and start the sweep that holds each to
/// `max_bytes`, on a background thread so an open never stats the cache.
fn open_disk(root: &Path, max_bytes: u64) -> Result<Arc<DiskTier>, CacheError> {
    for sub in Subdir::ALL {
        ensure_subdir(root, sub.name())?;
    }
    let disk = DiskTier::new(root.to_path_buf(), max_bytes);
    disk.sweep_in_background();
    Ok(disk)
}

/// The disk tiers opened by this process, by root. Several workspaces of one repository open the
/// same shared root, and each opening it afresh would give that directory a sweeper per workspace,
/// each with its own running total.
static OPEN_TIERS: Mutex<Vec<(PathBuf, std::sync::Weak<DiskTier>)>> = Mutex::new(Vec::new());

/// The live tier over `root`, or a newly opened one.
fn open_disk_once(root: &Path, max_bytes: u64) -> Result<Arc<DiskTier>, CacheError> {
    let mut open = OPEN_TIERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    open.retain(|(_, tier)| tier.strong_count() > 0);
    if let Some(tier) = open
        .iter()
        .find(|(open_root, _)| open_root == root)
        .and_then(|(_, tier)| tier.upgrade())
    {
        return Ok(tier);
    }
    let tier = open_disk(root, max_bytes)?;
    open.push((root.to_path_buf(), Arc::downgrade(&tier)));
    Ok(tier)
}

#[cfg(test)]
#[path = "git_cache_tests.rs"]
mod tests;
