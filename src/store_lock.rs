//! Store write-lock machinery: the advisory `.basemind/.lock` flock, the `.lock.meta` holder
//! sidecar, acquisition with retry, and the non-blocking writer probe.
//!
//! Split out of `store.rs` to keep that file under the 1000-line module cap. The lock *types*
//! ([`LockHolder`] / [`LockMeta`]) and [`StoreError`] stay in `store.rs`; this module holds the
//! behavior over them. `store.rs` re-exports the public entry points (`probe_writer_lock`,
//! `acquire_lock*`, …) so callers keep importing them from `crate::store`.

use std::fs::{File, OpenOptions};
use std::path::Path;

use fs2::FileExt;

use crate::store::{LOCK_FILE, LOCK_META_FILE, LockHolder, LockMeta, StoreError};

/// Best-effort probe: does a live writer currently hold the exclusive `.basemind/.lock`?
///
/// A read-only consumer calls this before deciding whether to even attempt opening the
/// single-holder Fjall index. If a writer holds the lock, the reader's open cannot succeed anyway
/// (Fjall is one-holder) — and, worse, the reader's *transient* acquisition attempt can knock the
/// rightful writer into read-only (the multi-session writer-downgrade race). So when a writer is
/// live the reader skips Fjall entirely and serves from the concurrently-readable blobs
/// (`index_db = None`). Returns `false` when no lock file exists or the probe can't run — the
/// caller then falls through to attempting the open (correct for the no-writer CLI case).
pub(crate) fn writer_lock_is_held(basemind_dir: &Path) -> bool {
    let path = basemind_dir.join(LOCK_FILE);
    let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
        return false;
    };
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            false
        }
        Err(_) => true,
    }
}

/// Non-blocking classification of the store write lock, for the CLI write path to pre-detect a
/// live `serve` / `watch` *before* colliding with it. The reactive [`StoreError::Locked`] path
/// remains the safety net for the race between this probe and the actual acquire.
#[derive(Debug)]
pub enum WriterProbe {
    /// No live writer holds the lock — safe to open for write.
    Free,
    /// A writer holds the lock. `holder` names it from the `.lock.meta` sidecar when readable;
    /// `None` when the sidecar is missing/corrupt (the holder is live but unidentified).
    Held { holder: Option<LockMeta> },
}

/// Probe the store write lock without blocking or acquiring it. See [`WriterProbe`]. A `Free`
/// result is advisory only — a writer may start between this call and a subsequent acquire, so
/// callers must still handle [`StoreError::Locked`].
pub fn probe_writer_lock(basemind_dir: &Path) -> WriterProbe {
    if writer_lock_is_held(basemind_dir) {
        WriterProbe::Held {
            holder: read_lock_meta(basemind_dir),
        }
    } else {
        WriterProbe::Free
    }
}

pub(crate) fn acquire_lock(basemind_dir: &Path) -> Result<File, StoreError> {
    acquire_lock_as(basemind_dir, LockHolder::Maintenance)
}

/// Acquire the store lock, recording `holder` in the `.lock.meta` sidecar on success and
/// reading the *existing* holder's sidecar on contention so the error names the live holder.
pub(crate) fn acquire_lock_as(basemind_dir: &Path, holder: LockHolder) -> Result<File, StoreError> {
    let path = basemind_dir.join(LOCK_FILE);
    const LOCK_ATTEMPTS: u32 = 25;
    const LOCK_BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);
    for attempt in 0..LOCK_ATTEMPTS {
        // Opened again on every attempt, and checked after the lock is won: a remover holds this
        // lock while it takes the whole directory away, and a lock won on the file it unlinked
        // would guard nothing against the next opener, who creates a new one.
        let file = open_lock_file(basemind_dir, &path)?;
        match file.try_lock_exclusive() {
            Ok(()) if is_file_at(&file, &path) => {
                write_lock_meta(basemind_dir, holder);
                return Ok(file);
            }
            Ok(()) => {}
            Err(_) if attempt + 1 < LOCK_ATTEMPTS => std::thread::sleep(LOCK_BACKOFF),
            Err(_) => break,
        }
    }
    Err(StoreError::Locked {
        holder: read_lock_meta(basemind_dir),
        path,
    })
}

/// Open (creating) the lock file, and its directory when a remover just took that away.
fn open_lock_file(basemind_dir: &Path, path: &Path) -> Result<File, StoreError> {
    let open = || {
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
    };
    open()
        .or_else(|error| {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error);
            }
            std::fs::create_dir_all(basemind_dir)?;
            open()
        })
        .map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })
}

