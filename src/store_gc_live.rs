//! Liveness bookkeeping for the blob sweep: which tier references a blob, and the machine-wide lock
//! that serialises destructive sweeps. Split out of `store_gc.rs` to keep that file under the module
//! size cap; `store_gc` re-exports [`LiveBlobs`].

use std::path::Path;

use ahash::AHashSet;

use crate::store::{INDEX_FILE, StoreError, VIEWS_DIR};
use crate::store_gc::blob_stem;

/// The blob suffixes that belong to the CODE lane: produced by the code-map scan from a
/// [`crate::store::FileEntry`] and live iff some workspace's `files` map references the hash. The
/// remaining suffix (`.doc.msgpack`) belongs to the DOCUMENT lane and is live iff a `doc_files`
/// entry references it.
///
/// The lanes must be tracked apart because a path can change tier (a markdown file was code-mapped
/// before prose was routed to the document tier) while keeping the SAME content hash: its stale
/// `.fm` / `.chunk` / `.rref` blobs share a stem with the live `.doc` blob, so a stem-only liveness
/// test would keep the dead code-lane blobs forever.
pub(crate) const CODE_LANE_SUFFIXES: [&str; 3] = [".fm.msgpack", ".chunk.msgpack", ".rref.msgpack"];

/// Content hashes referenced by the indexes, split by the tier that references them. See
/// [`CODE_LANE_SUFFIXES`] for why a single union is not enough.
#[derive(Debug, Clone, Default)]
pub struct LiveBlobs {
    /// Hashes referenced by a `files` entry (code map): keeps `.fm` / `.chunk` / `.rref` blobs alive.
    pub code: AHashSet<String>,
    /// Hashes referenced by a `doc_files` entry (document tier): keeps `.doc` blobs alive.
    pub docs: AHashSet<String>,
}

impl LiveBlobs {
    /// True when `stem` is referenced by EITHER tier.
    pub fn contains(&self, stem: &str) -> bool {
        self.code.contains(stem) || self.docs.contains(stem)
    }

    /// True when the blob `file_name` (a recognised blob filename) is kept alive by the tier that
    /// produces its suffix. `None` stem means the name is not a blob at all.
    pub(crate) fn keeps(&self, file_name: &str) -> bool {
        if let Some(stem) = CODE_LANE_SUFFIXES
            .iter()
            .find_map(|suffix| file_name.strip_suffix(suffix))
        {
            return self.code.contains(stem);
        }
        blob_stem(file_name).is_some_and(|stem| self.docs.contains(stem))
    }

    /// True when neither tier references anything.
    pub fn is_empty(&self) -> bool {
        self.code.is_empty() && self.docs.is_empty()
    }

    /// Every referenced stem (a stem referenced by both tiers appears twice).
    pub fn stems(&self) -> impl Iterator<Item = &String> {
        self.code.iter().chain(self.docs.iter())
    }

    /// Fold another set in (cross-workspace union).
    pub fn extend(&mut self, other: LiveBlobs) {
        self.code.extend(other.code);
        self.docs.extend(other.docs);
    }
}

/// Machine-wide advisory lock serialising destructive global sweeps. Held for the sweep's duration
/// and released on drop (or process death, so a crash never wedges it).
pub(crate) struct SweepLock(std::fs::File);

impl SweepLock {
    const FILE: &'static str = "gc.lock";

    pub(crate) fn try_acquire(cache_dir: &Path) -> Option<Self> {
        use fs2::FileExt;
        std::fs::create_dir_all(cache_dir).ok()?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(cache_dir.join(Self::FILE))
            .ok()?;
        file.try_lock_exclusive().ok()?;
        Some(Self(file))
    }
}

impl Drop for SweepLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

/// The first release minor whose writer open moves memory notes out of a view's index into the
/// group store. Earlier releases kept them in the view's `index.fjall`.
const GROUP_MEMORY_SINCE_MINOR: u16 = 30;

