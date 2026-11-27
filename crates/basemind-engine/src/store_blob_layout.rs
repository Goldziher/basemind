//! Where a content-addressed blob lives inside a blob namespace directory.
//!
//! A blob is `<hash>.<tier>.msgpack`, in one of two layouts. Flat: directly in the namespace
//! directory. Sharded: under a subdirectory named for the first two hex digits of its hash
//! (`<namespace>/ab/<hash>.fm.msgpack`), 256 buckets, so that many worktrees and repositories
//! sharing the cache do not put hundreds of thousands of entries in one directory.
//!
//! Which layout a namespace uses follows its blob schema ([`layout_of`]). Every release of a minor
//! shares that minor's namespace (`v<SCHEMA_VER>`), and the releases of 0.34 before the sharded
//! layout read and write flat names only: a blob a later 0.34 patch moved into a bucket would be gone
//! for them, and a daemon of theirs still running would leave the file out of its map. So the
//! namespaces up to [`LAST_FLAT_BLOB_SCHEMA`] stay flat, and the sharded layout starts with the next
//! namespace, which no flat-only release reads. A reader that finds a blob in the other layout of its
//! namespace moves it into place by link and unlink, one blob at a time as it is touched
//! ([`locate_blob`]). Sweeps and counts walk both layouts ([`blob_files`]).

use std::path::{Path, PathBuf};

/// Hex digits of the hash that name a blob's bucket directory.
const SHARD_DIGITS: usize = 2;

/// The last blob schema whose namespace is flat: the releases of that minor before the sharded
/// layout look for every blob directly in the namespace directory.
const LAST_FLAT_BLOB_SCHEMA: u16 = 34;

/// How the blobs of one namespace are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Flat,
    Sharded,
}

impl Layout {
    /// The path of the blob `<hash_hex><suffix>` under `blobs_dir` in this layout. Pure.
    fn path(self, blobs_dir: &Path, hash_hex: &str, suffix: &str) -> PathBuf {
        match self {
            Self::Flat => flat_blob_path(blobs_dir, hash_hex, suffix),
            Self::Sharded => sharded_blob_path(blobs_dir, hash_hex, suffix),
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Flat => Self::Sharded,
            Self::Sharded => Self::Flat,
        }
    }
}

/// The layout of the namespace of blob schema `schema`.
fn layout_of(schema: u16) -> Layout {
    if schema <= LAST_FLAT_BLOB_SCHEMA {
        Layout::Flat
    } else {
        Layout::Sharded
    }
}

/// The layout of this binary's namespace.
fn current_layout() -> Layout {
    layout_of(crate::extract::SCHEMA_VER)
}

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

/// The flat path of the blob `<hash_hex><suffix>` under `blobs_dir`. Pure: touches no file.
pub(crate) fn flat_blob_path(blobs_dir: &Path, hash_hex: &str, suffix: &str) -> PathBuf {
    blobs_dir.join(format!("{hash_hex}{suffix}"))
}

/// The path the blob `<hash_hex><suffix>` has under `blobs_dir` in this binary's layout, where a
/// writer creates it. Pure: touches no file.
#[cfg(test)]
pub(crate) fn blob_path(blobs_dir: &Path, hash_hex: &str, suffix: &str) -> PathBuf {
    current_layout().path(blobs_dir, hash_hex, suffix)
}

/// The path the blob `<hash_hex><suffix>` has under `blobs_dir` in the layout this binary does not
/// use: where a release of that layout leaves a twin. Pure: touches no file.
#[cfg(test)]
pub(crate) fn displaced_blob_path(blobs_dir: &Path, hash_hex: &str, suffix: &str) -> PathBuf {
    current_layout().other().path(blobs_dir, hash_hex, suffix)
}

/// The path to read or probe the blob `<hash_hex><suffix>` under `blobs_dir`: always its path in
/// this binary's layout, after moving a blob of that name from the other layout
/// into place when it is absent there. The probe costs one `stat` for a blob already in place; a
/// blob that exists in neither layout costs two and yields the (absent) path in place, where a
/// writer then creates it.
///
/// A move that loses a race (another reader moved the blob first) or fails (read-only cache) is not
/// an error: the path in place is returned if it now exists, else the other one, so the caller
/// still reads the blob.
pub(crate) fn locate_blob(blobs_dir: &Path, hash_hex: &str, suffix: &str) -> PathBuf {
    locate_in(current_layout(), blobs_dir, hash_hex, suffix)
}

fn locate_in(layout: Layout, blobs_dir: &Path, hash_hex: &str, suffix: &str) -> PathBuf {
    let placed = layout.path(blobs_dir, hash_hex, suffix);
    if placed.exists() {
        return placed;
    }
    let displaced = layout.other().path(blobs_dir, hash_hex, suffix);
    if !displaced.exists() {
        return placed;
    }
    if move_blob(&displaced, &placed) || placed.exists() {
        placed
    } else {
        displaced
    }
}

/// Whether `path`, a blob file of the namespace `blobs_dir` named `<hash_hex><suffix>`, is a twin
/// outside this binary's layout of a blob that is in place: a release of the other layout wrote it
/// again after it was moved. The copy in place is the one every reader of this release finds first,
/// so the twin only costs space.
pub(crate) fn is_displaced_twin(blobs_dir: &Path, path: &Path, hash_hex: &str, suffix: &str) -> bool {
    is_displaced_twin_in(current_layout(), blobs_dir, path, hash_hex, suffix)
}

fn is_displaced_twin_in(layout: Layout, blobs_dir: &Path, path: &Path, hash_hex: &str, suffix: &str) -> bool {
    let placed = layout.path(blobs_dir, hash_hex, suffix);
    *path == layout.other().path(blobs_dir, hash_hex, suffix) && placed.is_file()
}

/// Move the blob at `from` to `to`, creating `to`'s directory. Whether it moved.
///
/// The move is a hard link followed by removing `from`, not a rename: a rename replaces an existing
/// destination, so a writer that published `to` between the caller's probe and this call would lose
/// its blob to the older copy. A link fails when `to` exists. A `from` that cannot be removed after
/// the link leaves a twin that a sweep reaps; the blob has still moved.
fn move_blob(from: &Path, to: &Path) -> bool {
    let Some(dir) = to.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() || std::fs::hard_link(from, to).is_err() {
        return false;
    }
    let _ = std::fs::remove_file(from);
    true
}

/// Every file of a blob namespace in either layout: those directly in `blobs_dir` (flat) and those
/// inside its bucket directories. A missing `blobs_dir` yields none. A
/// bucket that vanishes while it is walked (a concurrent wipe) is skipped; any other read failure is
/// an error, so a sweep never decides on a partial listing.
pub fn blob_files(blobs_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
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
