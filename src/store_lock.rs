//! Store write-lock machinery: the advisory `.basemind/.lock` flock, the `.lock.meta` holder
//! sidecar, acquisition with retry, and the non-blocking writer probe.
//!
//! Split out of `store.rs` to keep that file under the 1000-line module cap. [`StoreError`] stays
//! in `store.rs`; this module holds the lock types ([`LockHolder`] / [`LockMeta`]) and the behavior
//! over them. `store.rs` re-exports the public names (`LockHolder`, `probe_writer_lock`,
//! `acquire_lock*`, …) so callers keep importing them from `crate::store`.

use std::fs::{File, OpenOptions};
use std::path::Path;

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::store::{LOCK_FILE, LOCK_META_FILE, StoreError};

/// Which basemind command is taking the exclusive store lock. Threaded from the caller
/// (`scan` / `rescan` / `watch` / `serve`) into [`Store::open_with_holder`] so a lock
/// contention error can name the *actual* holder rather than a hardcoded guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockHolder {
    /// `basemind serve` — the long-running MCP server (the common holder; an editor plugin).
    Serve,
    /// `basemind watch` — the filesystem watcher / incremental re-indexer.
    Watch,
    /// `basemind scan` — a one-shot full index.
    Scan,
    /// `basemind rescan` — an incremental re-index.
    Rescan,
    /// GC / cache maintenance or any caller that did not specify a more precise identity.
    Maintenance,
}

impl LockHolder {
    /// The exact CLI command a user would run for this holder, used verbatim in the error
    /// message so the guidance is actionable ("stop `basemind serve`").
    pub fn command(self) -> &'static str {
        match self {
            LockHolder::Serve => "basemind serve",
            LockHolder::Watch => "basemind watch",
            LockHolder::Scan => "basemind scan",
            LockHolder::Rescan => "basemind rescan",
            LockHolder::Maintenance => "a basemind cache/maintenance task",
        }
    }
}

/// On-disk sidecar describing who currently holds the store lock. Written atomically when
/// the exclusive lock is acquired and read on contention. Additive, non-load-bearing: a
/// missing or corrupt sidecar simply falls back to the generic lock message, so it never
/// trips schema wipe-on-mismatch (it lives outside the versioned index/blob stores).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockMeta {
    /// The CLI command of the holder (`basemind serve`, etc.).
    pub command: String,
    /// OS process id of the holder.
    pub pid: u32,
    /// Unix-epoch seconds when the lock was acquired.
    pub acquired_unix: i64,
}

/// Render the lock-contention message, naming the live holder from the `.lock.meta` sidecar
/// when it is available and falling back to the generic guess otherwise. Kept as a free fn so
/// the `thiserror` `#[error(...)]` attribute can call it for [`StoreError::Locked`].
pub(crate) fn lock_contention_message(path: &Path, holder: &Option<LockMeta>) -> String {
    match holder {
        Some(meta) => format!(
            "another basemind process holds the lock on {} (`{}`, pid {})",
            path.display(),
            meta.command,
            meta.pid
        ),
        None => format!(
            "another basemind process holds the lock on {} (usually the `basemind serve` MCP \
             server from your editor plugin, or `basemind watch`)",
            path.display()
        ),
    }
}

/// Actionable guidance printed when a CLI writer (`scan` / `rescan`) can't acquire the
/// store lock because another basemind process is holding it. Kept as a constant so the
/// scan and rescan paths emit identical wording and a test can assert the contract.
pub const LOCK_CONTENTION_HELP: &str = "the basemind index is locked by another process \
(likely the MCP server). If an editor/plugin is serving this repo, use its `index_rescan` tool \
to refresh the index, or stop that server before running `basemind scan`.";

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

