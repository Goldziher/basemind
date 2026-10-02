//! Seed a fresh linked-worktree workspace's `working` view from a sibling checkout of the same repo.
//!
//! A new worktree's per-workspace index starts empty, so its first scan cannot use the
//! `process_file` fast path (which needs an existing table entry) and re-reads, decodes and
//! rewrites every file even though the content-addressed blobs already exist from the sibling.
//! Cloning the sibling's `views/working/` (`index.msgpack` + `index.fjall/`) before fjall is
//! opened lets the scan see each content-identical file as `Unchanged` (read + hash only, no
//! fjall writes) while differing files re-extract normally. `process_file` compares the file HASH
//! to the entry when the mtime differs, so stale mtimes carried over from the sibling are harmless.
//!
//! What is cloned: only the view directory. It holds relative paths and hashes, nothing that embeds
//! the sibling root. What is NOT cloned, deliberately: `lance/` (lives beside `views/`, not inside
//! it; its documents/code-chunk rows are keyed by `(scope, path)` and the vectors belong to the
//! sibling's scope, so copying would leak foreign rows and the new workspace rebuilds it lazily),
//! `workspace.json` (keyed to the new root by `ensure_workspace_marker`), `status.json`, `.lock`.
//!
//! Crash safety: the clone lands in `<workspace>/.seed-tmp` (same filesystem), is vetted
//! (msgpack schema + fjall `meta.schema_ver`), and only then renamed onto the view path.
//! Best-effort throughout: any failure logs and falls back to the cold path.

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::git::Repo;
use crate::index::{INDEX_SCHEMA_VER, peek_index_schema_ver};
use crate::store_layout::{
    CACHE_DIR, INDEX_FILE, LOCK_FILE, VIEW_WORKING, VIEWS_DIR, WORKSPACES_DIR, cache_root, read_status_sidecar,
    read_workspace_marker, workspace_cache_dir, workspace_key,
};

/// Opt-out: any non-empty value other than `0` disables seeding.
pub const NO_SEED_ENV: &str = "BASEMIND_NO_SEED";
const INDEX_DB_DIR: &str = "index.fjall";
const SEED_TMP_DIR: &str = ".seed-tmp";
/// Non-macOS fallback copies real bytes when reflink is unavailable; refuse past this size.
#[cfg(not(target_os = "macos"))]
const MAX_PLAIN_COPY_BYTES: u64 = 2 * 1_024 * 1_024 * 1_024;

fn seed_disabled() -> bool {
    std::env::var(NO_SEED_ENV).is_ok_and(|v| !v.is_empty() && v != "0")
}

fn view_is_populated(view_dir: &Path) -> bool {
    view_dir.join(INDEX_FILE).is_file() && view_dir.join(INDEX_DB_DIR).is_dir()
}

/// Canonical main-worktree root when `root` is the top of a git worktree (not a subdirectory of one).
fn main_root_of(root: &Path) -> Option<PathBuf> {
    let repo = Repo::discover(root).ok()?;
    let canonical = root.canonicalize().ok()?;
    if repo.workdir().canonicalize().ok()? != canonical {
        return None;
    }
    repo.main_worktree_root().canonicalize().ok()
}

/// Score a candidate workspace dir: `None` when it has no populated working view.
fn candidate_score(workspace_dir: &Path) -> Option<u64> {
    let view = workspace_dir.join(VIEWS_DIR).join(VIEW_WORKING);
    if !view_is_populated(&view) {
        return None;
    }
    let files = read_status_sidecar(workspace_dir)
        .map(|s| s.file_count as u64)
        .or_else(|| std::fs::metadata(view.join(INDEX_FILE)).ok().map(|m| m.len()))?;
    (files > 0).then_some(files)
}

