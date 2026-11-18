//! On-disk layout of basemind's machine-global cache: where the cache root lives, how a worktree
//! root maps to a per-workspace cache directory, and the `workspace.json` marker that records that
//! mapping.
//!
//! Carved out of `store.rs` (which was over the 1000-line module cap) because these items answer a
//! single question — *which directory holds what* — and change for a single reason: the cache
//! layout. The [`Store`](crate::store::Store) handle, the msgpack index, and the blob accessors that
//! consume these paths stay in `store.rs` / `store_blob.rs`. `store.rs` re-exports every item here,
//! so callers keep importing them from `crate::store`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data_root::DataRoot;
use crate::hashing;
use crate::store_blob::write_bytes_atomic;

pub const INDEX_FILE: &str = "index.msgpack";
pub const BLOBS_DIR: &str = "blobs";
pub const LOCK_FILE: &str = ".lock";
/// Environment override for the data home ([`DataRoot`](crate::data_root::DataRoot)). The CLI maps
/// it onto the engine configuration; the test-isolation helper sets it so a child process inherits
/// the per-process temp dir.
pub const DATA_HOME_ENV: &str = "BASEMIND_DATA_HOME";
/// Sub-directory of the data home that holds all basemind cache state (`blobs/` + `workspaces/`).
pub const CACHE_DIR: &str = "cache";
/// Sub-directory of the cache holding per-workspace state, keyed by [`workspace_key`].
pub const WORKSPACES_DIR: &str = "workspaces";
/// Sidecar JSON written next to `.lock` naming the live lock holder (command + pid +
/// timestamp). Read on contention so the error can name the *actual* holder instead of a
/// hardcoded guess. Best-effort: a missing/corrupt sidecar degrades to a generic message.
pub const LOCK_META_FILE: &str = ".lock.meta";
/// Sidecar JSON written next to `.lock` recording the canonical worktree root a workspace cache dir
/// was keyed from. The dir name is a ONE-WAY blake3 of that path ([`workspace_key`]), so without
/// this marker nothing can tell whether a workspace's repo still exists — and an orphaned workspace
/// keeps voting in the daemon's cross-workspace blob GC, pinning its blobs in the machine-global
/// store forever (the cache then only ever grows). See [`crate::store_gc_workspace`], which reads it
/// to reap orphans. Written idempotently on every store open so pre-existing (pre-marker) workspace
/// dirs self-heal; best-effort and non-load-bearing, exactly like `.lock.meta` — a missing marker
/// only means the dir is unverifiable, and the reaper's conservative policy keeps it.
pub const WORKSPACE_MARKER_FILE: &str = "workspace.json";
/// Cheap status sidecar written next to [`WORKSPACE_MARKER_FILE`] after a working-view
/// scan/rescan. It lets a shell statusline render file counts + scan age WITHOUT opening the Fjall
/// index — which would force a full index recovery (heavy log spam, slow) on every ~5s refresh.
/// Best-effort and non-load-bearing, exactly like `workspace.json`: a missing/corrupt sidecar just
/// degrades the statusline to a "no index" hint. See [`write_status_sidecar`] / [`read_status_sidecar`].
pub const STATUS_SIDECAR_FILE: &str = "status.json";
/// Schema version of [`StatusSidecar`]. Bump on any incompatible field change; [`read_status_sidecar`]
/// ignores a sidecar whose `schema_ver` it does not recognize (forward-compat, like a schema wipe).
pub const STATUS_SIDECAR_SCHEMA_VER: u32 = 1;
/// File holding the generated-and-persisted per-workspace agent id, in the workspace's cache dir
/// beside [`WORKSPACE_MARKER_FILE`]. Named here, with the rest of the layout, because the store's
/// orphan reaper ([`crate::store_gc_workspace`]) must recognize it; the identity resolver in
/// `crate::comms::identity` reads and writes it.
pub const AGENT_ID_FILE: &str = "agent-id";
pub const VIEWS_DIR: &str = "views";
/// Lazy-opened LanceDB store directory under `.basemind/`. Created on first use.
#[cfg(feature = "intelligence")]
pub const LANCE_DIR: &str = "lance";
/// The legacy per-view location of the document retrieval stores (`views/<view>/retrieval/`). The
/// stores are the repository group's now ([`GROUP_LANCE_DIR`]); the first scan that opens the
/// group store removes this directory (`scanner_docs_store::open_doc_store`).
#[cfg(feature = "documents")]
pub const RETRIEVAL_DIR: &str = "retrieval";

/// View name used for the working-tree index. Also the default for `basemind serve`.
pub const VIEW_WORKING: &str = "working";
/// View name used when scanning the staging index.
pub const VIEW_STAGED: &str = "staged";

/// Build the view name used for an arbitrary rev. Slash-free so it's a single directory.
pub fn view_name_for_rev(short_sha: &str) -> String {
    format!("rev-{short_sha}")
}