/// Move into the group store the memory notes a release before 0.30 left in the index of each view of
/// `workspace`, before a collector removes that index. Only a writer open of the view moved them, so a
/// workspace not opened for writing since the upgrade still held them, and evicting or reaping it
/// destroyed notes a user wrote. Returns `false` when they could not be moved: the caller must then
/// leave the views alone. The caller holds the workspace lock.
pub(crate) fn rescue_legacy_memory(workspace: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(workspace.join(VIEWS_DIR)) else {
        return true;
    };
    for entry in entries.flatten() {
        let view_dir = entry.path();
        if !view_dir.join(crate::index::INDEX_DIR).is_dir() || written_since_group_memory(&view_dir) {
            continue;
        }
        let Some(marker) = crate::store_layout::read_workspace_marker(workspace) else {
            tracing::warn!(
                view = %view_dir.display(),
                "no workspace marker names the group of this view's memory notes; keeping it"
            );
            return false;
        };
        let memory_dir = crate::store_layout::memory_store_dir(&marker.root);
        if let Err(error) = crate::index::IndexDb::open_with_memory(&view_dir, &memory_dir) {
            tracing::warn!(
                view = %view_dir.display(),
                %error,
                "could not move this view's memory notes to the group store; keeping it"
            );
            return false;
        }
    }
    true
}

/// Whether the file map of `view_dir` was written by a release whose writer open, which runs before
/// the map is written, moves memory notes out of the view.
fn written_since_group_memory(view_dir: &Path) -> bool {
    #[derive(serde::Deserialize)]
    struct Version {
        schema_ver: u16,
    }
    std::fs::read(view_dir.join(INDEX_FILE))
        .ok()
        .and_then(|bytes| rmp_serde::from_slice::<Version>(&bytes).ok())
        .is_some_and(|version| version.schema_ver >= GROUP_MEMORY_SINCE_MINOR)
}

/// Whether `error` reading a file map marks a view the sweep passes over: an older schema or blob
/// layout, a map of this version that does not decode, or one whose version cannot be read.
pub(crate) fn is_rebuildable_view_error(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::SchemaMismatch { .. }
            | StoreError::StaleBlobLayout { .. }
            | StoreError::Decode(_)
            | StoreError::UnreadableFileMap { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(code: &[&str], docs: &[&str]) -> LiveBlobs {
        LiveBlobs {
            code: code.iter().map(|s| (*s).to_string()).collect(),
            docs: docs.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn each_suffix_is_kept_alive_only_by_its_own_tier() {
        let blobs = live(&["c"], &["d"]);
        for suffix in [".fm.msgpack", ".chunk.msgpack", ".rref.msgpack"] {
            assert!(blobs.keeps(&format!("c{suffix}")), "code tier keeps c{suffix}");
            assert!(!blobs.keeps(&format!("d{suffix}")), "doc tier does not keep d{suffix}");
        }
        assert!(blobs.keeps("d.doc.msgpack"));
        assert!(
            !blobs.keeps("c.doc.msgpack"),
            "a code reference does not keep a .doc blob"
        );
        assert!(!blobs.keeps("stray.txt"), "a non-blob name is never kept by reference");
    }

    #[test]
    fn extend_unions_both_tiers_independently() {
        let mut a = live(&["x"], &[]);
        a.extend(live(&[], &["x"]));
        assert!(a.keeps("x.fm.msgpack") && a.keeps("x.doc.msgpack"));
        assert!(!a.is_empty());
        assert_eq!(a.stems().count(), 2);
    }

    #[test]
    fn a_second_sweep_cannot_start_while_one_holds_the_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = SweepLock::try_acquire(dir.path()).expect("first sweep acquires");
        assert!(
            SweepLock::try_acquire(dir.path()).is_none(),
            "contended sweep is skipped, not queued"
        );
        drop(first);
        assert!(SweepLock::try_acquire(dir.path()).is_some(), "released on drop");
    }
}
