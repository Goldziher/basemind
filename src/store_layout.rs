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
/// Cheap status sidecar written next to [`WORKSPACE_MARKER_FILE`] after a working-view
/// scan/rescan. It lets a shell statusline render file counts + scan age WITHOUT opening the Fjall
/// index — which would force a full index recovery (heavy log spam, slow) on every ~5s refresh.
/// Best-effort and non-load-bearing, exactly like `workspace.json`: a missing/corrupt sidecar just
/// degrades the statusline to a "no index" hint. See [`write_status_sidecar`] / [`read_status_sidecar`].
pub const STATUS_SIDECAR_FILE: &str = "status.json";
/// Schema version of [`StatusSidecar`]. Bump on any incompatible field change; [`read_status_sidecar`]
/// ignores a sidecar whose `schema_ver` it does not recognize (forward-compat, like a schema wipe).
pub const STATUS_SIDECAR_SCHEMA_VER: u32 = 1;
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
    /// Number of content-addressed filemap blobs on disk (`*.fm.msgpack` in the global blob store),
    /// counted consistently with the MCP `status` tool's `blob_count`.
    pub blob_count: usize,
    /// Unix-epoch seconds when the sidecar was written (i.e. when the scan that produced it finished).
    pub scanned_unix: i64,
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
pub fn write_status_sidecar(basemind_dir: &Path, file_count: usize, blob_count: usize) {
    let scanned_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let sidecar = StatusSidecar {
        schema_ver: STATUS_SIDECAR_SCHEMA_VER,
        file_count,
        blob_count,
        scanned_unix,
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

/// Redirect [`cache_root`] at a per-process temp dir for the whole test binary.
///
/// Marker file stamped into a cache directory created by [`init_isolated_cache`]. Its presence is
/// the only evidence that an inherited `$BASEMIND_DATA_HOME` is a throwaway test cache rather than a
/// developer's real one, which is what makes adopting it safe.
#[cfg(any(feature = "test-support", test))]
const ISOLATION_MARKER: &str = ".basemind-isolated-test-cache";

/// An inherited `$BASEMIND_DATA_HOME` that [`init_isolated_cache`] created in an ancestor process.
#[cfg(any(feature = "test-support", test))]
fn inherited_isolated_root() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(DATA_HOME_ENV)?);
    path.join(ISOLATION_MARKER).is_file().then_some(path)
}

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
///
/// The same treatment covers every other daemon family basemind can auto-spawn, because a daemon
/// that survives the suite keeps an exclusive Fjall directory lock and blocks the next
/// `basemind scan`:
/// * **agent-ipc** (`basemind-agent-ipc`) already derives both its socket
///   (`cache_root()/agent/<key>.sock`) and its session store from [`cache_root`], so
///   `$BASEMIND_DATA_HOME` alone redirects it — but its shipped lifetime is a 2-minute bootstrap
///   window and a 10-minute idle window, so the reap knobs are pinned short here too.
/// * **shells/rmux** resolves its endpoint from `directories::ProjectDirs`, i.e. the user's REAL
///   data dir, unless `$BASEMIND_SHELLS_SOCKET` overrides it — so that override is pinned into the
///   tempdir, which is the only thing keeping a test off the developer's live rmux daemon.
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
        let root: &Path = match inherited_isolated_root() {
            Some(path) => Box::leak(Box::new(path)).as_path(),
            None => {
                let dir = Box::leak(Box::new(tempfile::tempdir().expect("create isolated cache tempdir")));
                std::fs::write(dir.path().join(ISOLATION_MARKER), b"basemind test cache\n")
                    .expect("write the isolated-cache marker");
                dir.path()
            }
        };
        let comms_dir = root.join("comms");
        let shells_endpoint = isolated_shells_endpoint(root);
        // SAFETY: set exactly once, inside `Once::call_once`, before any test thread reads
        unsafe {
            std::env::set_var(DATA_HOME_ENV, root);
            // Literal keys, not the `comms::singleton::COMMS_DIR_ENV` / `shells::SHELLS_SOCKET_ENV`
            // constants: both modules are feature-gated, this helper is not. ~keep
            std::env::set_var("BASEMIND_COMMS_DIR", comms_dir);
            std::env::set_var("BASEMIND_SHELLS_SOCKET", shells_endpoint);
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

/// Endpoint the embedded shells (rmux) daemon binds under an isolated cache: `<dir>/shells/rmux.sock`.
/// The parent is created eagerly because the daemon binds the socket without creating it, and a bind
/// failure would silently fall the runtime back to the user's real per-user socket.
#[cfg(all(any(feature = "test-support", test), not(windows)))]
fn isolated_shells_endpoint(dir: &Path) -> std::ffi::OsString {
    let shells_dir = dir.join("shells");
    let _ = std::fs::create_dir_all(&shells_dir);
    shells_dir.join("rmux.sock").into_os_string()
}

/// Windows has no filesystem socket to place under the tempdir — rmux binds a named pipe, and
/// `shells::daemon::validate_socket_path` accepts nothing but a `\\.\pipe\` name. Namespacing it by
/// pid is what isolates one test binary from another and from the user's live daemon.
#[cfg(all(any(feature = "test-support", test), windows))]
fn isolated_shells_endpoint(_dir: &Path) -> std::ffi::OsString {
    std::ffi::OsString::from(format!(r"\\.\pipe\basemind-shells-test-{}", std::process::id()))
}

#[cfg(test)]
mod daemon_isolation_tests {
    use super::*;

    /// Every daemon family basemind can auto-spawn must resolve inside the per-process temp cache.
    /// A family that escapes leaks a daemon past the suite, and a leaked daemon holds an exclusive
    /// Fjall directory lock that blocks the next `basemind scan`.
    #[test]
    fn every_daemon_family_resolves_inside_the_temp_cache() {
        init_isolated_cache();
        let temp_root = PathBuf::from(std::env::var_os(DATA_HOME_ENV).expect("BASEMIND_DATA_HOME is set"));

        assert_eq!(cache_root(), temp_root, "cache_root must follow the isolation seam");
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

        // agent-ipc has no socket env var of its own: it derives `cache_root()/agent/<key>.sock` and
        // `cache_root()/agent/sessions/`, so the `cache_root` assertion above is its path isolation.
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

    #[cfg(not(windows))]
    #[test]
    fn shells_socket_override_points_at_a_bindable_path_in_the_temp_cache() {
        init_isolated_cache();
        let temp_root = PathBuf::from(std::env::var_os(DATA_HOME_ENV).expect("BASEMIND_DATA_HOME is set"));
        let socket = PathBuf::from(std::env::var_os("BASEMIND_SHELLS_SOCKET").expect("BASEMIND_SHELLS_SOCKET is set"));

        assert!(
            socket.starts_with(&temp_root),
            "shells/rmux daemon escaped isolation: {}",
            socket.display()
        );
        assert!(
            socket.parent().is_some_and(Path::is_dir),
            "the socket parent must exist or the bind falls back to the real per-user socket"
        );
        // The platform `sockaddr_un` ceiling is ~104 bytes; a too-long tempdir path would make the
        // bind fail and silently hand the test the developer's live daemon. ~keep
        assert!(
            socket.as_os_str().len() < 104,
            "isolated socket path must stay bindable: {} ({} bytes)",
            socket.display(),
            socket.as_os_str().len()
        );
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
        let previous = std::env::var_os(DATA_HOME_ENV);
        // SAFETY: single-threaded within this test; the previous value is restored before returning.
        unsafe { std::env::set_var(DATA_HOME_ENV, real.path()) };
        let resolved = inherited_isolated_root();
        match previous {
            Some(value) => unsafe { std::env::set_var(DATA_HOME_ENV, value) },
            None => unsafe { std::env::remove_var(DATA_HOME_ENV) },
        }
        assert_eq!(
            resolved, None,
            "an unmarked data home is the developer's real cache and must not be adopted"
        );
    }

    /// The env override only isolates if the real resolver accepts it — `ShellRuntime::new` runs the
    /// value through `validate_socket_path` and silently falls back to the user's per-user socket on
    /// rejection, which is exactly the leak this test exists to catch.
    #[cfg(all(feature = "shells", any(unix, windows)))]
    #[test]
    fn shell_runtime_resolves_the_isolated_endpoint() {
        init_isolated_cache();
        let expected =
            PathBuf::from(std::env::var_os("BASEMIND_SHELLS_SOCKET").expect("BASEMIND_SHELLS_SOCKET is set"));
        assert_eq!(
            crate::shells::ShellRuntime::new().socket_path(),
            expected.as_path(),
            "the shells runtime must bind the isolated endpoint, not the per-user default"
        );
    }
}

#[cfg(test)]
mod status_sidecar_tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_status_sidecar(dir.path(), 42, 7);
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