/// Stable per-workspace key: a hex blake3 hash of the **canonicalized** worktree-root path. One
/// key per worktree root (linked git worktrees canonicalize to distinct paths and so get distinct
/// keys — correct, since the global blob store dedups byte-identical content across them anyway).
///
/// Canonicalization resolves symlinks so `/tmp/x` and `/private/tmp/x` (macOS) map to one key;
/// a path that cannot be canonicalized (does not exist yet) falls back to its raw form so a
/// freshly-created root still hashes deterministically.
pub fn workspace_key(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    hashing::hex(&hashing::hash_bytes(canonical.as_os_str().as_encoded_bytes()))
}

/// Per-workspace cache directory for `root`: `<data home>/cache/workspaces/<workspace_key>/`.
/// Holds `views/<view>/`, the top-level `index.msgpack` (legacy), the LanceDB store, and the
/// per-workspace `.lock`. Blobs are NOT here — they live in the global [`global_blobs_dir`].
pub fn workspace_cache_dir(root: &Path) -> PathBuf {
    DataRoot::current().workspace_cache_dir(root)
}

/// Sub-directory of the cache holding per-repo-group state shared by every worktree of one clone,
/// keyed by [`repo_group_key`]. Today that is the user-authored memory store; nothing under it is
/// rebuildable, so no garbage collector may list it.
pub const REPOS_DIR: &str = "repos";
/// Fjall database under a repo-group dir holding `memory_by_key`.
pub const MEMORY_STORE_DIR: &str = "memory.fjall";

/// Group key for the worktree at `root`: the blake3 of the canonical main worktree root (the parent
/// of the shared git common dir), so every linked worktree of one clone maps to the same group. A
/// root outside any git repository (or in a bare one) falls back to its own [`workspace_key`].
pub fn repo_group_key(root: &Path) -> String {
    match crate::git::Repo::discover(root) {
        Ok(repo) => {
            let main = repo.main_worktree_root();
            let canonical = main.canonicalize().unwrap_or(main);
            hashing::hex(&hashing::hash_bytes(canonical.as_os_str().as_encoded_bytes()))
        }
        Err(_) => workspace_key(root),
    }
}

/// Per-repo-group directory for `root`: `<data home>/cache/repos/<repo_group_key>/`.
pub fn repo_group_dir(root: &Path) -> PathBuf {
    DataRoot::current().repo_group_dir(root)
}

/// Sub-directory of the cache holding the derived state of each repository group (ADR-0028), keyed
/// by [`repo_group_key`]: the group index and its lock. Rebuildable, unlike [`REPOS_DIR`].
pub const GROUPS_DIR: &str = "groups";

/// `<data home>/cache/groups/<repo_group_key>/`: the group index of `root`'s repository group.
pub fn group_index_dir(root: &Path) -> PathBuf {
    DataRoot::current().group_index_dir(root)
}

/// Directory of a repository group's document retrieval stores, under its [`GROUPS_DIR`] entry:
/// `cache/groups/<group>/lance/`, one store per embedding model (`crate::lance::docs`).
pub const GROUP_LANCE_DIR: &str = "lance";

/// The group key the workspace cache dir `basemind_dir` belongs to: the group its marker records,
/// else the group of its root while the root exists. `None` when neither names a group, or the
/// recorded group is not a single path component.
pub fn group_of_workspace(basemind_dir: &Path) -> Option<String> {
    let marker = read_workspace_marker(basemind_dir)?;
    let group = match marker.group {
        Some(group) => group,
        None => marker.root.exists().then(|| repo_group_key(&marker.root))?,
    };
    let single = !group.is_empty() && group != "." && group != ".." && !group.contains(['/', '\\']);
    single.then_some(group)
}

/// The group directory (`cache/groups/<group>/`) of the workspace cache dir `basemind_dir`
/// (`cache/workspaces/<key>`), by [`group_of_workspace`].
pub fn group_dir_of(basemind_dir: &Path) -> Option<PathBuf> {
    let group = group_of_workspace(basemind_dir)?;
    let cache = basemind_dir.parent()?.parent()?;
    Some(cache.join(GROUPS_DIR).join(group))
}

/// The group retrieval stores ([`GROUP_LANCE_DIR`]) of the workspace cache dir `basemind_dir`, under
/// its [`group_dir_of`].
pub fn group_lance_dir_of(basemind_dir: &Path) -> Option<PathBuf> {
    group_dir_of(basemind_dir).map(|group| group.join(GROUP_LANCE_DIR))
}

/// The group's user-memory Fjall database: `<repo_group_dir>/memory.fjall`. Outlives every
/// workspace in the group, so budget eviction and the orphan reaper never touch it.
pub fn memory_store_dir(root: &Path) -> PathBuf {
    DataRoot::current().memory_store_dir(root)
}

/// [`memory_store_dir`] of the group keyed `group` ([`repo_group_key`]), for a root that is gone.
pub fn group_memory_store_dir(group: &str) -> PathBuf {
    DataRoot::current().group_memory_store_dir(group)
}

/// Directory, beside [`MEMORY_STORE_DIR`], that holds the vector stores of the memory notes: one
/// subdirectory per embedding model and dimension.
pub const MEMORY_VECTORS_DIR: &str = "memory.lance";

