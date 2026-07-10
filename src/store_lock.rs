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
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
    const LOCK_ATTEMPTS: u32 = 25;
    const LOCK_BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);
    for attempt in 0..LOCK_ATTEMPTS {
        match file.try_lock_exclusive() {
            Ok(()) => {
                write_lock_meta(basemind_dir, holder);
                return Ok(file);
            }
            Err(_) if attempt + 1 < LOCK_ATTEMPTS => std::thread::sleep(LOCK_BACKOFF),
            Err(_) => {
                return Err(StoreError::Locked {
                    holder: read_lock_meta(basemind_dir),
                    path,
                });
            }
        }
    }
    unreachable!("loop returns on the final attempt")
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
        assert!(
            matches!(probe_writer_lock(tmp.path()), WriterProbe::Free),
            "lock is free once the holder drops"
        );
    }
}
