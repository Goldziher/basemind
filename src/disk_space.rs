//! Free-space pre-check for the index volume.
//!
//! A scan that runs the disk dry does not fail cleanly: fjall's commits start returning
//! `StorageFull`, then `Poisoned`, and the half-written store used to be reported as complete
//! (issue #79). Refusing up front is cheap by comparison, so a full scan asks
//! [`ensure_scan_space`] once its candidates are known and before it extracts anything, and the
//! resolve pass asks [`ensure_free_space`].
//!
//! The requirement is an estimate, not a measurement ([`SpaceEstimate`]): a rescan rewrites most of
//! the index, and LSM compaction transiently holds old and new tables side by side, so the existing
//! index counts 1.5 times; every source byte no file map lists yet adds index rows, blobs and, when
//! code is embedded, vectors, at the per-source-byte ratios below; and a fixed headroom, which
//! defaults to [`DEFAULT_HEADROOM_MB`] and is overridden by [`MIN_FREE_ENV`], comes on top. The
//! ratios are rounded up from what a full scan of a monorepo with ~600 MiB of source left on disk
//! (3.4x index, 2.3x blobs, 1.35x vectors), so a first scan, or the first after a release-minor
//! bump, which starts from an empty index, is charged for what it writes.
//!
//! A refusal names what an older release left on the volume ([`Reclaimable`]); it is gathered only
//! when the check refuses, because it walks the blob store.

use std::fmt;
use std::path::{Path, PathBuf};

/// Environment override for the fixed headroom, in MiB. `0` leaves only the size-proportional
/// estimate.
pub const MIN_FREE_ENV: &str = "BASEMIND_MIN_FREE_MB";
/// Headroom added to the size estimate when [`MIN_FREE_ENV`] is unset or unparsable.
pub const DEFAULT_HEADROOM_MB: u64 = 512;

/// A ratio of whole numbers, applied to a byte count without overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ratio {
    /// Numerator.
    pub num: u64,
    /// Denominator; never zero.
    pub den: u64,
}

impl Ratio {
    /// `bytes` scaled by the ratio, rounded down, saturating.
    pub fn of(self, bytes: u64) -> u64 {
        (bytes / self.den).saturating_mul(self.num)
    }
}

/// `1.5x`: the ratio as a multiplier with one decimal.
impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tenths = self.num.saturating_mul(10) / self.den;
        write!(f, "{}.{}x", tenths / 10, tenths % 10)
    }
}

/// The existing index counts 1.5 times: a rescan rewrites it while compaction holds both copies.
pub const INDEX_REWRITE: Ratio = Ratio { num: 3, den: 2 };
/// Index bytes per new source byte.
pub const INDEX_PER_SOURCE: Ratio = Ratio { num: 7, den: 2 };
/// Blob bytes per new source byte.
pub const BLOBS_PER_SOURCE: Ratio = Ratio { num: 5, den: 2 };
/// Vector bytes per new source byte, when code is embedded.
pub const VECTORS_PER_SOURCE: Ratio = Ratio { num: 3, den: 2 };

/// What a pass is expected to write, from which [`SpaceEstimate::required_bytes`] follows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpaceEstimate {
    /// Bytes of the group index on disk now.
    pub index_bytes: u64,
    /// Source bytes the pass extracts that no file map lists yet: working-tree files
    /// ([`new_source_bytes`]) or git blobs ([`capped_bytes`]), as the scan's source reads them.
    pub source_bytes: u64,
    /// Whether the pass writes code vectors.
    pub vectors: bool,
    /// The fixed headroom ([`MIN_FREE_ENV`]).
    pub headroom_bytes: u64,
}

impl SpaceEstimate {
    /// Index growth: the rewrite of the existing index plus the rows of the new source.
    pub fn index_growth(&self) -> u64 {
        INDEX_REWRITE
            .of(self.index_bytes)
            .saturating_add(INDEX_PER_SOURCE.of(self.source_bytes))
    }

    /// Blob growth of the new source.
    pub fn blob_growth(&self) -> u64 {
        BLOBS_PER_SOURCE.of(self.source_bytes)
    }

    /// Vector growth of the new source; zero when no code is embedded.
    pub fn vector_growth(&self) -> u64 {
        if self.vectors {
            VECTORS_PER_SOURCE.of(self.source_bytes)
        } else {
            0
        }
    }

    /// Bytes the pass needs free: every growth plus the headroom.
    pub fn required_bytes(&self) -> u64 {
        self.index_growth()
            .saturating_add(self.blob_growth())
            .saturating_add(self.vector_growth())
            .saturating_add(self.headroom_bytes)
    }
}

