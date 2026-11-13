//! Cross-process leases and conservative retention for immutable base generations.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use fs2::FileExt as _;

const BASE: &str = "base";
const LEASES: &str = ".leases";

/// Keep the lease alive for the entire lifetime of a generation handle and its snapshots.
#[derive(Debug)]
pub(crate) struct GenerationLease {
    path: PathBuf,
    _lock: File,
}

impl GenerationLease {
    pub(crate) fn open(group: &Path, generation: u64) -> io::Result<Self> {
        let gate = group_lock(group)?;
        fs2::FileExt::lock_shared(&gate)?;
        let path = generation_path(group, generation);
        if !fs::symlink_metadata(&path)?.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "generation is not a directory",
            ));
        }
        let lock = generation_lock(group, generation)?;
        fs2::FileExt::lock_shared(&lock)?;
        Ok(Self { path, _lock: lock })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RetiredGenerations {
    pub(crate) removed: Vec<u64>,
    pub(crate) active: Vec<u64>,
}

/// Remove candidates that have neither a durable overlay pin nor a live reader.
///
/// The caller selects retired candidates, preserving the newest compatible generation.
/// Read durable pins in the callback. Overlay pin creation and replacement must take the
/// same exclusive group lock before publishing metadata. An error reading pins prevents GC.
pub(crate) fn retire_unpinned(
    group: &Path,
    candidates: &BTreeSet<u64>,
    read_pins: impl FnOnce() -> io::Result<BTreeSet<u64>>,
) -> io::Result<RetiredGenerations> {
    let gate = group_lock(group)?;
    gate.lock_exclusive()?;
    let pins = read_pins()?;
    let mut result = RetiredGenerations::default();
    let mut pending = Vec::new();
    for generation in candidates.difference(&pins).copied() {
        let lease = generation_lock(group, generation)?;
        match lease.try_lock_exclusive() {
            Ok(()) => {}
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
            {
                result.active.push(generation);
                continue;
            }
            Err(error) => return Err(error),
        }
        if let Some(path) = mark_retired(group, generation)? {
            pending.push((generation, path, lease));
        }
    }
    drop(gate);
    for (generation, path, _lease) in pending {
        fs::remove_dir_all(path)?;
        result.removed.push(generation);
    }
    Ok(result)
}

fn mark_retired(group: &Path, generation: u64) -> io::Result<Option<PathBuf>> {
    let path = generation_path(group, generation);
    let retired = group.join(BASE).join(format!(".retired-{generation}"));
    if directory_exists(&path)? {
        fs::rename(path, &retired)?;
        Ok(Some(retired))
    } else {
        Ok(directory_exists(&retired)?.then_some(retired))
    }
}

fn directory_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "generation is not a directory",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Builder must include this watermark in ID selection under the exclusive group lock.
pub(crate) fn highest_leased_generation(group: &Path) -> io::Result<u64> {
    let entries = match fs::read_dir(group.join(BASE).join(LEASES)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut highest = 0;
    for entry in entries {
        let entry = entry?;
        if let Some(id) = entry.file_name().to_str().and_then(|name| name.strip_prefix("gen-"))
            && let Ok(id) = id.parse::<u64>()
        {
            highest = highest.max(id);
        }
    }
    Ok(highest)
}

pub(crate) fn group_lock(group: &Path) -> io::Result<File> {
    open_lock(&group.join("base.lock"))
}

fn generation_lock(group: &Path, generation: u64) -> io::Result<File> {
    let dir = group.join(BASE).join(LEASES);
    fs::create_dir_all(&dir)?;
    // Never unlink leases. Their identities must remain stable across competing processes.
    open_lock(&dir.join(format!("gen-{generation}")))
}

fn open_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn generation_path(group: &Path, generation: u64) -> PathBuf {
    group.join(BASE).join(format!("gen-{generation}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_durable_pins_and_live_readers_then_collects_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        let group = dir.path();
        for id in [1, 2, 3, 4] {
            fs::create_dir_all(generation_path(group, id)).unwrap();
        }
        let reader = GenerationLease::open(group, 2).unwrap();
        let candidates = BTreeSet::from([1, 2, 3]);
        let retired = retire_unpinned(group, &candidates, || Ok(BTreeSet::from([1]))).unwrap();
        assert_eq!(retired.removed, [3]);
        assert_eq!(retired.active, [2]);
        assert!(reader.path().is_dir());
        assert!(generation_path(group, 1).is_dir());
        assert!(generation_path(group, 4).is_dir());
        drop(reader);
        assert_eq!(
            retire_unpinned(group, &candidates, || Ok(BTreeSet::from([1])))
                .unwrap()
                .removed,
            [2]
        );
        assert!(GenerationLease::open(group, 2).is_err());
        assert_eq!(highest_leased_generation(group).unwrap(), 3);
    }

    #[test]
    fn unreadable_pins_block_collection_and_retired_cleanup_is_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let group = dir.path();
        fs::create_dir_all(generation_path(group, 1)).unwrap();
        let retired = group.join(BASE).join(".retired-2");
        fs::create_dir(&retired).unwrap();
        let candidates = BTreeSet::from([1, 2]);
        assert!(retire_unpinned(group, &candidates, || Err(io::ErrorKind::InvalidData.into())).is_err());
        assert!(generation_path(group, 1).is_dir());
        assert!(retired.is_dir());
        assert_eq!(
            retire_unpinned(group, &candidates, || Ok(BTreeSet::new()))
                .unwrap()
                .removed,
            [1, 2]
        );
    }
}