/// `<repo_group_dir>/memory.lance/`: the memory notes' vector stores of every embedding model.
pub fn memory_vectors_dir(root: &Path) -> PathBuf {
    DataRoot::current().memory_vectors_dir(root)
}

/// The layout of the blob store, recorded in every file map ([`crate::store::Index::blob_layout`]).
/// `0`: blobs flat in `cache/blobs/`. `1`: one namespace directory per blob schema. A file map of
/// an earlier layout names blobs that are not where this binary looks, so it is discarded and the
/// workspace scanned again, without waiting for a release to change the schema as well.
pub const BLOB_LAYOUT: u8 = 1;

/// Root of the global blob store: `<data home>/cache/blobs/`. Holds one namespace directory per
/// blob schema; only [`global_blobs_dir`] is read or written by this binary.
pub fn global_blobs_root() -> PathBuf {
    DataRoot::current().global_blobs_root()
}

/// The GLOBAL content-addressed blob store: `<data home>/cache/blobs/v<SCHEMA_VER>/`. Shared
/// across every workspace on the machine, so byte-identical files are extracted + embedded exactly
/// once.
///
/// Namespaced by blob schema because two installed basemind minors share the cache: in one flat
/// directory each would overwrite the other's blobs as "not current", and the other side's file map
/// would go on calling those files unchanged. Other namespaces are never read, written or deleted
/// here; [`crate::store_gc`] reaps the superseded ones.
pub fn global_blobs_dir() -> PathBuf {
    DataRoot::current().global_blobs_dir()
}

/// The `workspace.json` sidecar: the canonical worktree root a workspace cache dir was keyed from.
/// See [`WORKSPACE_MARKER_FILE`] for why it exists (the dir name is a one-way hash, so an orphan is
/// otherwise undetectable — and an undetectable orphan pins global blobs forever).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceMarker {
    /// The canonicalized worktree root this cache dir belongs to. `exists()` on it is the single
    /// liveness test the orphan reaper runs.
    pub root: PathBuf,
    /// Unix-epoch seconds when the marker was (re)written. Diagnostics only.
    pub updated_unix: i64,
    /// The root's repository group ([`repo_group_key`]), recorded while the root exists: a collector
    /// that finds the root gone can no longer resolve it, and a linked worktree's group is not the
    /// hash of its own root. `None` in a marker written before the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// Idempotently record `root` in `basemind_dir/workspace.json`.
///
/// A no-op when the marker already names the same canonical root and the group that root resolves
/// to now, so the frequent read-only opens don't rewrite it on every MCP call. The group is
/// compared, not only recorded once: it moves when the main clone moves or git discovery starts or
/// stops failing, and the group reaper deletes any group no marker names. Best-effort: an I/O
/// failure (or a root path that is not valid UTF-8, which JSON cannot encode) leaves the dir
/// unverifiable, which the reaper treats as "keep" — never as "delete". Errors are swallowed
/// deliberately, mirroring the `.lock.meta` writer.
pub fn ensure_workspace_marker(basemind_dir: &Path, root: &Path) {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let group = repo_group_key(&canonical);
    if let Some(existing) = read_workspace_marker(basemind_dir)
        && existing.root == canonical
        && existing.group.as_deref() == Some(group.as_str())
    {
        return;
    }
    let updated_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let marker = WorkspaceMarker {
        root: canonical,
        updated_unix,
        group: Some(group),
    };
    let Ok(bytes) = serde_json::to_vec(&marker) else {
        return;
    };
    let _ = write_bytes_atomic(basemind_dir.join(WORKSPACE_MARKER_FILE), &bytes);
}

/// Read the `workspace.json` marker. `None` when it is absent or unparsable — the caller must then
/// treat the workspace as *unverifiable* (never as orphaned).
pub fn read_workspace_marker(basemind_dir: &Path) -> Option<WorkspaceMarker> {
    let bytes = std::fs::read(basemind_dir.join(WORKSPACE_MARKER_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The cheap `status.json` sidecar: a snapshot of a working view's size (file + blob counts) and
/// when it was last scanned. Written after every working-view scan/rescan so a shell statusline can
/// render the rich per-repo line by reading one tiny JSON file instead of opening the Fjall index.
///
/// Deliberately minimal and best-effort: it mirrors `workspace.json`'s "additive, non-load-bearing"
/// contract, never gates a scan, and is safe to be absent (the reader degrades to a "no index" hint).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusSidecar {
    /// Sidecar schema version; see [`STATUS_SIDECAR_SCHEMA_VER`].
    pub schema_ver: u32,
    /// Number of code files in the working view's index (`Index::files.len()` — the same population
    /// the MCP `status` tool reports as `file_count`).
    pub file_count: usize,
    /// Number of distinct file contents the working view's file map holds, one filemap blob each.
    /// Unlike the MCP `status` tool's `blob_count`, it counts this workspace's blobs only.
    pub blob_count: usize,
    /// Unix-epoch seconds when the sidecar was written (i.e. when the scan that produced it finished).
    pub scanned_unix: i64,
    /// False from the moment the code map is flushed until the resolve pass has committed every
    /// edge. A scan that dies or aborts in between leaves it false, which readers report as
    /// degraded. Absent in a pre-marker sidecar, which reads as complete.
    #[serde(default = "default_true")]
    pub resolve_complete: bool,
    /// True when the last full scan was cancelled or aborted before covering the workspace, so the
    /// file map holds only the files completed so far. Absent reads as false.
    #[serde(default)]
    pub partial: bool,
    /// Why the scan stopped short; set together with [`Self::partial`].
    #[serde(default)]
    pub partial_reason: Option<String>,
    /// Files the partial scan had completed.
    #[serde(default)]
    pub progress_done: Option<usize>,
    /// Candidate files the partial scan had enumerated.
    #[serde(default)]
    pub progress_total: Option<usize>,
}

fn default_true() -> bool {
    true
}

/// Why a full scan stopped before covering the workspace, and how far it got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialScan {
    pub reason: String,
    pub done: usize,
    pub total: usize,
}

/// The completeness half of the sidecar: what a reader needs to tell a finished index from a
/// degraded one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanHealth {
    pub resolve_complete: bool,
    pub partial: Option<PartialScan>,
}