/// An advisory lock that is held, and released by an explicit unlock when dropped.
///
/// Closing the file is not enough. A lock belongs to the open file description, and a child that
/// another thread forked holds a copy of the descriptor until it execs: for that moment the lock
/// outlives the close, and the next `try_lock` on the file finds it held. An unlock releases it
/// whatever copies exist.
#[derive(Debug)]
pub(crate) struct LockGuard(File);

impl LockGuard {
    /// Take over `file`, on which the caller has just won a lock.
    pub(crate) fn held(file: File) -> Self {
        Self(file)
    }
}

impl std::ops::Deref for LockGuard {
    type Target = File;

    fn deref(&self) -> &File {
        &self.0
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

pub(crate) fn acquire_lock(basemind_dir: &Path) -> Result<LockGuard, StoreError> {
    acquire_lock_as(basemind_dir, LockHolder::Maintenance)
}

/// Acquire the store lock, recording `holder` in the `.lock.meta` sidecar on success and
/// reading the *existing* holder's sidecar on contention so the error names the live holder.
pub(crate) fn acquire_lock_as(basemind_dir: &Path, holder: LockHolder) -> Result<LockGuard, StoreError> {
    let path = basemind_dir.join(LOCK_FILE);
    const LOCK_ATTEMPTS: u32 = 25;
    const LOCK_BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);
    // A lock won on a file no longer at the path is not contention: the remover that held it is
    // gone, and the next open finds or creates the file that is there. Bounded apart from the
    // contended attempts, so winning a stale file on the last of those does not report a free
    // lock as held.
    const STALE_RETRIES: u32 = 100;
    let (mut contended, mut stale) = (0, 0);
    while contended < LOCK_ATTEMPTS && stale < STALE_RETRIES {
        // Opened again on every attempt, and checked after the lock is won: a remover holds this
        // lock while it takes the whole directory away, and a lock won on the file it unlinked
        // would guard nothing against the next opener, who creates a new one.
        let file = open_lock_file(basemind_dir, &path)?;
        #[cfg(test)]
        hook::run(&path);
        match file.try_lock_exclusive() {
            Ok(()) if is_file_at(&file, &path) => {
                write_lock_meta(basemind_dir, holder);
                return Ok(LockGuard::held(file));
            }
            Ok(()) => stale += 1,
            Err(_) => {
                contended += 1;
                if contended < LOCK_ATTEMPTS {
                    std::thread::sleep(LOCK_BACKOFF);
                }
            }
        }
    }
    Err(StoreError::Locked {
        holder: read_lock_meta(basemind_dir),
        path,
    })
}

/// A seam between opening the lock file and trying to lock it, where a test puts the remover that
/// replaces the file at the path.
#[cfg(test)]
pub(crate) mod hook {
    use std::cell::RefCell;
    use std::path::Path;

    type Hook = Box<dyn FnMut(&Path)>;

