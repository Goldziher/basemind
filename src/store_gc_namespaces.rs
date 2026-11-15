//! Reaping blob namespaces this binary does not write.
//!
//! Blobs live under `cache/blobs/v<SCHEMA_VER>/` (see [`crate::store_layout::global_blobs_dir`]), so
//! two installed basemind minors never read or overwrite each other's blobs. What is left behind is
//! garbage only this sweep collects: the un-namespaced blobs of builds from before the namespaces,
//! and the namespaces of older blob schemas. Both are reaped once a file is older than the blob
//! grace period, the same age a scan's unreferenced blobs must reach. A namespace of a NEWER schema
//! belongs to a newer basemind sharing the cache and is never touched.

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::store_gc::{GcError, GcReport, read_dir};

/// Whether a file name in the blobs root is a blob an older build wrote there: a current blob
/// suffix, or one of the dead pre-0.9 ones. Anything else in the root is not this sweep's to take.
fn is_blob_file(name: &str) -> bool {
    crate::store_gc::blob_stem(name).is_some() || name.ends_with(".l1.msgpack") || name.ends_with(".l2.msgpack")
}

/// The blob schema a namespace directory name (`v<N>`) stands for.
pub(crate) fn namespace_schema(name: &str) -> Option<u16> {
    name.strip_prefix('v')?.parse().ok()
}

/// Remove `path` when it is at least `grace` old. Returns whether the file is gone.
fn reap_if_aged(path: &Path, now: SystemTime, grace: Duration, report: &mut GcReport) -> Result<bool, GcError> {
    let io = |source| GcError::Io {
        path: path.to_path_buf(),
        source,
    };
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(source) => return Err(io(source)),
    };
    report.scanned += 1;
    let age = meta
        .modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .unwrap_or(Duration::ZERO);
    if age < grace {
        return Ok(false);
    }
    std::fs::remove_file(path).map_err(io)?;
    report.removed += 1;
    report.bytes_freed += meta.len();
    Ok(true)
}

/// Reap the aged files of one superseded namespace, and the directory once it is empty.
fn reap_namespace(dir: &Path, now: SystemTime, grace: Duration, report: &mut GcReport) -> Result<(), GcError> {
    let mut emptied = true;
    for entry in read_dir(dir)? {
        let path = entry
            .map_err(|source| GcError::Io {
                path: dir.to_path_buf(),
                source,
            })?
            .path();
        if path.is_dir() || !reap_if_aged(&path, now, grace, report)? {
            emptied = false;
        }
    }
    if emptied {
        // Best effort: a writer of that version may have dropped a blob in since the listing.
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

/// Reap every aged blob under `blobs_root` that the `current` schema's namespace does not own:
/// un-namespaced blob files and the namespaces of older schemas. Newer namespaces, the current one
/// and anything unrecognised, a file in the root that is not a blob included, are left alone.
pub(crate) fn sweep_superseded_namespaces(
    blobs_root: &Path,
    current: u16,
    grace: Duration,
) -> Result<GcReport, GcError> {
    let now = SystemTime::now();
    let mut report = GcReport::default();
    if !blobs_root.exists() {
        return Ok(report);
    }
    for entry in read_dir(blobs_root)? {
        let entry = entry.map_err(|source| GcError::Io {
            path: blobs_root.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            let older = entry
                .file_name()
                .to_str()
                .and_then(namespace_schema)
                .is_some_and(|schema| schema < current);
            if older {
                reap_namespace(&path, now, grace, &mut report)?;
            }
        } else if entry.file_name().to_str().is_some_and(is_blob_file) {
            reap_if_aged(&path, now, grace, &mut report)?;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT: u16 = 7;

    fn plant(root: &Path, rel: &str) -> std::path::PathBuf {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"blob").unwrap();
        path
    }

    /// Only what an older build left behind is garbage: the current namespace is the live store and
    /// a newer one belongs to a newer basemind sharing the cache.
    #[test]
    fn only_unnamespaced_blobs_and_older_namespaces_are_reaped() {
        let root = tempfile::tempdir().unwrap();
        let legacy = plant(root.path(), "aa.fm.msgpack");
        let older = plant(root.path(), "v6/bb.fm.msgpack");
        let current = plant(root.path(), "v7/cc.fm.msgpack");
        let newer = plant(root.path(), "v8/dd.fm.msgpack");
        let unknown = plant(root.path(), "scratch/ee.fm.msgpack");
        let not_a_blob = plant(root.path(), "gc-notes.json");
        let dead_format = plant(root.path(), "ff.l1.msgpack");

        let report = sweep_superseded_namespaces(root.path(), CURRENT, Duration::ZERO).unwrap();

        assert_eq!(report.removed, 3);
        assert!(!legacy.exists(), "an un-namespaced blob is garbage");
        assert!(!dead_format.exists(), "so is one of a dead blob format");
        assert!(
            not_a_blob.exists(),
            "a file in the root that is not a blob is not ours to reap"
        );
        assert!(
            !older.exists() && !root.path().join("v6").exists(),
            "an older namespace goes"
        );
        assert!(current.exists(), "the current namespace is never swept here");
        assert!(newer.exists(), "a newer basemind's namespace is never touched");
        assert!(unknown.exists(), "an unrecognised directory is left alone");
    }

    #[test]
    fn nothing_younger_than_the_grace_period_is_reaped() {
        let root = tempfile::tempdir().unwrap();
        let legacy = plant(root.path(), "aa.fm.msgpack");
        let older = plant(root.path(), "v6/bb.fm.msgpack");

        let report = sweep_superseded_namespaces(root.path(), CURRENT, Duration::from_secs(3600)).unwrap();

        assert_eq!((report.scanned, report.removed), (2, 0));
        assert!(legacy.exists() && older.exists());
    }
}
