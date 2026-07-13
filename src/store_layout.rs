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

use crate::hashing;
use crate::store_blob::write_bytes_atomic;

pub const INDEX_FILE: &str = "index.msgpack";
pub const BLOBS_DIR: &str = "blobs";
pub const LOCK_FILE: &str = ".lock";
/// Environment override for the global cache root. When set, [`cache_root`] returns it verbatim
/// instead of the XDG data dir — the single seam the test-isolation helper uses to redirect every
/// workspace's cache into a per-process temp dir.
pub const DATA_HOME_ENV: &str = "BASEMIND_DATA_HOME";
/// Sub-directory of [`cache_root`] that holds all basemind cache state (`blobs/` + `workspaces/`).
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
pub const VIEWS_DIR: &str = "views";
/// Lazy-opened LanceDB store directory under `.basemind/`. Created on first use.
#[cfg(feature = "intelligence")]
pub const LANCE_DIR: &str = "lance";

/// View name used for the working-tree index. Also the default for `basemind serve`.
pub const VIEW_WORKING: &str = "working";
/// View name used when scanning the staging index.
pub const VIEW_STAGED: &str = "staged";

/// Build the view name used for an arbitrary rev. Slash-free so it's a single directory.
pub fn view_name_for_rev(short_sha: &str) -> String {
    format!("rev-{short_sha}")
}

/// Root of basemind's GLOBAL on-disk cache, shared across every workspace on the machine.
///
/// Resolution order:
/// 1. `$BASEMIND_DATA_HOME` when set (the test-isolation seam; also a user escape hatch).
/// 2. Else `directories::ProjectDirs::from("", "", "basemind").data_dir()` — the platform XDG
///    data dir (`~/.local/share/basemind` on Linux, `~/Library/Application Support/basemind` on
///    macOS, `%APPDATA%\basemind\data` on Windows).
/// 3. Else the current directory (only when `ProjectDirs` cannot resolve a home dir — no `HOME`).
///
/// The cache lives under `cache_root()/cache/`: a global `blobs/` (content-addressed, shared by
/// every workspace) plus per-workspace state under `workspaces/<workspace_key>/`.
pub fn cache_root() -> PathBuf {
    if let Some(explicit) = std::env::var_os(DATA_HOME_ENV) {
        return PathBuf::from(explicit);
    }
    directories::ProjectDirs::from("", "", "basemind")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
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

/// Per-workspace cache directory for `root`: `cache_root()/cache/workspaces/<workspace_key>/`.
/// Holds `views/<view>/`, the top-level `index.msgpack` (legacy), the LanceDB store, and the
/// per-workspace `.lock`. Blobs are NOT here — they live in the global [`global_blobs_dir`].
pub fn workspace_cache_dir(root: &Path) -> PathBuf {
    cache_root()
        .join(CACHE_DIR)
        .join(WORKSPACES_DIR)
        .join(workspace_key(root))
}

/// The GLOBAL content-addressed blob store: `cache_root()/cache/blobs/`. Shared across every
/// workspace on the machine, so byte-identical files are extracted + embedded exactly once.
pub fn global_blobs_dir() -> PathBuf {
    cache_root().join(CACHE_DIR).join(BLOBS_DIR)
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
}

/// Idempotently record `root` in `basemind_dir/workspace.json`.
///
/// A no-op when the marker already names the same canonical root, so the frequent read-only opens
/// don't rewrite it on every MCP call. Best-effort: an I/O failure (or a root path that is not valid
/// UTF-8, which JSON cannot encode) leaves the dir unverifiable, which the reaper treats as
/// "keep" — never as "delete". Errors are swallowed deliberately, mirroring the `.lock.meta` writer.
pub fn ensure_workspace_marker(basemind_dir: &Path, root: &Path) {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if let Some(existing) = read_workspace_marker(basemind_dir)
        && existing.root == canonical
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

/// Redirect [`cache_root`] at a per-process temp dir for the whole test binary.
///
/// Sets `$BASEMIND_DATA_HOME` exactly once (via [`std::sync::Once`]) to a leaked [`tempfile::TempDir`]
/// so it outlives every test in the binary, and is idempotent across the many fixture constructors
/// that call it. Workspace-keying + content-addressed blobs keep tests mutually isolated even
/// though they share this one cache root, so all tests in a binary can safely share it — no
/// per-test env churn, no races on `set_var`.
///
/// Also pins `$BASEMIND_COMMS_DIR` under the same tempdir. On a `comms` build the real `basemind
/// serve` binary is a `daemon_writer` that forwards every write to the machine daemon (auto-spawned
/// on first use); a test that spawns `serve` inherits this env, so its daemon binds an ISOLATED
/// socket under the tempdir instead of touching the user's real machine daemon.
#[cfg(any(feature = "test-support", test))]
pub fn init_isolated_cache() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Leak the TempDir so the directory lives for the entire process; the OS reclaims it on
        // exit. A dropped TempDir here would delete the cache out from under still-running tests.
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("create isolated cache tempdir")));
        let comms_dir = dir.path().join("comms");
        // SAFETY: set exactly once, inside `Once::call_once`, before any test thread reads
        // `cache_root()` (every fixture constructor calls this first). Rust 2024 marks `set_var`
        // unsafe because concurrent get/set is UB; the single-write-before-any-read discipline
        // here upholds that invariant. `BASEMIND_COMMS_DIR` is inert on non-comms builds.
        unsafe {
            std::env::set_var(DATA_HOME_ENV, dir.path());
            std::env::set_var("BASEMIND_COMMS_DIR", comms_dir);
        }
    });
}