impl ScanHealth {
    /// A finished scan whose resolve pass committed (or has nothing to resolve).
    pub fn complete() -> Self {
        Self {
            resolve_complete: true,
            partial: None,
        }
    }

    /// The code map is flushed but the resolve pass has not finished yet.
    pub fn resolving() -> Self {
        Self {
            resolve_complete: false,
            partial: None,
        }
    }

    /// The scan stopped short: the resolve pass did not run either.
    pub fn partial(reason: impl Into<String>, done: usize, total: usize) -> Self {
        Self {
            resolve_complete: false,
            partial: Some(PartialScan {
                reason: reason.into(),
                done,
                total,
            }),
        }
    }
}

impl Default for ScanHealth {
    fn default() -> Self {
        Self::complete()
    }
}

impl StatusSidecar {
    /// The health recorded in this sidecar.
    pub fn health(&self) -> ScanHealth {
        ScanHealth {
            resolve_complete: self.resolve_complete,
            partial: self.partial.then(|| PartialScan {
                reason: self.partial_reason.clone().unwrap_or_default(),
                done: self.progress_done.unwrap_or(0),
                total: self.progress_total.unwrap_or(0),
            }),
        }
    }

    /// Whether the index is anything less than a finished scan with a committed resolve pass.
    pub fn is_degraded(&self) -> bool {
        self.partial || !self.resolve_complete
    }

    /// One-line operator description of the degradation, or `None` for a healthy index.
    pub fn degraded_summary(&self) -> Option<String> {
        if self.partial {
            let reason = self.partial_reason.as_deref().unwrap_or("unknown");
            let (done, total) = (self.progress_done.unwrap_or(0), self.progress_total.unwrap_or(0));
            return Some(format!(
                "partial scan ({reason}; {done}/{total} files); a full rescan is needed"
            ));
        }
        (!self.resolve_complete)
            .then(|| "resolve pass incomplete; resolved references and callers may be stale".to_string())
    }
}

/// Path of the `status.json` sidecar inside a workspace cache dir.
pub fn status_sidecar_path(basemind_dir: &Path) -> PathBuf {
    basemind_dir.join(STATUS_SIDECAR_FILE)
}

/// Count content-addressed filemap blobs (`*.fm.msgpack`) in `blobs_dir` — the same population the
/// MCP `status` tool reports as `blob_count`. Returns `0` when the directory is absent or unreadable;
/// the count is advisory, never load-bearing.
pub fn count_fm_blobs(blobs_dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(blobs_dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".fm.msgpack")))
        .count()
}

/// Best-effort atomic write of the `status.json` sidecar into `basemind_dir`. `scanned_unix` is set
/// to now. Errors are swallowed deliberately, mirroring [`ensure_workspace_marker`] — a failed
/// sidecar write must never fail a scan, and a stale/absent sidecar only degrades the statusline.
pub fn write_status_sidecar(basemind_dir: &Path, file_count: usize, blob_count: usize, health: &ScanHealth) {
    let scanned_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let sidecar = StatusSidecar {
        schema_ver: STATUS_SIDECAR_SCHEMA_VER,
        file_count,
        blob_count,
        scanned_unix,
        resolve_complete: health.resolve_complete,
        partial: health.partial.is_some(),
        partial_reason: health.partial.as_ref().map(|p| p.reason.clone()),
        progress_done: health.partial.as_ref().map(|p| p.done),
        progress_total: health.partial.as_ref().map(|p| p.total),
    };
    let Ok(bytes) = serde_json::to_vec(&sidecar) else {
        return;
    };
    let _ = write_bytes_atomic(status_sidecar_path(basemind_dir), &bytes);
}