/// Whether `file` is still the file at `path`: the same inode on unix. Elsewhere a directory
/// cannot be removed while a file in it is open, so the path existing is the whole test.
fn is_file_at(file: &File, path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (file.metadata(), std::fs::metadata(path)) {
            (Ok(held), Ok(current)) => held.ino() == current.ino() && held.dev() == current.dev(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        path.exists()
    }
}

/// Directory, beside the one that holds the workspace directories, where a workspace directory is
/// parked on its way out.
const REAPED_DIR: &str = ".reaped";

/// Remove a workspace directory whose lock the caller holds. The directory is renamed aside first
/// and deleted from there: its path is free the instant the rename lands, so an opener never finds
/// a half-deleted directory, and one that was waiting on this lock sees its file is no longer the
/// one at the path and starts over ([`acquire_lock_as`]).
pub(crate) fn remove_dir_under_lock(workspace_dir: &Path) -> std::io::Result<()> {
    let parked = workspace_dir.parent().and_then(Path::parent).and_then(|cache| {
        let reaped = cache.join(REAPED_DIR);
        std::fs::create_dir_all(&reaped).ok()?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let name = workspace_dir.file_name()?.to_string_lossy().into_owned();
        let parked = reaped.join(format!("{name}-{}-{stamp}", std::process::id()));
        std::fs::rename(workspace_dir, &parked).ok()?;
        Some(parked)
    });
    match parked {
        Some(parked) => std::fs::remove_dir_all(parked),
        None => std::fs::remove_dir_all(workspace_dir),
    }
}

/// Delete what a remover that died mid-delete left parked beside `workspaces_dir`.
pub(crate) fn purge_reaped(workspaces_dir: &Path) {
    if let Some(cache) = workspaces_dir.parent() {
        let _ = std::fs::remove_dir_all(cache.join(REAPED_DIR));
    }
}

/// Write the `.lock.meta` sidecar naming the current holder. Best-effort and atomic
/// (tmp + rename): the lock itself is already held when this runs, so a failure here only
/// degrades the *next* contender's error message to the generic guess — never a correctness
/// issue. Errors are swallowed deliberately.
fn write_lock_meta(basemind_dir: &Path, holder: LockHolder) {
    let acquired_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let meta = LockMeta {
        command: holder.command().to_string(),
        pid: std::process::id(),
        acquired_unix,
    };
    let Ok(bytes) = serde_json::to_vec(&meta) else {
        return;
    };
    let final_path = basemind_dir.join(LOCK_META_FILE);
    let tmp_path = basemind_dir.join(format!("{LOCK_META_FILE}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp_path, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp_path, &final_path);
    }
}

/// Read the `.lock.meta` sidecar to identify the live holder. `None` when it is absent or
/// unparsable, so the caller falls back to the generic lock message.
fn read_lock_meta(basemind_dir: &Path) -> Option<LockMeta> {
    let bytes = std::fs::read(basemind_dir.join(LOCK_META_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_writer_lock_reports_free_when_unlocked() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(
            matches!(probe_writer_lock(tmp.path()), WriterProbe::Free),
            "an untouched .basemind dir has no live writer"
        );
    }

    #[test]
    fn probe_writer_lock_names_the_live_holder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let guard = acquire_lock_as(tmp.path(), LockHolder::Serve).expect("acquire exclusive lock");
        match probe_writer_lock(tmp.path()) {
            WriterProbe::Held { holder: Some(meta) } => {
                assert_eq!(meta.command, "basemind serve", "sidecar names the holder")
            }
            other => panic!("expected Held with named holder, got {other:?}"),
        }
        drop(guard);
        // The lock fd is O_CLOEXEC, but a sibling test thread that forks a subprocess between the
        // fork and its exec briefly holds a copy of the open file description, and a flock lives as
        // long as ANY copy does. So "free once dropped" is eventual under parallel tests: poll for
        // the child's exec (milliseconds) instead of asserting on the first probe.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !matches!(probe_writer_lock(tmp.path()), WriterProbe::Free) {
            assert!(
                std::time::Instant::now() < deadline,
                "lock is free once the holder drops"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// A remover holds the lock while it takes the workspace directory away. An opener that was
    /// waiting must end up holding the lock file that is at the path, not the one that was
    /// unlinked with the directory.
    #[cfg(unix)]
    #[test]
    fn an_opener_waiting_on_a_removed_directory_locks_the_file_at_the_path() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("workspaces").join("key");
        std::fs::create_dir_all(&workspace).expect("mk workspace");
        std::fs::write(workspace.join("status.json"), b"{}").expect("content");
        let remover = acquire_lock(&workspace).expect("the remover locks the workspace");

        let opener = {
            let workspace = workspace.clone();
            std::thread::spawn(move || acquire_lock_as(&workspace, LockHolder::Scan))
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        remove_dir_under_lock(&workspace).expect("remove");
        drop(remover);

        let held = opener.join().expect("join").expect("the opener gets the lock");
        let at_path = std::fs::metadata(workspace.join(LOCK_FILE)).expect("a lock file is at the path");
        assert_eq!(held.metadata().expect("held").ino(), at_path.ino());
        assert!(!workspace.join("status.json").exists(), "the old directory is gone");
        assert!(
            std::fs::read_dir(tmp.path().join(REAPED_DIR))
                .expect("parking dir")
                .next()
                .is_none(),
            "nothing is left parked"
        );
    }

    /// A lock won on a file that is no longer at its path is not a lock on the workspace.
    #[cfg(unix)]
    #[test]
    fn a_lock_file_replaced_at_its_path_is_not_the_current_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(LOCK_FILE);
        let file = open_lock_file(tmp.path(), &path).expect("open");
        assert!(is_file_at(&file, &path));

        std::fs::remove_file(&path).expect("unlink");
        assert!(!is_file_at(&file, &path), "unlinked");
        let _replacement = open_lock_file(tmp.path(), &path).expect("recreate");
        assert!(!is_file_at(&file, &path), "another file is at the path");
    }
}