    thread_local! {
        static AFTER_OPEN: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Run `hook` after every open of a lock file on this thread, until the guard drops. Unix only:
    /// the tests that swap the file at the path are.
    #[cfg(unix)]
    pub(crate) fn after_open(hook: impl FnMut(&Path) + 'static) -> Guard {
        AFTER_OPEN.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Guard
    }

    #[cfg(unix)]
    pub(crate) struct Guard;

    #[cfg(unix)]
    impl Drop for Guard {
        fn drop(&mut self) {
            AFTER_OPEN.with(|slot| *slot.borrow_mut() = None);
        }
    }

    pub(super) fn run(path: &Path) {
        // Taken out while it runs, so a hook that itself opens a lock file does not re-enter.
        let hook = AFTER_OPEN.with(|slot| slot.borrow_mut().take());
        if let Some(mut hook) = hook {
            hook(path);
            AFTER_OPEN.with(|slot| *slot.borrow_mut() = Some(hook));
        }
    }
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

/// How long after parking a directory its remover is taken to be still deleting it. A delete that
/// failed is never retried by the remover, and after a restart the pid may belong to another
/// process: past this, the directory is anyone's to delete.
const PARKED_REMOVAL_WINDOW: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// The pid and the parking instant in a parked directory's name (`<workspace>-<pid>-<stamp>`).
fn parked_by(name: &str) -> Option<(u32, std::time::SystemTime)> {
    let mut parts = name.rsplitn(3, '-');
    let stamp: u64 = parts.next()?.parse().ok()?;
    let pid = parts.next()?.parse().ok()?;
    Some((pid, std::time::UNIX_EPOCH + std::time::Duration::from_nanos(stamp)))
}

/// Whether a directory parked at `parked` may still be being deleted by its remover. A stamp
/// ahead of the clock by more than the window is as old as one behind it: the clock was moved.
fn parked_recently(parked: std::time::SystemTime, now: std::time::SystemTime) -> bool {
    let apart = now.duration_since(parked).unwrap_or_else(|ahead| ahead.duration());
    apart < PARKED_REMOVAL_WINDOW
}

/// Whether the process that parked a directory may still be deleting it.
fn remover_is_live(pid: u32) -> bool {
    #[cfg(any(unix, windows))]
    {
        crate::daemon_lock::pid_is_live(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Delete what a remover that died or gave up mid-delete left parked beside `workspaces_dir`. A
/// directory parked within [`PARKED_REMOVAL_WINDOW`] by a process that is still alive is that
/// process's to finish: deleting under it would fail its own delete on a file that is suddenly
/// gone.
pub(crate) fn purge_reaped(workspaces_dir: &Path) {
    let Some(reaped) = workspaces_dir.parent().map(|cache| cache.join(REAPED_DIR)) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in std::fs::read_dir(&reaped).into_iter().flatten().flatten() {
        let live = entry
            .file_name()
            .to_str()
            .and_then(parked_by)
            .is_some_and(|(pid, parked)| parked_recently(parked, now) && remover_is_live(pid));
        if !live {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    // Only an empty directory goes.
    let _ = std::fs::remove_dir(&reaped);
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

    /// Replace the file at `path` with another one: both exist at once, so the inodes differ.
    #[cfg(unix)]
    fn swap_lock_file(path: &Path) {
        let replacement = path.with_extension("swap");
        std::fs::write(&replacement, b"").expect("replacement");
        std::fs::rename(&replacement, path).expect("swap");
    }

    /// The window the identity check exists for: the file is opened, a remover replaces it at the
    /// path, and the lock is then won on the file that is no longer there.
    #[cfg(unix)]
    #[test]
    fn a_lock_won_on_a_file_swapped_after_the_open_is_not_accepted() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let swaps = std::rc::Rc::new(std::cell::Cell::new(0_u32));
        let _hook = {
            let swaps = std::rc::Rc::clone(&swaps);
            hook::after_open(move |path| {
                if swaps.get() == 0 {
                    swap_lock_file(path);
                }
                swaps.set(swaps.get() + 1);
            })
        };

        let held = acquire_lock_as(tmp.path(), LockHolder::Scan).expect("the second open wins");

        assert_eq!(swaps.get(), 2, "the stale win was retried with a fresh open");
        let at_path = std::fs::metadata(tmp.path().join(LOCK_FILE)).expect("lock file");
        assert_eq!(held.metadata().expect("held").ino(), at_path.ino());
        assert!(
            writer_lock_is_held(tmp.path()),
            "the lock held is the one a second opener would contend on"
        );
    }

    /// A stale win is not contention: more of them than there are contended attempts still end in
    /// the lock, never in a free lock reported as held.
    #[cfg(unix)]
    #[test]
    fn stale_wins_do_not_use_up_the_contended_attempts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let swaps = std::rc::Rc::new(std::cell::Cell::new(0_u32));
        let _hook = {
            let swaps = std::rc::Rc::clone(&swaps);
            hook::after_open(move |path| {
                if swaps.get() < 30 {
                    swap_lock_file(path);
                }
                swaps.set(swaps.get() + 1);
            })
        };

        acquire_lock_as(tmp.path(), LockHolder::Scan).expect("the lock is free");
        assert_eq!(swaps.get(), 31);
    }

    #[test]
    fn purge_reaped_leaves_what_a_live_remover_parked() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let reaped = tmp.path().join(REAPED_DIR);
        let stamp = |ago: std::time::Duration| {
            (std::time::SystemTime::now() - ago)
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos()
        };
        let pid = std::process::id();
        let live = reaped.join(format!("key-with-dash-{pid}-{}", stamp(std::time::Duration::ZERO)));
        // No process has this pid: above every platform's pid range, below `pid_t`'s sign bit.
        let dead = reaped.join(format!("key-2147483646-{}", stamp(std::time::Duration::ZERO)));
        // A failed delete is not retried, and after a restart the pid may be another process's.
        let abandoned = reaped.join(format!("key-{pid}-{}", stamp(PARKED_REMOVAL_WINDOW * 2)));
        for dir in [&workspaces, &live, &dead, &abandoned] {
            std::fs::create_dir_all(dir).expect("mkdir");
        }

        purge_reaped(&workspaces);

        assert!(live.exists(), "a live remover is still deleting this one");
        assert!(!dead.exists(), "the remover of this one died mid-delete");
        assert!(
            !abandoned.exists(),
            "a live pid does not hold a directory parked long ago"
        );

        std::fs::remove_dir_all(&live).expect("the live remover finishes");
        purge_reaped(&workspaces);
        assert!(!reaped.exists(), "the empty parking directory goes too");
    }

    #[test]
    fn a_parking_stamp_far_from_the_clock_either_way_is_old() {
        let now = std::time::SystemTime::now();
        let minute = std::time::Duration::from_secs(60);
        assert!(parked_recently(now - minute, now));
        assert!(parked_recently(now + minute, now), "a little skew");
        assert!(!parked_recently(now - PARKED_REMOVAL_WINDOW - minute, now));
        assert!(
            !parked_recently(now + PARKED_REMOVAL_WINDOW + minute, now),
            "the clock was set back"
        );
    }

    /// A lock released by closing its file alone stays held while a child another thread forked
    /// still has a copy of the descriptor, before it execs. The guard unlocks, so the lock is free
    /// the moment it drops, however many processes are being spawned around it. The children are
    /// forked and exec'd: a plain spawn may use `posix_spawn` (always on macOS), which leaves no
    /// moment in which a child holds the descriptor, and the test could not fail.
    #[cfg(unix)]
    #[test]
    fn a_dropped_lock_is_free_at_once_while_other_threads_spawn_processes() {
        use std::os::unix::process::CommandExt;
        use std::sync::atomic::{AtomicBool, Ordering};

        let tmp = tempfile::tempdir().expect("tempdir");
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let spawners: Vec<_> = (0..8)
            .map(|_| {
                let stop = std::sync::Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let mut child = std::process::Command::new("/usr/bin/true");
                        // SAFETY: the hook does nothing, so it is async-signal-safe; it is there
                        // only because std forks and execs a command that has one.
                        unsafe {
                            child.pre_exec(|| Ok(()));
                        }
                        let _ = child.status();
                    }
                })
            })
            .collect();

        let mut still_held = 0;
        for _ in 0..400 {
            drop(acquire_lock(tmp.path()).expect("acquire"));
            let reopened = open_lock_file(tmp.path(), &tmp.path().join(LOCK_FILE)).expect("open");
            if reopened.try_lock_exclusive().is_err() {
                still_held += 1;
            }
            let _ = FileExt::unlock(&reopened);
        }
        stop.store(true, Ordering::Relaxed);
        for spawner in spawners {
            spawner.join().expect("join");
        }

        assert_eq!(
            still_held, 0,
            "a dropped lock was still held on {still_held} of 400 relocks"
        );
    }
}