/// Read the `status.json` sidecar. `None` when it is absent, unparsable, or carries an unrecognized
/// `schema_ver` — the caller must then treat the workspace as unscanned (render the "no index" hint).
pub fn read_status_sidecar(basemind_dir: &Path) -> Option<StatusSidecar> {
    let bytes = std::fs::read(status_sidecar_path(basemind_dir)).ok()?;
    let sidecar: StatusSidecar = serde_json::from_slice(&bytes).ok()?;
    (sidecar.schema_ver == STATUS_SIDECAR_SCHEMA_VER).then_some(sidecar)
}

/// Replace only the health fields of the sidecar, keeping its counts and timestamp. With no usable
/// sidecar there is nothing to keep, so a zero-count one is written: the point is that a reader sees
/// the degradation. Best-effort, like [`write_status_sidecar`].
pub fn update_status_health(basemind_dir: &Path, health: &ScanHealth) {
    let (file_count, blob_count) = read_status_sidecar(basemind_dir).map_or((0, 0), |s| (s.file_count, s.blob_count));
    write_status_sidecar(basemind_dir, file_count, blob_count, health);
}

/// Redirect the data home at a per-process temp dir for the whole test binary.
///
/// Marker file stamped into a cache directory created by [`init_isolated_cache`]. Its presence is
/// the only evidence that an inherited `$BASEMIND_DATA_HOME` is a throwaway test cache rather than a
/// developer's real one, which is what makes adopting it safe.
#[cfg(any(feature = "test-support", test))]
const ISOLATION_MARKER: &str = ".basemind-isolated-test-cache";

/// An inherited `$BASEMIND_DATA_HOME` that [`init_isolated_cache`] created in an ancestor process.
#[cfg(any(feature = "test-support", test))]
fn inherited_isolated_root() -> Option<PathBuf> {
    marked_isolated_root(PathBuf::from(std::env::var_os(DATA_HOME_ENV)?))
}

/// `path` when it carries [`ISOLATION_MARKER`], i.e. was minted by [`init_isolated_cache`]. Takes the
/// candidate as a value so tests can exercise the check without mutating the process-global env.
#[cfg(any(feature = "test-support", test))]
fn marked_isolated_root(path: PathBuf) -> Option<PathBuf> {
    path.join(ISOLATION_MARKER).is_file().then_some(path)
}

/// Name prefix of a cache dir created by [`init_isolated_cache`], so the stale sweep only stats
/// directories it could have created.
#[cfg(any(feature = "test-support", test))]
const ISOLATED_CACHE_PREFIX: &str = "basemind-test-";

/// The `tempfile` default prefix, which isolated caches carried before they were named.
#[cfg(any(feature = "test-support", test))]
const LEGACY_ISOLATED_CACHE_PREFIX: &str = ".tmp";

/// A marked cache this old belongs to a test process that was killed before its exit handler ran.
#[cfg(any(feature = "test-support", test))]
const STALE_ISOLATED_CACHE_AGE: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// Removals per sweep, so a backlog costs a test binary's start-up a bounded amount of work.
#[cfg(any(feature = "test-support", test))]
const MAX_STALE_REMOVALS: usize = 64;

/// The isolated cache this process created (never one it adopted), removed at process exit.
#[cfg(any(feature = "test-support", test))]
static CREATED_ISOLATED_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// `atexit` handler: best-effort removal of [`CREATED_ISOLATED_ROOT`]. A daemon still holding files
/// there (or Windows refusing to delete an open file) only leaves debris for the stale sweep.
#[cfg(any(feature = "test-support", test))]
extern "C" fn remove_created_isolated_cache() {
    if let Some(root) = CREATED_ISOLATED_ROOT.get() {
        // Unwinding out of an `extern "C"` fn aborts the exiting process. ~keep
        let _ = std::panic::catch_unwind(|| std::fs::remove_dir_all(root));
    }
}

