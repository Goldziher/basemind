//! Orphaned-workspace reaper for the machine-global cache.
//!
//! Per-workspace cache state lives at `cache_root()/cache/workspaces/<workspace_key>/`, where the
//! key is a **one-way** blake3 hash of the canonicalized worktree root. Blobs, by contrast, live in
//! ONE machine-global content-addressed store shared by every workspace, and the daemon's blob GC
//! ([`crate::store_gc::gc_global_blobs`]) reference-counts a blob against *every* workspace dir it
//! finds — so a workspace dir left behind by a deleted repo / worktree / test temp dir keeps voting
//! in that live set and **pins its blobs forever**. The cache then grows monotonically and can never
//! shrink.
//!
//! This module closes that leak: it reads the [`crate::store::WorkspaceMarker`] each workspace dir
//! records (the canonical root it was keyed from — the missing piece that makes an orphan
//! *detectable* at all) and removes the dirs whose root no longer exists on disk. The daemon runs it
//! immediately before the blob sweep, under the same write guard, so the blobs a reaped workspace was
//! pinning are reclaimed by that very sweep.
//!
//! The policy is deliberately conservative — a wrong delete here destroys a live index:
//!
//! | Workspace dir | Action |
//! |---|---|
//! | marker present, recorded root missing | reap (`remove_dir_all`) |
//! | marker present, recorded root exists | keep |
//! | marker **absent** (pre-0.23 legacy dir) | **keep** — unverifiable; it self-heals on next open |
//! | `.lock` held by another process | skip — never reap a workspace someone is using |
//!
//! Split into its own module (mirroring `store_lock.rs`) to keep `store_gc.rs` under the module
//! size cap; the entry points are re-exported from [`crate::store_gc`].

use std::path::Path;

use serde::Serialize;

use crate::store::{CACHE_DIR, WORKSPACES_DIR, acquire_lock, cache_root, read_workspace_marker};
use crate::store_gc::{GcError, dir_size};

/// Result of an orphaned-workspace reap sweep. Every inspected dir lands in exactly one of the
/// four outcome counters (`reaped` + `kept_live` + `kept_unverifiable` + `skipped_locked` ==
/// `scanned`), so an operator can tell "nothing to reap" apart from "nothing was verifiable".
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReapReport {
    /// Workspace directories inspected.
    pub scanned: usize,
    /// Orphaned workspace directories removed (marker present, recorded root gone).
    pub reaped: usize,
    /// Kept because the recorded root still exists.
    pub kept_live: usize,
    /// Kept because the dir carries no marker, so its root cannot be verified (legacy dirs).
    pub kept_unverifiable: usize,
    /// Skipped because another process holds the workspace `.lock`.
    pub skipped_locked: usize,
    /// Bytes reclaimed by the removals (on-disk size, stat'd before deletion).
    pub bytes_freed: u64,
}

/// Reap orphaned workspace cache dirs under the machine-global `cache/workspaces/`.
///
/// Destructive; see the module docs for the (conservative) policy. Only the daemon calls this, from
/// inside its blob-GC write guard, so no scan is writing to a workspace mid-reap.
pub fn reap_orphaned_workspaces() -> Result<ReapReport, GcError> {
    reap_orphaned_workspaces_in(&cache_root().join(CACHE_DIR).join(WORKSPACES_DIR))
}

