//! Free-space pre-check for the index volume.
//!
//! A scan that runs the disk dry does not fail cleanly: fjall's commits start returning
//! `StorageFull`, then `Poisoned`, and the half-written store used to be reported as complete
//! (issue #79). Refusing up front is cheap by comparison, so the scan and the resolve pass both
//! ask [`ensure_free_space`] first.
//!
//! The requirement is an estimate, not a measurement: a rescan rewrites most of the index, and
//! LSM compaction transiently holds old and new tables side by side, so it is
//! [`GROWTH_NUM`]/[`GROWTH_DEN`] times the current index size plus a fixed headroom. The headroom
//! defaults to [`DEFAULT_HEADROOM_MB`] and is overridden by [`MIN_FREE_ENV`].

use std::path::{Path, PathBuf};

/// Environment override for the fixed headroom, in MiB. `0` leaves only the size-proportional
/// estimate.
pub const MIN_FREE_ENV: &str = "BASEMIND_MIN_FREE_MB";
/// Headroom added to the size estimate when [`MIN_FREE_ENV`] is unset or unparsable.
pub const DEFAULT_HEADROOM_MB: u64 = 512;
/// The estimate is 1.5x the existing index size.
const GROWTH_NUM: u64 = 3;
const GROWTH_DEN: u64 = 2;

/// The index volume cannot hold the work the pass is about to do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "not enough free disk space for the index at {}: {} MiB free, about {} MiB needed \
     (1.5x the {} MiB index plus headroom; override the headroom with BASEMIND_MIN_FREE_MB=<MiB>)",
    .dir.display(), .free_bytes >> 20, .required_bytes >> 20, .index_bytes >> 20
)]
pub struct InsufficientSpace {
    pub dir: PathBuf,
    pub free_bytes: u64,
    pub required_bytes: u64,
    pub index_bytes: u64,
}

/// Headroom in bytes from the raw [`MIN_FREE_ENV`] value.
fn headroom_bytes(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_HEADROOM_MB)
        .saturating_mul(1 << 20)
}

/// Bytes a pass needs free given the index's current size and the headroom.
pub fn required_bytes(index_bytes: u64, headroom_bytes: u64) -> u64 {
    (index_bytes / GROWTH_DEN)
        .saturating_mul(GROWTH_NUM)
        .saturating_add(headroom_bytes)
}

/// Pure decision: `Err` when `free_bytes` is below the requirement. The seam the tests use.
pub fn evaluate(dir: &Path, index_bytes: u64, headroom_bytes: u64, free_bytes: u64) -> Result<(), InsufficientSpace> {
    let required_bytes = required_bytes(index_bytes, headroom_bytes);
    if free_bytes >= required_bytes {
        return Ok(());
    }
    Err(InsufficientSpace {
        dir: dir.to_path_buf(),
        free_bytes,
        required_bytes,
        index_bytes,
    })
}

/// Total size of the regular files under `dir`; zero when it is absent.
pub fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => dir_bytes(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |meta| meta.len()),
            _ => 0,
        })
        .sum()
}

/// Refuse when the volume holding `view_dir` lacks room for the index. An unreadable free-space
/// figure passes: a platform quirk must not block scans that would have worked.
pub fn ensure_free_space(view_dir: &Path) -> Result<(), InsufficientSpace> {
    let Ok(free) = fs2::available_space(nearest_existing(view_dir)) else {
        return Ok(());
    };
    let index_bytes = dir_bytes(&view_dir.join(crate::index::INDEX_DIR));
    let headroom = headroom_bytes(std::env::var(MIN_FREE_ENV).ok().as_deref());
    evaluate(view_dir, index_bytes, headroom, free)
}

/// `path` or its closest existing ancestor, since the view dir may not be created yet.
fn nearest_existing(path: &Path) -> &Path {
    path.ancestors().find(|p| p.exists()).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    #[test]
    fn requirement_is_one_and_a_half_times_the_index_plus_headroom() {
        assert_eq!(required_bytes(100 * MIB, 512 * MIB), 150 * MIB + 512 * MIB);
        assert_eq!(required_bytes(0, 512 * MIB), 512 * MIB);
    }

    #[test]
    fn refuses_below_the_threshold_and_names_the_numbers() {
        let err = evaluate(Path::new("/idx"), 100 * MIB, 512 * MIB, 600 * MIB).expect_err("600 < 662 MiB");
        assert_eq!(err.free_bytes, 600 * MIB);
        assert_eq!(err.required_bytes, 662 * MIB);
        assert!(err.to_string().contains(MIN_FREE_ENV));
    }

    #[test]
    fn allows_at_or_above_the_threshold() {
        assert!(evaluate(Path::new("/idx"), 100 * MIB, 512 * MIB, 662 * MIB).is_ok());
    }

    #[test]
    fn env_override_replaces_the_default_headroom() {
        assert_eq!(headroom_bytes(None), DEFAULT_HEADROOM_MB * MIB);
        assert_eq!(headroom_bytes(Some(" 64 ")), 64 * MIB);
        assert_eq!(headroom_bytes(Some("0")), 0);
        assert_eq!(headroom_bytes(Some("lots")), DEFAULT_HEADROOM_MB * MIB);
    }

    #[test]
    fn dir_bytes_sums_nested_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("a"), [0u8; 10]).expect("write");
        std::fs::write(dir.path().join("sub").join("b"), [0u8; 5]).expect("write");
        assert_eq!(dir_bytes(dir.path()), 15);
        assert_eq!(dir_bytes(&dir.path().join("missing")), 0);
    }

    #[test]
    fn a_not_yet_created_view_dir_resolves_to_its_existing_ancestor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("a").join("b");
        assert_eq!(nearest_existing(&missing), dir.path());
    }
}