/// Remove marked isolated caches directly under `temp_root` whose marker is at least `max_age`
/// old, returning how many were removed. Only entries named with [`ISOLATED_CACHE_PREFIX`] or
/// [`LEGACY_ISOLATED_CACHE_PREFIX`] are inspected, nothing is descended into, and a directory
/// without [`ISOLATION_MARKER`] is never touched. Errors are ignored.
#[cfg(any(feature = "test-support", test))]
fn sweep_stale_isolated_caches(temp_root: &Path, max_age: std::time::Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(temp_root) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        if removed >= MAX_STALE_REMOVALS {
            break;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with(ISOLATED_CACHE_PREFIX) || name.starts_with(LEGACY_ISOLATED_CACHE_PREFIX)) {
            continue;
        }
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let dir = entry.path();
        let Ok(marker) = std::fs::symlink_metadata(dir.join(ISOLATION_MARKER)) else {
            continue;
        };
        let stale = marker.is_file()
            && marker
                .modified()
                .ok()
                .and_then(|written| now.duration_since(written).ok())
                .is_some_and(|age| age >= max_age);
        if stale && std::fs::remove_dir_all(&dir).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Sets `$BASEMIND_DATA_HOME` exactly once (via [`std::sync::Once`]) to a temp dir that outlives
/// every test in the binary, and is idempotent across the many fixture constructors that call it.
/// The process that created the dir removes it at exit; ones left by killed processes are swept
/// when the next one is created. Workspace-keying + content-addressed blobs keep tests mutually
/// isolated even though they share this one cache root, so all tests in a binary can safely share
/// it — no per-test env churn, no races on `set_var`.
///
/// Also pins `$BASEMIND_COMMS_DIR` under the same tempdir. On a `comms` build the real `basemind
/// serve` binary relays to the machine daemon (auto-spawned on first use); a test that spawns `serve` inherits this env, so its daemon binds an ISOLATED
/// socket under the tempdir instead of touching the user's real machine daemon.
///
/// A cache created here is stamped with [`ISOLATION_MARKER`] and re-adopted by any child process,
/// so the machine-wide daemon ceiling counts one registry across the whole tree.
#[cfg(any(feature = "test-support", test))]
pub fn init_isolated_cache() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // A data home this helper itself created (identified by its marker file) is ADOPTED rather
        // than replaced. The machine-wide daemon registry and its ceiling live under this directory,
        // so minting a fresh one in a child process would hand that child an empty registry — every
        // generation counting 0 live daemons and being allowed to spawn another. That is what let a
        // single stray re-exec grow to thousands of daemons instead of being refused at the 8th.
        // Sharing it across the process tree is the only way the ceiling can bind.
        //
        // The marker is what makes adoption safe: an unmarked `$BASEMIND_DATA_HOME` is a developer's
        // REAL cache, and a test must never be pointed at that. ~keep
        let root: PathBuf = match inherited_isolated_root() {
            Some(path) => path,
            None => {
                let temp_root = std::env::temp_dir();
                sweep_stale_isolated_caches(&temp_root, STALE_ISOLATED_CACHE_AGE);
                let dir = tempfile::Builder::new()
                    .prefix(ISOLATED_CACHE_PREFIX)
                    .tempdir_in(&temp_root)
                    .expect("create isolated cache tempdir")
                    .keep();
                std::fs::write(dir.join(ISOLATION_MARKER), b"basemind test cache\n")
                    .expect("write the isolated-cache marker");
                if CREATED_ISOLATED_ROOT.set(dir.clone()).is_ok() {
                    // SAFETY: registers a plain `extern "C" fn()` with no captured state; it reads
                    // only the `OnceLock` set just above. ~keep
                    unsafe {
                        libc::atexit(remove_created_isolated_cache);
                    }
                }
                dir
            }
        };
        let comms_dir = root.join("comms");
        crate::config::engine::set_data_root(crate::data_root::DataRoot::new(root.clone()));
        // SAFETY: set exactly once, inside `Once::call_once`, before any test thread reads
        unsafe {
            std::env::set_var(DATA_HOME_ENV, root);
            // A literal key, not the `comms::singleton::COMMS_DIR_ENV` constant: that module is
            // feature-gated, this helper is not. ~keep
            std::env::set_var("BASEMIND_COMMS_DIR", comms_dir);
            // A test binary spawns its own isolated daemons (per the dirs above). Pin a fast idle
            // reap so they self-terminate within seconds of the suite going quiet instead of lingering
            // the shipped window — otherwise a parallel `cargo test` accumulates one daemon per binary,
            // each with tens of threads, which is what exhausted the process table. Only set when unset
            // still wins. Well above any inter-RPC gap these suites have, so no daemon reaps mid-test.
            for (key, value) in [
                ("BASEMIND_COMMS_IDLE_REAP_SECS", "20"),
                ("BASEMIND_COMMS_IDLE_CHECK_SECS", "3"),
                ("BASEMIND_AGENT_BOOTSTRAP_SECS", "20"),
                ("BASEMIND_AGENT_IDLE_REAP_SECS", "20"),
                ("BASEMIND_AGENT_IDLE_CHECK_SECS", "3"),
            ] {
                if std::env::var_os(key).is_none() {
                    std::env::set_var(key, value);
                }
            }
        }
    });
}

#[cfg(test)]
mod daemon_isolation_tests {
    use super::*;

    /// A test that never calls [`init_isolated_cache`] still gets the isolated cache: the first read
    /// of the root initialises it, so one test binary never sees two data homes, and no test writes
    /// into the developer's real cache. Run alone (`-- --exact`) to see it hold without any other
    /// test having initialised the cache first.
    #[test]
    fn the_first_read_of_the_cache_root_isolates_it() {
        let root = DataRoot::current().path().to_path_buf();

        assert!(
            root.join(ISOLATION_MARKER).is_file(),
            "{} is not an isolated test cache",
            root.display()
        );
        assert_eq!(std::env::var_os(DATA_HOME_ENV).map(PathBuf::from), Some(root));
    }