/// Pick the sibling workspace dir to clone from: the main worktree's workspace when populated,
/// else the populated same-repo sibling with the most files.
fn find_source(root: &Path, own_dir: &Path) -> Option<PathBuf> {
    let main_root = main_root_of(root)?;
    let own_key = workspace_key(root);
    if workspace_key(&main_root) != own_key {
        let main_dir = workspace_cache_dir(&main_root);
        if candidate_score(&main_dir).is_some() {
            return Some(main_dir);
        }
    }
    let workspaces = cache_root().join(CACHE_DIR).join(WORKSPACES_DIR);
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(&workspaces).ok()?.flatten() {
        let dir = entry.path();
        if dir == own_dir || !dir.is_dir() {
            continue;
        }
        let Some(score) = candidate_score(&dir) else { continue };
        if best.as_ref().is_some_and(|(s, _)| *s >= score) {
            continue;
        }
        let Some(marker) = read_workspace_marker(&dir) else {
            continue;
        };
        if !marker.root.exists() || main_root_of(&marker.root).as_deref() != Some(main_root.as_path()) {
            continue;
        }
        best = Some((score, dir));
    }
    best.map(|(_, dir)| dir)
}

/// Seed `view_dir` (the `working` view of the workspace at `root`) from a sibling. Returns the
/// source workspace dir on success; `None` when seeding was skipped or fell back. The caller holds
/// `basemind_dir`'s exclusive lock and has not yet opened fjall.
pub(crate) fn seed_working_view(root: &Path, basemind_dir: &Path, view_dir: &Path) -> Option<PathBuf> {
    if seed_disabled() || cfg!(windows) || view_is_populated(view_dir) {
        return None;
    }
    let started = Instant::now();
    let source_ws = find_source(root, basemind_dir)?;
    let source_view = source_ws.join(VIEWS_DIR).join(VIEW_WORKING);
    let tmp = basemind_dir.join(SEED_TMP_DIR);
    let _ = std::fs::remove_dir_all(&tmp);

    // Hold the sibling's flock while cloning when it is free (no writer mid-flush); a live
    // daemon-held sibling is tolerated: fjall journals are crash-consistent.
    let sibling_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(source_ws.join(LOCK_FILE))
        .ok()
        .and_then(|f| fs2::FileExt::try_lock_exclusive(&f).ok().map(|()| f));
    let cloned = clone_dir(&source_view, &tmp);
    drop(sibling_lock);

    let result = cloned
        .map_err(|e| e.to_string())
        .and_then(|()| vet_clone(&tmp))
        .and_then(|()| promote(&tmp, view_dir));
    match result {
        Ok(()) => {
            tracing::info!(
                source = %source_ws.display(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "seeded working view from sibling worktree"
            );
            Some(source_ws)
        }
        Err(reason) => {
            let _ = std::fs::remove_dir_all(&tmp);
            tracing::info!(source = %source_ws.display(), %reason, "view seed skipped; cold scan");
            None
        }
    }
}

/// Reject a clone whose msgpack or fjall schema does not match this build, or is unreadable.
fn vet_clone(tmp: &Path) -> Result<(), String> {
    match crate::store::read_index(tmp) {
        Ok(Some(_)) => {}
        Ok(None) => return Err("clone has no index.msgpack".into()),
        Err(e) => return Err(format!("msgpack: {e}")),
    }
    match peek_index_schema_ver(tmp) {
        Ok(Some(v)) if v == INDEX_SCHEMA_VER => Ok(()),
        Ok(other) => Err(format!("fjall schema_ver {other:?} != {INDEX_SCHEMA_VER}")),
        Err(e) => Err(format!("fjall: {e}")),
    }
}

/// Atomically move the vetted clone into place. `view_dir` may exist as an empty directory.
fn promote(tmp: &Path, view_dir: &Path) -> Result<(), String> {
    if view_dir.exists() {
        std::fs::remove_dir_all(view_dir).map_err(|e| format!("clear {}: {e}", view_dir.display()))?;
    }
    std::fs::rename(tmp, view_dir).map_err(|e| format!("rename: {e}"))
}

#[cfg(target_os = "macos")]
fn clone_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "nul in path"))
    };
    let (s, d) = (c(src)?, c(dst)?);
    // SAFETY: both pointers are valid NUL-terminated strings for the duration of the call.
    let rc = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn clone_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    if dir_size(src)? > MAX_PLAIN_COPY_BYTES {
        return Err(std::io::Error::other("source view exceeds plain-copy size limit"));
    }
    let result = copy_tree(src, dst);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(dst);
    }
    result
}