/// Space an older release left on the volume, and how to get it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaimable {
    /// What it is.
    pub what: String,
    /// Where it is.
    pub path: PathBuf,
    /// Its size.
    pub bytes: u64,
    /// How it is removed.
    pub remedy: &'static str,
}

/// The index volume cannot hold the work the pass is about to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsufficientSpace {
    /// The group index directory whose volume was checked.
    pub dir: PathBuf,
    /// Bytes free on that volume.
    pub free_bytes: u64,
    /// Bytes the pass needs free ([`SpaceEstimate::required_bytes`]).
    pub required_bytes: u64,
    /// What the requirement is made of.
    pub estimate: SpaceEstimate,
    /// What an older release left on the volume.
    pub reclaimable: Vec<Reclaimable>,
}

impl fmt::Display for InsufficientSpace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let estimate = &self.estimate;
        write!(
            f,
            "not enough free disk space for the index at {}: {} free, about {} needed ({} index: \
             {INDEX_REWRITE} the {} index plus {INDEX_PER_SOURCE} the {} of new source; {} blobs: \
             {BLOBS_PER_SOURCE} the new source; {} vectors: {VECTORS_PER_SOURCE} the new source when \
             code is embedded; {} headroom, set with {MIN_FREE_ENV}=<MiB>)",
            self.dir.display(),
            mib(self.free_bytes),
            mib(self.required_bytes),
            mib(estimate.index_growth()),
            mib(estimate.index_bytes),
            mib(estimate.source_bytes),
            mib(estimate.blob_growth()),
            mib(estimate.vector_growth()),
            mib(estimate.headroom_bytes),
        )?;
        if self.reclaimable.is_empty() {
            return write!(f, ". Free space on that volume and scan again");
        }
        write!(f, ". To free space:")?;
        for item in &self.reclaimable {
            write!(
                f,
                " {} of {} at {} ({});",
                mib(item.bytes),
                item.what,
                item.path.display(),
                item.remedy
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for InsufficientSpace {}

/// `bytes` in whole MiB, rounded up so a non-zero figure never reads as zero.
fn mib(bytes: u64) -> String {
    format!("{} MiB", bytes.div_ceil(1 << 20))
}

/// Headroom in bytes from the raw [`MIN_FREE_ENV`] value.
pub fn headroom_bytes(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_HEADROOM_MB)
        .saturating_mul(1 << 20)
}

/// Pure decision: `Err` when `free_bytes` is below the estimate's requirement. `leftovers` runs
/// only on a refusal. The seam the tests use.
pub fn evaluate(
    dir: &Path,
    estimate: SpaceEstimate,
    free_bytes: u64,
    leftovers: impl FnOnce() -> Vec<Reclaimable>,
) -> Result<(), InsufficientSpace> {
    let required_bytes = estimate.required_bytes();
    if free_bytes >= required_bytes {
        return Ok(());
    }
    Err(InsufficientSpace {
        dir: dir.to_path_buf(),
        free_bytes,
        required_bytes,
        estimate,
        reclaimable: leftovers(),
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

/// Bytes of the files `rels` name under `root`, each at most `max_file_bytes` (a larger one is
/// skipped by the scan, so it costs nothing). A path that cannot be read counts zero. The caller
/// passes the candidates no file map lists, so a rescan stats only what is new.
pub fn new_source_bytes<'a>(root: &Path, rels: impl IntoIterator<Item = &'a str>, max_file_bytes: u64) -> u64 {
    capped_bytes(
        rels.into_iter()
            .filter_map(|rel| std::fs::metadata(root.join(rel)).ok())
            .filter(std::fs::Metadata::is_file)
            .map(|meta| meta.len()),
        max_file_bytes,
    )
}

/// The sum of `sizes`, each at most `max_file_bytes` (a larger file is skipped by the scan, so it
/// costs nothing). A git-backed scan sizes its new candidates from their blobs and sums them here.
pub fn capped_bytes(sizes: impl IntoIterator<Item = u64>, max_file_bytes: u64) -> u64 {
    sizes.into_iter().filter(|size| *size <= max_file_bytes).sum()
}

/// What older releases left in the cache at `cache_dir`: the blob namespaces of a schema below
/// `schema`, and the per-view indexes the group index replaced, summed over every workspace.
pub fn reclaimable(cache_dir: &Path, schema: u16) -> Vec<Reclaimable> {
    let mut found: Vec<Reclaimable> =
        superseded_blob_namespaces(&cache_dir.join(crate::store_layout::BLOBS_DIR), schema)
            .into_iter()
            .map(|(path, bytes)| Reclaimable {
                what: "blobs of an older release".to_string(),
                path,
                bytes,
                remedy: "`basemind cache gc` removes them",
            })
            .collect();
    let workspaces = cache_dir.join(crate::store_layout::WORKSPACES_DIR);
    let (count, bytes) = legacy_view_indexes(&workspaces);
    if bytes > 0 {
        found.push(Reclaimable {
            what: format!("per-view indexes of an older release in {count} workspaces"),
            path: workspaces,
            bytes,
            remedy: "each goes when basemind next opens its worktree for writing",
        });
    }
    found
}

/// `(path, bytes)` of every non-empty `v<N>` namespace under `blobs_root` with `N < schema`.
fn superseded_blob_namespaces(blobs_root: &Path, schema: u16) -> Vec<(PathBuf, u64)> {
    let Ok(entries) = std::fs::read_dir(blobs_root) else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, u64)> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(crate::store_gc_namespaces::namespace_schema)
                .is_some_and(|ver| ver < schema)
        })
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .map(|path| {
            let bytes = dir_bytes(&path);
            (path, bytes)
        })
        .filter(|(_, bytes)| *bytes > 0)
        .collect();
    found.sort();
    found
}