    /// Every daemon family basemind can auto-spawn must resolve inside the per-process temp cache.
    /// A family that escapes leaks a daemon past the suite, and a leaked daemon holds an exclusive
    /// Fjall directory lock that blocks the next `basemind scan`.
    #[test]
    fn every_daemon_family_resolves_inside_the_temp_cache() {
        init_isolated_cache();
        let temp_root = PathBuf::from(std::env::var_os(DATA_HOME_ENV).expect("BASEMIND_DATA_HOME is set"));

        assert_eq!(
            DataRoot::current().path(),
            temp_root,
            "the data home must follow the isolation seam"
        );
        if let Some(dirs) = directories::ProjectDirs::from("", "", "basemind") {
            assert!(
                !temp_root.starts_with(dirs.data_dir()),
                "the isolated cache must not sit inside the real data dir {}",
                dirs.data_dir().display()
            );
        }

        let comms_dir = PathBuf::from(std::env::var_os("BASEMIND_COMMS_DIR").expect("BASEMIND_COMMS_DIR is set"));
        assert!(
            comms_dir.starts_with(&temp_root),
            "comms daemon escaped isolation: {}",
            comms_dir.display()
        );

        // agent-ipc has no socket env var of its own: it derives `<data home>/agent/<key>.sock` and
        // `<data home>/agent/sessions/`, so the data-home assertion above is its path isolation.
        // Only its lifetime can still escape, hence the pinned reap windows. ~keep
        for key in [
            "BASEMIND_AGENT_BOOTSTRAP_SECS",
            "BASEMIND_AGENT_IDLE_REAP_SECS",
            "BASEMIND_AGENT_IDLE_CHECK_SECS",
        ] {
            let raw = std::env::var(key).unwrap_or_else(|_| panic!("{key} is set"));
            let seconds: u64 = raw.parse().unwrap_or_else(|_| panic!("{key} is numeric, got {raw:?}"));
            assert!(
                (1..=60).contains(&seconds),
                "{key} must stay far below the shipped window so a test daemon self-reaps, got {seconds}"
            );
        }
    }

    /// The isolated cache must carry its marker, or a child process cannot tell it apart from a
    /// developer's real `$BASEMIND_DATA_HOME` and will mint a fresh one — which is what detaches the
    /// daemon ceiling from the process tree it is supposed to bound.
    #[test]
    fn the_isolated_cache_is_marked_and_is_adopted_by_a_child() {
        init_isolated_cache();
        let root = PathBuf::from(std::env::var_os(DATA_HOME_ENV).expect("BASEMIND_DATA_HOME is set"));
        assert!(
            root.join(ISOLATION_MARKER).is_file(),
            "the isolated cache must be marked so a child adopts it instead of minting its own"
        );
        assert_eq!(
            inherited_isolated_root().as_deref(),
            Some(root.as_path()),
            "a child inheriting this env must resolve the SAME cache, or the daemon ceiling counts \
             an empty registry in every generation"
        );
    }

    /// An unmarked directory is a developer's real cache. Adopting it would point the suite at live
    /// data and at the user's live daemons, so the marker check must reject it.
    #[test]
    fn an_unmarked_inherited_data_home_is_never_adopted() {
        let real = tempfile::tempdir().expect("tempdir");
        // Pure on purpose: pointing the process-global `BASEMIND_DATA_HOME` at an unmarked directory
        // here raced every concurrently running test that reads the isolated cache.
        let resolved = marked_isolated_root(real.path().to_path_buf());
        assert_eq!(
            resolved, None,
            "an unmarked data home is the developer's real cache and must not be adopted"
        );
    }

    /// A killed test process never runs its exit handler, so its cache is reclaimed by the next
    /// one's sweep: stale marked dirs (new and legacy names) go, while an unmarked dir and a fresh
    /// marked dir stay.
    #[test]
    fn the_sweep_removes_only_stale_marked_caches() {
        let temp_root = tempfile::tempdir().expect("tempdir");
        let age = STALE_ISOLATED_CACHE_AGE;
        let long_ago = std::time::SystemTime::now() - age - std::time::Duration::from_secs(60);
        let make = |name: &str, marker_written: Option<std::time::SystemTime>| {
            let dir = temp_root.path().join(name);
            std::fs::create_dir_all(dir.join("cache")).expect("create candidate");
            if let Some(written) = marker_written {
                let marker = dir.join(ISOLATION_MARKER);
                std::fs::write(&marker, b"basemind test cache\n").expect("write marker");
                std::fs::File::options()
                    .write(true)
                    .open(&marker)
                    .and_then(|file| file.set_modified(written))
                    .expect("backdate marker");
            }
            dir
        };
        let stale = make("basemind-test-stale", Some(long_ago));
        let legacy = make(".tmpLegacy", Some(long_ago));
        let unmarked = make("basemind-test-unmarked", None);
        let fresh = make("basemind-test-fresh", Some(std::time::SystemTime::now()));

        let removed = sweep_stale_isolated_caches(temp_root.path(), age);

        assert_eq!(removed, 2, "exactly the stale and the legacy marked caches are removed");
        assert!(!stale.exists(), "a stale marked cache must be removed");
        assert!(!legacy.exists(), "a stale legacy `.tmp*` marked cache must be removed");
        assert!(unmarked.exists(), "an unmarked dir must never be touched");
        assert!(
            fresh.exists(),
            "a fresh marked cache belongs to a live process and must stay"
        );
    }
}

