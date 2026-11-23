//! Where a content-addressed blob lives inside a blob namespace directory.
//!
//! A blob is `<hash>.<tier>.msgpack`. Releases before the sharded layout kept every blob flat in the
//! namespace directory; with many worktrees and repositories sharing the cache that is hundreds of
//! thousands of entries in one directory. The current layout puts a blob under a subdirectory named
//! for the first two hex digits of its hash (`<namespace>/ab/<hash>.fm.msgpack`), 256 buckets.
//!
//! The namespace (`v<SCHEMA_VER>`) does not change, so an upgrade rewrites nothing: a reader finds a
//! flat blob and moves it into its bucket by rename, one blob at a time as it is touched
//! ([`locate_blob`]). An older release keeps reading and writing flat names in the same namespace; a
//! blob it writes is found and migrated by the next current-release read, and one it cannot see in
//! a bucket is rebuilt by its next scan. Sweeps and counts walk both layouts ([`blob_files`]).

use std::path::{Path, PathBuf};

/// Hex digits of the hash that name a blob's bucket directory.
const SHARD_DIGITS: usize = 2;

/// The bucket directory name of the blob `hash_hex`: its first [`SHARD_DIGITS`] characters, or the
/// whole stem when it is shorter (only a hand-made name in a test is).
fn shard_name(hash_hex: &str) -> &str {
    hash_hex.get(..SHARD_DIGITS).unwrap_or(hash_hex)
}

/// Whether `name` is a bucket directory name: [`SHARD_DIGITS`] lowercase hex digits.
fn is_shard_name(name: &str) -> bool {
    name.len() == SHARD_DIGITS && name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The sharded path of the blob `<hash_hex><suffix>` under `blobs_dir`. Pure: touches no file.
pub(crate) fn sharded_blob_path(blobs_dir: &Path, hash_hex: &str, suffix: &str) -> PathBuf {
    blobs_dir.join(shard_name(hash_hex)).join(format!("{hash_hex}{suffix}"))
}

/// The flat path an older release wrote the blob `<hash_hex><suffix>` to.
pub(crate) fn flat_blob_path(blobs_dir: &Path, hash_hex: &str, suffix: &str) -> PathBuf {
    blobs_dir.join(format!("{hash_hex}{suffix}"))
}

/// The path to read or probe the blob `<hash_hex><suffix>` under `blobs_dir`: always the sharded
/// path, after moving a flat blob of that name into place when the sharded one is absent. The probe
/// costs one `stat` for a blob already sharded; a blob that exists in neither layout costs two and
/// yields the (absent) sharded path, where a writer then creates it.
///
/// A migration that loses a race (another reader moved the blob first) or fails (read-only cache)
/// is not an error: the sharded path is returned if it now exists, else the flat one, so the caller
/// still reads the blob.
pub(crate) fn locate_blob(blobs_dir: &Path, hash_hex: &str, suffix: &str) -> PathBuf {
    let sharded = sharded_blob_path(blobs_dir, hash_hex, suffix);
    if sharded.exists() {
        return sharded;
    }
    let flat = flat_blob_path(blobs_dir, hash_hex, suffix);
    if !flat.exists() {
        return sharded;
    }
    if migrate_blob(&flat, &sharded) || sharded.exists() {
        sharded
    } else {
        flat
    }
}

/// Move `flat` to `sharded`, creating the bucket directory. Whether it moved.
///
/// The move is a hard link followed by removing `flat`, not a rename: a rename replaces an existing
/// destination, so a writer that published `sharded` between the caller's probe and this call would
/// lose its blob to the older flat one. A link fails when `sharded` exists. A `flat` that cannot be
/// removed after the link leaves a twin that a sweep reaps; the blob has still moved.
fn migrate_blob(flat: &Path, sharded: &Path) -> bool {
    let Some(bucket) = sharded.parent() else {
        return false;
    };
    if std::fs::create_dir_all(bucket).is_err() || std::fs::hard_link(flat, sharded).is_err() {
        return false;
    }
    let _ = std::fs::remove_file(flat);
    true
}

/// Every file of a blob namespace in either layout: those directly in `blobs_dir` (flat, written by
/// an older release) and those inside its bucket directories. A missing `blobs_dir` yields none. A
/// bucket that vanishes while it is walked (a concurrent wipe) is skipped; any other read failure is
/// an error, so a sweep never decides on a partial listing.
pub(crate) fn blob_files(blobs_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let entries = match std::fs::read_dir(blobs_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let is_bucket = file_type.is_dir() && entry.file_name().to_str().is_some_and(is_shard_name);
        if !is_bucket {
            if file_type.is_file() {
                files.push(path);
            }
            continue;
        }
        match std::fs::read_dir(&path) {
            Ok(bucket) => {
                for blob in bucket {
                    let blob = blob?;
                    if blob.file_type()?.is_file() {
                        files.push(blob.path());
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(files)
}

#[cfg(test)]
#[path = "store_blob_layout_tests.rs"]
mod tests;