/// How many `<workspace>/views/<view>/index.fjall` directories exist under `workspaces`, and their
/// total size.
fn legacy_view_indexes(workspaces: &Path) -> (usize, u64) {
    let Ok(entries) = std::fs::read_dir(workspaces) else {
        return (0, 0);
    };
    entries
        .flatten()
        .filter_map(|workspace| std::fs::read_dir(workspace.path().join(crate::store_layout::VIEWS_DIR)).ok())
        .flatten()
        .flatten()
        .map(|view| view.path().join(crate::index::INDEX_DIR))
        .filter(|index| index.is_dir())
        .fold((0, 0), |(count, bytes), index| (count + 1, bytes + dir_bytes(&index)))
}

/// Refuse a full scan whose `estimate` does not fit the volume holding the group index under
/// `group_dir`. An unreadable free-space figure passes: a platform quirk must not block scans that
/// would have worked.
pub fn ensure_scan_space(group_dir: &Path, estimate: SpaceEstimate) -> Result<(), InsufficientSpace> {
    let Some(free) = available_bytes(group_dir) else {
        return Ok(());
    };
    evaluate(group_dir, estimate, free, || {
        reclaimable(
            &crate::data_root::DataRoot::current().cache_dir(),
            crate::extract::SCHEMA_VER,
        )
    })
}

/// The estimate of a pass that adds no new source: [`ensure_scan_space`] over the group index's
/// current size and the configured headroom. The resolve pass asks this.
pub fn ensure_free_space(group_dir: &Path) -> Result<(), InsufficientSpace> {
    ensure_scan_space(group_dir, current_estimate(group_dir, 0, false))
}

/// A [`SpaceEstimate`] for `group_dir` as it is on disk now, under the configured headroom.
pub fn current_estimate(group_dir: &Path, source_bytes: u64, vectors: bool) -> SpaceEstimate {
    SpaceEstimate {
        index_bytes: dir_bytes(&group_dir.join(crate::index::INDEX_DIR)),
        source_bytes,
        vectors,
        headroom_bytes: crate::config::engine::with(|config| config.min_free_bytes),
    }
}

#[cfg(test)]
thread_local! {
    static FAKE_FREE_BYTES: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Make this thread's checks see `free` bytes free instead of asking the volume; `None` restores
/// the volume.
#[cfg(test)]
pub(crate) fn fake_free_bytes(free: Option<u64>) {
    FAKE_FREE_BYTES.set(free);
}

/// Free bytes on the volume holding `path`, or `None` when the platform cannot say.
pub fn available_bytes(path: &Path) -> Option<u64> {
    #[cfg(test)]
    if let Some(free) = FAKE_FREE_BYTES.get() {
        return Some(free);
    }
    fs2::available_space(nearest_existing(path)).ok()
}

/// `path` or its closest existing ancestor, since the group dir may not be created yet.
fn nearest_existing(path: &Path) -> &Path {
    path.ancestors().find(|p| p.exists()).unwrap_or(path)
}

#[cfg(test)]
#[path = "disk_space_tests.rs"]
mod tests;
