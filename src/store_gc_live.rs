//! Liveness bookkeeping for the blob sweep: which tier references a blob, and the machine-wide lock
//! that serialises destructive sweeps. Split out of `store_gc.rs` to keep that file under the module
//! size cap; `store_gc` re-exports [`LiveBlobs`].

use std::path::Path;

use ahash::AHashSet;

use crate::store::{INDEX_FILE, StoreError, VIEWS_DIR, acquire_lock, read_index};
use crate::store_gc::{GcError, blob_stem, collect_referenced_hashes, read_dir};

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

/// Collect one workspace's live hashes, healing view indexes that the current binary cannot read.
///
/// A schema-mismatched or corrupt `index.msgpack` is rebuildable cache state, not durable data. If
/// it remains in the global mark set it blocks every workspace's GC forever. Remove only the bad
/// view while holding the workspace lock, preserving `workspace.json`, `agent-id`, and durable
/// memory. Transient I/O and lock failures remain fatal so an incomplete live set never drives a
/// destructive sweep.
pub(crate) fn collect_workspace_hashes_healing_stale_views(workspace_dir: &Path) -> Result<LiveBlobs, GcError> {
    match collect_referenced_hashes(workspace_dir) {
        Ok(referenced) => Ok(referenced),
        Err(GcError::Store(error)) if is_rebuildable_view_error(&error) => {
            let _lock = acquire_lock(workspace_dir)?;
            let views_dir = workspace_dir.join(VIEWS_DIR);
            for entry in read_dir(&views_dir)? {
                let entry = entry.map_err(|source| GcError::Io {
                    path: views_dir.clone(),
                    source,
                })?;
                let view_dir = entry.path();
                if !view_dir.is_dir() || !view_dir.join(INDEX_FILE).exists() {
                    continue;
                }
                if let Err(error) = read_index(&view_dir) {
                    if !is_rebuildable_view_error(&error) {
                        return Err(GcError::Store(error));
                    }
                    std::fs::remove_dir_all(&view_dir).map_err(|source| GcError::Io {
                        path: view_dir.clone(),
                        source,
                    })?;
                    tracing::warn!(
                        workspace = %workspace_dir.display(),
                        view = %view_dir.display(),
                        %error,
                        "removed unreadable rebuildable view so global GC can continue"
                    );
                }
            }
            collect_referenced_hashes(workspace_dir)
        }
        Err(error) => Err(error),
    }
}

fn is_rebuildable_view_error(error: &StoreError) -> bool {
    matches!(error, StoreError::SchemaMismatch { .. } | StoreError::Decode(_))
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