#[cfg(test)]
mod workspace_marker_tests {
    use super::*;

    /// A marker whose recorded group is no longer the root's group (the main clone moved, or git
    /// discovery failed when the marker was written) is rewritten: the group reaper trusts it, and a
    /// stale one gets the group in use deleted.
    #[test]
    fn a_marker_naming_another_group_is_rewritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).expect("mk root");
        let canonical = root.canonicalize().expect("canonical root");
        let stale = WorkspaceMarker {
            root: canonical.clone(),
            updated_unix: 0,
            group: Some("old-group".to_string()),
        };
        std::fs::write(
            dir.path().join(WORKSPACE_MARKER_FILE),
            serde_json::to_vec(&stale).expect("encode marker"),
        )
        .expect("write marker");

        ensure_workspace_marker(dir.path(), &root);

        let marker = read_workspace_marker(dir.path()).expect("marker");
        assert_eq!(marker.root, canonical);
        assert_eq!(marker.group, Some(repo_group_key(&canonical)));
    }
}

#[cfg(test)]
mod status_sidecar_tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_status_sidecar(dir.path(), 42, 7, &ScanHealth::complete());
        let sidecar = read_status_sidecar(dir.path()).expect("sidecar present after write");
        assert_eq!(sidecar.schema_ver, STATUS_SIDECAR_SCHEMA_VER);
        assert_eq!(sidecar.file_count, 42);
        assert_eq!(sidecar.blob_count, 7);
        assert!(
            sidecar.scanned_unix > 0,
            "scanned_unix should be a real epoch, got {}",
            sidecar.scanned_unix
        );
    }

    #[test]
    fn a_pre_marker_sidecar_reads_as_complete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy =
            format!(r#"{{"schema_ver":{STATUS_SIDECAR_SCHEMA_VER},"file_count":3,"blob_count":3,"scanned_unix":9}}"#);
        std::fs::write(status_sidecar_path(dir.path()), legacy).expect("write");
        let sidecar = read_status_sidecar(dir.path()).expect("legacy sidecar still parses");
        assert!(!sidecar.is_degraded());
        assert_eq!(sidecar.degraded_summary(), None);
    }

    #[test]
    fn health_round_trips_and_summarizes() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_status_sidecar(dir.path(), 5, 5, &ScanHealth::partial("cancelled", 2, 5));
        let sidecar = read_status_sidecar(dir.path()).expect("sidecar");
        assert!(sidecar.is_degraded());
        assert_eq!(sidecar.health(), ScanHealth::partial("cancelled", 2, 5));
        let summary = sidecar.degraded_summary().expect("summary");
        assert!(summary.contains("cancelled") && summary.contains("2/5"), "{summary}");

        write_status_sidecar(dir.path(), 5, 5, &ScanHealth::resolving());
        let sidecar = read_status_sidecar(dir.path()).expect("sidecar");
        assert!(sidecar.is_degraded() && !sidecar.partial);
    }

    #[test]
    fn update_health_keeps_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_status_sidecar(dir.path(), 42, 7, &ScanHealth::complete());
        update_status_health(dir.path(), &ScanHealth::partial("disk full", 1, 9));
        let sidecar = read_status_sidecar(dir.path()).expect("sidecar");
        assert_eq!((sidecar.file_count, sidecar.blob_count), (42, 7));
        assert!(sidecar.partial);
    }

    #[test]
    fn read_is_none_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_status_sidecar(dir.path()), None);
    }

    #[test]
    fn read_rejects_unrecognized_schema_ver() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bytes = serde_json::to_vec(&StatusSidecar {
            schema_ver: STATUS_SIDECAR_SCHEMA_VER + 1,
            file_count: 1,
            blob_count: 1,
            scanned_unix: 1,
            resolve_complete: true,
            partial: false,
            partial_reason: None,
            progress_done: None,
            progress_total: None,
        })
        .expect("serialize");
        std::fs::write(status_sidecar_path(dir.path()), bytes).expect("write");
        assert_eq!(
            read_status_sidecar(dir.path()),
            None,
            "a future schema_ver must be ignored, not misread"
        );
    }

    #[test]
    fn count_fm_blobs_counts_only_fm_msgpack() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in [
            "a.fm.msgpack",
            "b.fm.msgpack",
            "c.doc.msgpack",
            "d.txt",
            "e.fm.msgpack.tmp",
        ] {
            std::fs::write(dir.path().join(name), b"x").expect("write blob");
        }
        assert_eq!(count_fm_blobs(dir.path()), 2, "only *.fm.msgpack files count");
    }

    #[test]
    fn count_fm_blobs_is_zero_for_missing_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(count_fm_blobs(&dir.path().join("does-not-exist")), 0);
    }
}

/// Make the entries of directory `path` durable: a rename or create inside it survives a crash
/// only once the directory itself is synced. A no-op where directories cannot be synced.
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    return std::fs::File::open(path)?.sync_all();
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}