#[cfg(windows)]
fn clone_dir(_src: &Path, _dst: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "seeding unsupported on Windows",
    ))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn dir_size(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        total += if meta.is_dir() {
            dir_size(&entry.path())?
        } else {
            meta.len()
        };
    }
    Ok(total)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to)?;
            continue;
        }
        let input = std::fs::File::open(&from)?;
        let output = std::fs::File::create(&to)?;
        // FICLONE = _IOW(0x94, 9, int); falls back to a byte copy when the fs cannot reflink.
        // SAFETY: both fds are open for the duration of the call.
        let rc = unsafe { libc::ioctl(output.as_raw_fd(), 0x4004_9409, input.as_raw_fd()) };
        if rc != 0 {
            drop(output);
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigV1;
    use crate::scanner::{EmbedMode, ScanSource, scan};
    use crate::store::{Store, init_isolated_cache};
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// Main checkout (scanned, store closed) plus a fresh linked worktree beside it.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        init_isolated_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");
        let main = base.join("main");
        std::fs::create_dir(&main).expect("mkdir");
        git(&main, &["init", "-q"]);
        for (name, body) in [
            ("a.rs", "pub fn a() {}\n"),
            ("b.rs", "pub fn b() {}\n"),
            ("c.rs", "pub fn c() {}\n"),
        ] {
            std::fs::write(main.join(name), body).expect("write");
        }
        git(&main, &["add", "."]);
        git(&main, &["commit", "-q", "-m", "init"]);
        let wt = base.join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().expect("utf8")]);
        let mut store = Store::open(&main, VIEW_WORKING).expect("open main");
        scan(
            &main,
            &mut store,
            &ConfigV1::with_defaults(),
            ScanSource::WorkingTree,
            EmbedMode::Inline,
        )
        .expect("scan main");
        drop(store);
        (tmp, main, wt.canonicalize().expect("wt"))
    }

    #[test]
    fn seeded_view_makes_identical_files_unchanged_and_reextracts_modified() {
        let (_tmp, main, wt) = fixture();
        std::fs::write(wt.join("b.rs"), "pub fn b() { let _x = 1; }\n").expect("modify");

        let mut store = Store::open(&wt, VIEW_WORKING).expect("open wt");
        assert_eq!(store.index.files.len(), 3, "destination view appears populated");
        assert!(!workspace_cache_dir(&wt).join(SEED_TMP_DIR).exists());
        let report = scan(
            &wt,
            &mut store,
            &ConfigV1::with_defaults(),
            ScanSource::WorkingTree,
            EmbedMode::Inline,
        )
        .expect("scan wt");
        assert_eq!(report.stats.skipped_unchanged, 2, "a.rs and c.rs are content-identical");
        assert_eq!(report.stats.updated, 1, "only the modified b.rs is re-extracted");
        drop(store);
        // The sibling is untouched by the clone.
        let m = Store::open(&main, VIEW_WORKING).expect("reopen main");
        assert_eq!(m.index.files.len(), 3);
    }

    #[test]
    fn schema_mismatched_source_falls_back_to_cold_view() {
        let (_tmp, main, wt) = fixture();
        let view = workspace_cache_dir(&main).join(VIEWS_DIR).join(VIEW_WORKING);
        let path = view.join(INDEX_FILE);
        let mut idx: crate::store::Index = rmp_serde::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        idx.schema_ver = idx.schema_ver.wrapping_add(1);
        std::fs::write(&path, rmp_serde::to_vec_named(&idx).unwrap()).unwrap();

        let store = Store::open(&wt, VIEW_WORKING).expect("open wt");
        assert!(store.index.files.is_empty(), "mismatched source must not seed");
        assert!(
            !workspace_cache_dir(&wt).join(SEED_TMP_DIR).exists(),
            "no half-cloned dir left"
        );
    }

    #[test]
    fn candidate_without_populated_view_is_ignored() {
        init_isolated_cache();
        let empty = tempfile::tempdir().unwrap();
        assert!(candidate_score(empty.path()).is_none());
    }
}