/// [`reap_orphaned_workspaces`] against an explicit workspaces directory. Production passes the
/// global `cache/workspaces`; unit tests pass a per-test temp dir so they never mutate (nor race on)
/// the machine-global cache. Mirrors the `*_in` seam convention in [`crate::store_gc`].
pub(crate) fn reap_orphaned_workspaces_in(workspaces_dir: &Path) -> Result<ReapReport, GcError> {
    let mut report = ReapReport::default();
    if !workspaces_dir.exists() {
        return Ok(report);
    }
    let entries = std::fs::read_dir(workspaces_dir).map_err(|source| GcError::Io {
        path: workspaces_dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| GcError::Io {
            path: workspaces_dir.to_path_buf(),
            source,
        })?;
        let workspace_dir = entry.path();
        if !workspace_dir.is_dir() {
            continue;
        }
        report.scanned += 1;

        let Some(marker) = read_workspace_marker(&workspace_dir) else {
            // Legacy dir (or an unreadable marker): the dir name is a one-way hash, so we cannot
            // prove it is an orphan. Never delete what we cannot verify — it acquires a marker the
            // next time its store is opened, and becomes reapable then.
            report.kept_unverifiable += 1;
            tracing::debug!(
                workspace = %workspace_dir.display(),
                "workspace cache has no root marker; keeping (unverifiable, self-heals on next open)"
            );
            continue;
        };
        if marker.root.exists() {
            report.kept_live += 1;
            continue;
        }
        // The root does not stat — but "deleted" and "not mounted right now" look identical from
        // here. A repo on an external disk or a network share vanishes wholesale when the volume
        // detaches, and reaping on that signal would destroy a live workspace's index because
        // someone unplugged a drive. A deleted repo leaves its PARENT behind (`rm -rf ~/old` keeps
        // `~/`); a detached volume takes the parent with it. So require the parent to still exist
        // before believing the root is genuinely gone.
        if marker.root.parent().is_some_and(|parent| !parent.exists()) {
            report.kept_unverifiable += 1;
            tracing::debug!(
                workspace = %workspace_dir.display(),
                root = %marker.root.display(),
                "workspace root AND its parent are both missing (unmounted volume?); keeping — a \
                 detached disk must not be mistaken for a deleted repo"
            );
            continue;
        }

        // The recorded root is gone. Take the workspace's own advisory lock before touching it: a
        // live holder (scan / serve / the daemon's hot pool) means someone is still using this dir
        // even though the root momentarily fails to stat — skip it and retry on the next sweep.
        let Ok(lock) = acquire_lock(&workspace_dir) else {
            report.skipped_locked += 1;
            tracing::debug!(
                workspace = %workspace_dir.display(),
                "orphaned workspace cache is locked by another process; skipping this sweep"
            );
            continue;
        };
        let bytes = dir_size(&workspace_dir)?;
        std::fs::remove_dir_all(&workspace_dir).map_err(|source| GcError::Io {
            path: workspace_dir.clone(),
            source,
        })?;
        drop(lock);
        report.reaped += 1;
        report.bytes_freed += bytes;
        tracing::info!(
            workspace = %workspace_dir.display(),
            root = %marker.root.display(),
            bytes,
            "reaped orphaned workspace cache (its worktree root no longer exists)"
        );
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{FileEntry, INDEX_FILE, Index, VIEWS_DIR, ensure_workspace_marker};
    use crate::store_gc::{collect_referenced_hashes_global_in, gc_global_blobs_in};
    use std::fs;
    use std::path::PathBuf;

    /// Seed `<workspaces>/<key>/views/working/index.msgpack` referencing `stems`, and — when
    /// `root` is `Some` — the `workspace.json` marker recording that root. `None` reproduces a
    /// pre-0.23 legacy dir (no marker).
    fn seed_workspace(workspaces_dir: &Path, key: &str, root: Option<&Path>, stems: &[&str]) -> PathBuf {
        let workspace_dir = workspaces_dir.join(key);
        let working = workspace_dir.join(VIEWS_DIR).join("working");
        fs::create_dir_all(&working).expect("mk workspace view");
        let mut index = Index::empty();
        for (i, stem) in stems.iter().enumerate() {
            index.files.insert(
                crate::path::RelPath::from(format!("src/f{i}.rs").as_str()),
                FileEntry {
                    hash_hex: (*stem).to_string(),
                    language: "rust".to_string(),
                    size_bytes: 2,
                    mtime: 0,
                },
            );
        }
        let bytes = rmp_serde::to_vec_named(&index).expect("encode index");
        fs::write(working.join(INDEX_FILE), bytes).expect("write index");
        if let Some(root) = root {
            ensure_workspace_marker(&workspace_dir, root);
            assert!(
                read_workspace_marker(&workspace_dir).is_some(),
                "fixture must land a readable marker"
            );
        }
        workspace_dir
    }

    /// Create a worktree root that exists on disk (the thing a workspace dir is keyed from).
    fn make_root(tmp: &Path, name: &str) -> PathBuf {
        let root = tmp.join(name);
        fs::create_dir_all(root.join("src")).expect("mk root");
        root
    }

    #[test]
    fn an_orphaned_workspace_whose_root_is_gone_is_reaped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let live_root = make_root(tmp.path(), "live-repo");
        let dead_root = make_root(tmp.path(), "dead-repo");

        let live_ws = seed_workspace(&workspaces, "key-live", Some(&live_root), &["a".repeat(64).as_str()]);
        let dead_ws = seed_workspace(&workspaces, "key-dead", Some(&dead_root), &["b".repeat(64).as_str()]);

        fs::remove_dir_all(&dead_root).expect("delete the orphan's root");

        let report = reap_orphaned_workspaces_in(&workspaces).expect("reap");

        assert_eq!(report.scanned, 2, "both workspace dirs inspected");
        assert_eq!(report.reaped, 1, "exactly the orphan reaped");
        assert_eq!(report.kept_live, 1, "the live workspace is kept");
        assert_eq!(report.kept_unverifiable, 0, "both dirs carry markers");
        assert!(report.bytes_freed > 0, "the reaped tree's bytes are accounted");
        assert!(!dead_ws.exists(), "orphaned workspace dir removed");
        assert!(live_ws.exists(), "live workspace dir untouched");
    }

    #[test]
    fn a_workspace_on_an_unmounted_volume_is_never_reaped() {
        // A detached external disk / network share makes its repos vanish exactly like a deleted
        // repo does — except the PARENT vanishes too. Reaping on that signal would destroy a live
        // index because someone unplugged a drive, so it must be kept as unverifiable.
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let volume = tmp.path().join("Volumes").join("external");
        let root = volume.join("repo");
        fs::create_dir_all(root.join("src")).expect("mk root on the volume");

        let workspace = seed_workspace(&workspaces, "key-ext", Some(&root), &["c".repeat(64).as_str()]);

        // Detach the whole volume — root AND its parent disappear together.
        fs::remove_dir_all(tmp.path().join("Volumes")).expect("unmount the volume");

        let report = reap_orphaned_workspaces_in(&workspaces).expect("reap");

        assert_eq!(report.reaped, 0, "an unmounted volume must never be reaped");
        assert_eq!(report.kept_unverifiable, 1, "it is kept as unverifiable");
        assert!(workspace.exists(), "the workspace index survives the unmount");
    }

    #[test]
    fn a_workspace_with_a_live_root_is_never_reaped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let root = make_root(tmp.path(), "live-repo");
        let workspace = seed_workspace(&workspaces, "key-live", Some(&root), &["a".repeat(64).as_str()]);

        let report = reap_orphaned_workspaces_in(&workspaces).expect("reap");

        assert_eq!(report.reaped, 0, "a live root must never be reaped");
        assert_eq!(report.kept_live, 1);
        assert_eq!(report.bytes_freed, 0);
        assert!(workspace.join(VIEWS_DIR).join("working").join(INDEX_FILE).exists());
    }

    #[test]
    fn a_legacy_workspace_without_a_marker_is_never_reaped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        // No marker, and no root anywhere on disk — the *most* orphan-looking dir there is. The
        // conservative policy still keeps it: an unverifiable dir is never deleted.
        let legacy = seed_workspace(&workspaces, "key-legacy", None, &["a".repeat(64).as_str()]);

        let report = reap_orphaned_workspaces_in(&workspaces).expect("reap");

        assert_eq!(report.scanned, 1);
        assert_eq!(report.reaped, 0, "a dir we cannot verify must never be deleted");
        assert_eq!(report.kept_unverifiable, 1, "and it is reported as unverifiable");
        assert!(legacy.exists(), "legacy workspace dir survives");
        assert!(
            legacy.join(VIEWS_DIR).join("working").join(INDEX_FILE).exists(),
            "its index survives intact"
        );
    }

    /// The money test: an orphaned workspace pins its blobs in the machine-global store forever,
    /// because it keeps voting in the GC's cross-workspace live set. Reaping it must release them.
    #[test]
    fn reaping_an_orphaned_workspace_releases_the_blobs_it_pinned() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let blobs = tmp.path().join("blobs");
        fs::create_dir_all(&blobs).expect("mk blobs");

        let live_stem = "a".repeat(64);
        let pinned_stem = "b".repeat(64); // referenced ONLY by the orphaned workspace
        let pinned_bytes = b"blob-pinned-by-the-orphan";
        fs::write(blobs.join(format!("{live_stem}.fm.msgpack")), b"fm-live").expect("live blob");
        fs::write(blobs.join(format!("{pinned_stem}.fm.msgpack")), pinned_bytes).expect("pinned blob");

        let live_root = make_root(tmp.path(), "live-repo");
        let dead_root = make_root(tmp.path(), "dead-repo");
        seed_workspace(&workspaces, "key-live", Some(&live_root), &[live_stem.as_str()]);
        let dead_ws = seed_workspace(&workspaces, "key-dead", Some(&dead_root), &[pinned_stem.as_str()]);
        fs::remove_dir_all(&dead_root).expect("delete the orphan's root");

        // BEFORE the reap: the orphan still votes, so the blob it alone references is "live" and
        // the global sweep keeps it. This is the leak.
        let before = gc_global_blobs_in(&workspaces, &blobs).expect("gc before");
        assert_eq!(before.removed, 0, "nothing reclaimable while the orphan pins the blob");
        assert!(
            blobs.join(format!("{pinned_stem}.fm.msgpack")).exists(),
            "the orphan pins its blob against the pre-reap sweep"
        );

        let reap = reap_orphaned_workspaces_in(&workspaces).expect("reap");
        assert_eq!(reap.reaped, 1, "the orphan is reaped");
        assert!(!dead_ws.exists());

        // AFTER the reap: nothing references the blob, so the same sweep reclaims it.
        let referenced = collect_referenced_hashes_global_in(&workspaces).expect("union");
        assert!(
            !referenced.contains(&pinned_stem),
            "the reaped workspace no longer votes in the live set"
        );
        let after = gc_global_blobs_in(&workspaces, &blobs).expect("gc after");
        assert_eq!(after.removed, 1, "the previously-pinned blob is reclaimed");
        assert_eq!(after.bytes_freed, pinned_bytes.len() as u64);
        assert!(
            !blobs.join(format!("{pinned_stem}.fm.msgpack")).exists(),
            "the leaked blob is finally gone"
        );
        assert!(
            blobs.join(format!("{live_stem}.fm.msgpack")).exists(),
            "the live workspace's blob survives"
        );
    }

    #[test]
    fn a_blob_shared_with_a_live_workspace_survives_the_reap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let blobs = tmp.path().join("blobs");
        fs::create_dir_all(&blobs).expect("mk blobs");

        let shared_stem = "a".repeat(64); // referenced by BOTH workspaces (content-addressed dedup)
        fs::write(blobs.join(format!("{shared_stem}.fm.msgpack")), b"fm-shared").expect("shared blob");

        let live_root = make_root(tmp.path(), "live-repo");
        let dead_root = make_root(tmp.path(), "dead-repo");
        seed_workspace(&workspaces, "key-live", Some(&live_root), &[shared_stem.as_str()]);
        seed_workspace(&workspaces, "key-dead", Some(&dead_root), &[shared_stem.as_str()]);
        fs::remove_dir_all(&dead_root).expect("delete the orphan's root");

        assert_eq!(reap_orphaned_workspaces_in(&workspaces).expect("reap").reaped, 1);

        let report = gc_global_blobs_in(&workspaces, &blobs).expect("gc");
        assert_eq!(report.removed, 0, "the live workspace still references the blob");
        assert!(
            blobs.join(format!("{shared_stem}.fm.msgpack")).exists(),
            "a blob shared with a live workspace must never be over-reaped"
        );
    }

    #[test]
    fn a_locked_orphaned_workspace_is_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let dead_root = make_root(tmp.path(), "dead-repo");
        let dead_ws = seed_workspace(&workspaces, "key-dead", Some(&dead_root), &["a".repeat(64).as_str()]);
        fs::remove_dir_all(&dead_root).expect("delete the orphan's root");

        // Hold the workspace's advisory lock, as a live scan / serve would.
        let _held = acquire_lock(&dead_ws).expect("hold the workspace lock");

        let report = reap_orphaned_workspaces_in(&workspaces).expect("reap");

        assert_eq!(report.reaped, 0, "a locked workspace is never reaped");
        assert_eq!(report.skipped_locked, 1);
        assert!(dead_ws.exists(), "the locked workspace dir survives");
    }
}
