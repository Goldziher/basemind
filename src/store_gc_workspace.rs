//! Orphaned-workspace reaper for the machine-global cache.
//!
//! Per-workspace cache state lives at `<data home>/cache/workspaces/<workspace_key>/`, where the
//! key is a **one-way** blake3 hash of the canonicalized worktree root. Blobs, by contrast, live in
//! ONE machine-global content-addressed store shared by every workspace, and the daemon's blob GC
//! ([`crate::store_gc::gc_global_blobs`]) reference-counts a blob against *every* workspace dir it
//! finds — so a workspace dir left behind by a deleted repo / worktree / test temp dir keeps voting
//! in that live set and **pins its blobs forever**. The cache then grows monotonically and can never
//! shrink.
//!
//! This module closes that leak: it reads the [`crate::store::WorkspaceMarker`] each workspace dir
//! records (the canonical root it was keyed from — the missing piece that makes an orphan
//! *detectable* at all) and removes the dirs whose root no longer exists on disk. The daemon and
//! `basemind cache gc` run it immediately before the blob sweep, so the blobs a reaped workspace was
//! pinning are reclaimed by that very sweep.
//!
//! The policy is deliberately conservative — a wrong delete here destroys a live index:
//!
//! | Workspace dir | Action |
//! |---|---|
//! | marker present, recorded root missing | reap (renamed aside, then deleted) |
//! | marker present, recorded root exists | keep |
//! | marker **absent**, holds index or other state | **keep** — unverifiable; it self-heals on next open |
//! | marker **absent**, only stale disposable leftovers | reap — see [`DISPOSABLE_LEFTOVERS`] |
//! | `.lock` held by another process | skip — never reap a workspace someone is using |
//!
//! A marker-less dir is reaped only when every entry in it is a regular file named in
//! [`DISPOSABLE_LEFTOVERS`], nothing in it (nor the dir itself) was modified within
//! [`MARKERLESS_WORKSPACE_GRACE`], and no process holds its `.lock`. These are the dirs a comms CLI
//! verb or hook leaves behind when it resolves its identity for a directory that is never scanned:
//! no store open ever writes their marker, so without this rule they accumulate forever.
//!
//! Split into its own module (mirroring `store_lock.rs`) to keep `store_gc.rs` under the module
//! size cap; the entry points are re-exported from [`crate::store_gc`].

use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::data_root::DataRoot;
use crate::store::{LOCK_FILE, LOCK_META_FILE, acquire_lock, read_workspace_marker, writer_lock_is_held};
use crate::store_cache_admin::{TELEMETRY_FILENAME, TELEMETRY_ROTATED_FILENAME};
use crate::store_gc::{GcError, dir_size};
use crate::store_layout::AGENT_ID_FILE;

/// The only files a marker-less workspace dir may hold and still be reaped: the persisted CLI
/// identity, the telemetry log, and the store lock with its holder sidecar. None of them is index
/// state, and each is recreated on demand. Any other entry (a subdirectory, a symlink, an unknown
/// file, an unparsable `workspace.json`) keeps the dir.
const DISPOSABLE_LEFTOVERS: [&str; 5] = [
    AGENT_ID_FILE,
    TELEMETRY_FILENAME,
    TELEMETRY_ROTATED_FILENAME,
    LOCK_FILE,
    LOCK_META_FILE,
];

/// How long a marker-less dir of [`DISPOSABLE_LEFTOVERS`] must sit untouched before it is reaped.
/// A scan creates its dir and takes `.lock` before it writes the marker, and the identity resolver
/// refreshes `agent-id` while it is in use, so the window only has to outlast a pause between uses
/// of one identity.
const MARKERLESS_WORKSPACE_GRACE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Result of an orphaned-workspace reap sweep. Every inspected dir lands in exactly one of the
/// four outcome counters (`reaped` + `kept_live` + `kept_unverifiable` + `skipped_locked` ==
/// `scanned`), so an operator can tell "nothing to reap" apart from "nothing was verifiable".
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReapReport {
    /// Workspace directories inspected.
    pub scanned: usize,
    /// Workspace directories removed: orphans (marker present, recorded root gone) plus
    /// [`Self::reaped_markerless`].
    pub reaped: usize,
    /// The subset of [`Self::reaped`] that carried no marker and held only stale disposable
    /// leftovers.
    pub reaped_markerless: usize,
    /// Kept because the recorded root still exists.
    pub kept_live: usize,
    /// Kept because the root cannot be verified: no marker and the dir is not provably disposable,
    /// or the recorded root sits on a volume that is not mounted. Also kept: an orphan whose view
    /// holds memory notes a release before 0.30 kept there, which could not be moved to the group
    /// store.
    pub kept_unverifiable: usize,
    /// Skipped because another process holds the workspace `.lock`.
    pub skipped_locked: usize,
    /// Bytes reclaimed by the removals (on-disk size, stat'd before deletion).
    pub bytes_freed: u64,
}

/// Reap orphaned workspace cache dirs under the machine-global `cache/workspaces/`.
///
/// Destructive; see the module docs for the (conservative) policy. The daemon calls this from inside
/// its blob-GC write guard and `basemind cache gc` from its own process; either way a dir is removed
/// only under its workspace lock, so a scan writing to it keeps it.
pub fn reap_orphaned_workspaces(data: &DataRoot) -> Result<ReapReport, GcError> {
    reap_orphaned_workspaces_in(&data.workspaces_dir())
}

/// [`reap_orphaned_workspaces`] against an explicit workspaces directory. Production passes the
/// global `cache/workspaces`; unit tests pass a per-test temp dir so they never mutate (nor race on)
/// the machine-global cache. Mirrors the `*_in` seam convention in [`crate::store_gc`].
pub(crate) fn reap_orphaned_workspaces_in(workspaces_dir: &Path) -> Result<ReapReport, GcError> {
    reap_orphaned_workspaces_at(workspaces_dir, SystemTime::now())
}

/// [`reap_orphaned_workspaces_in`] with an explicit clock, so tests age a marker-less dir past
/// [`MARKERLESS_WORKSPACE_GRACE`] without rewriting mtimes.
fn reap_orphaned_workspaces_at(workspaces_dir: &Path, now: SystemTime) -> Result<ReapReport, GcError> {
    let mut report = ReapReport::default();
    if !workspaces_dir.exists() {
        return Ok(report);
    }
    crate::store_lock::purge_reaped(workspaces_dir);
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
            match classify_markerless(&workspace_dir, now) {
                Markerless::Keep => {
                    report.kept_unverifiable += 1;
                    tracing::debug!(
                        workspace = %workspace_dir.display(),
                        "workspace cache has no root marker; keeping (unverifiable, self-heals on next open)"
                    );
                }
                Markerless::Locked => report.skipped_locked += 1,
                Markerless::Disposable => reap_markerless(&workspace_dir, &mut report)?,
            }
            continue;
        };
        // Only a definite "not found" reads as deleted: `exists()` is false for a root this process
        // may not stat (EPERM, EACCES) or a stale mount (ESTALE), and that cache is not an orphan. ~keep
        match marker.root.try_exists() {
            Ok(true) => {
                report.kept_live += 1;
                continue;
            }
            Err(error) => {
                report.kept_unverifiable += 1;
                tracing::debug!(
                    workspace = %workspace_dir.display(),
                    root = %marker.root.display(),
                    %error,
                    "workspace root cannot be checked; keeping"
                );
                continue;
            }
            Ok(false) => {}
        }
        if marker
            .root
            .parent()
            .is_some_and(|parent| !matches!(parent.try_exists(), Ok(true)))
        {
            report.kept_unverifiable += 1;
            tracing::debug!(
                workspace = %workspace_dir.display(),
                root = %marker.root.display(),
                "workspace root AND its parent are both missing (unmounted volume?); keeping — a \
                 detached disk must not be mistaken for a deleted repo"
            );
            continue;
        }

        let Ok(lock) = acquire_lock(&workspace_dir) else {
            report.skipped_locked += 1;
            tracing::debug!(
                workspace = %workspace_dir.display(),
                "orphaned workspace cache is locked by another process; skipping this sweep"
            );
            continue;
        };
        if !crate::store_gc_live::rescue_legacy_memory(&workspace_dir) {
            report.kept_unverifiable += 1;
            continue;
        }
        let bytes = dir_size(&workspace_dir)?;
        crate::store_lock::remove_dir_under_lock(&workspace_dir).map_err(|source| GcError::Io {
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

/// Marker-less workspace dirs the destructive sweep would reap, for the report-only GC paths.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkerlessCandidates {
    /// Dirs that hold only stale disposable leftovers.
    pub count: usize,
    /// Their combined on-disk size.
    pub bytes: u64,
}

/// Count the marker-less workspace dirs [`reap_orphaned_workspaces`] would remove, deleting nothing
/// and taking no lock.
pub fn markerless_workspace_candidates(data: &DataRoot) -> Result<MarkerlessCandidates, GcError> {
    markerless_workspace_candidates_at(&data.workspaces_dir(), SystemTime::now())
}

fn markerless_workspace_candidates_at(workspaces_dir: &Path, now: SystemTime) -> Result<MarkerlessCandidates, GcError> {
    let mut candidates = MarkerlessCandidates::default();
    if !workspaces_dir.exists() {
        return Ok(candidates);
    }
    let io = |source| GcError::Io {
        path: workspaces_dir.to_path_buf(),
        source,
    };
    for entry in std::fs::read_dir(workspaces_dir).map_err(io)? {
        let workspace_dir = entry.map_err(io)?.path();
        if !workspace_dir.is_dir() || read_workspace_marker(&workspace_dir).is_some() {
            continue;
        }
        if matches!(classify_markerless(&workspace_dir, now), Markerless::Disposable) {
            candidates.count += 1;
            candidates.bytes += dir_size(&workspace_dir)?;
        }
    }
    Ok(candidates)
}

/// What the sweep may do with a workspace dir that has no readable marker.
enum Markerless {
    /// Holds state, or was touched within the grace window.
    Keep,
    /// Disposable and stale, but another process holds its `.lock`.
    Locked,
    /// Disposable, stale and unlocked.
    Disposable,
}

fn classify_markerless(workspace_dir: &Path, now: SystemTime) -> Markerless {
    let Some(newest) = newest_disposable_mtime(workspace_dir) else {
        return Markerless::Keep;
    };
    let stale = now
        .duration_since(newest)
        .is_ok_and(|age| age >= MARKERLESS_WORKSPACE_GRACE);
    if !stale {
        return Markerless::Keep;
    }
    if writer_lock_is_held(workspace_dir) {
        return Markerless::Locked;
    }
    Markerless::Disposable
}

/// The newest mtime among `workspace_dir` and its entries, or `None` unless every entry is a
/// regular file named in [`DISPOSABLE_LEFTOVERS`]. Anything unreadable also yields `None`: a dir
/// that cannot be inspected is never disposable.
fn newest_disposable_mtime(workspace_dir: &Path) -> Option<SystemTime> {
    let mut newest = std::fs::symlink_metadata(workspace_dir).ok()?.modified().ok()?;
    for entry in std::fs::read_dir(workspace_dir).ok()? {
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_file() {
            return None;
        }
        let name = entry.file_name();
        if !name.to_str().is_some_and(|name| DISPOSABLE_LEFTOVERS.contains(&name)) {
            return None;
        }
        newest = newest.max(entry.metadata().ok()?.modified().ok()?);
    }
    Some(newest)
}

/// Remove a marker-less dir [`classify_markerless`] found disposable. The contents are checked
/// again under the workspace lock, so a store open that raced the classification keeps its dir.
fn reap_markerless(workspace_dir: &Path, report: &mut ReapReport) -> Result<(), GcError> {
    let Ok(lock) = acquire_lock(workspace_dir) else {
        report.skipped_locked += 1;
        return Ok(());
    };
    if newest_disposable_mtime(workspace_dir).is_none() {
        report.kept_unverifiable += 1;
        return Ok(());
    }
    let bytes = dir_size(workspace_dir)?;
    crate::store_lock::remove_dir_under_lock(workspace_dir).map_err(|source| GcError::Io {
        path: workspace_dir.to_path_buf(),
        source,
    })?;
    drop(lock);
    report.reaped += 1;
    report.reaped_markerless += 1;
    report.bytes_freed += bytes;
    tracing::info!(
        workspace = %workspace_dir.display(),
        bytes,
        "reaped marker-less workspace cache (only stale disposable leftovers)"
    );
    Ok(())
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
                    extract_epoch: crate::extract::EXTRACT_EPOCH,
                    content_key: None,
                    lines: 0,
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
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let volume = tmp.path().join("Volumes").join("external");
        let root = volume.join("repo");
        fs::create_dir_all(root.join("src")).expect("mk root on the volume");

        let workspace = seed_workspace(&workspaces, "key-ext", Some(&root), &["c".repeat(64).as_str()]);

        fs::remove_dir_all(tmp.path().join("Volumes")).expect("unmount the volume");

        let report = reap_orphaned_workspaces_in(&workspaces).expect("reap");

        assert_eq!(report.reaped, 0, "an unmounted volume must never be reaped");
        assert_eq!(report.kept_unverifiable, 1, "it is kept as unverifiable");
        assert!(workspace.exists(), "the workspace index survives the unmount");
    }

    #[test]
    fn reaping_every_orphan_leaves_the_repo_group_memory_store() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let root = make_root(tmp.path(), "gone");
        seed_workspace(&workspaces, "key-gone", Some(&root), &["d".repeat(64).as_str()]);
        let memory = tmp
            .path()
            .join("repos")
            .join("group")
            .join(crate::store::MEMORY_STORE_DIR);
        fs::create_dir_all(&memory).expect("mk memory store");
        fs::write(memory.join("data"), b"memory").expect("write memory");
        fs::remove_dir_all(&root).expect("delete the repo");

        let report = reap_orphaned_workspaces_in(&workspaces).expect("reap");

        assert_eq!(report.reaped, 1, "the orphaned workspace is reaped");
        assert_eq!(
            fs::read(memory.join("data")).unwrap(),
            b"memory",
            "group memory is never reaped"
        );
    }

    /// A release before 0.30 kept memory notes in each view's index. Reaping an orphan no writer had
    /// opened since the upgrade deleted them; they are moved to a group store first.
    #[test]
    fn reaping_an_orphan_moves_its_pre_030_memory_notes_to_a_group_store_first() {
        crate::store::init_isolated_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let root = make_root(tmp.path(), "gone-0-29");
        let workspace = seed_workspace(&workspaces, "key-0-29", Some(&root), &[]);
        plant_legacy_notes(&workspace, 29);
        let marker_root = read_workspace_marker(&workspace).expect("marker").root;
        fs::remove_dir_all(&root).expect("delete the repo");

        let report = reap_orphaned_workspaces_in(&workspaces).expect("reap");

        assert_eq!(report.reaped, 1);
        assert!(!workspace.exists());
        let reader = tmp.path().join("reader");
        let memory_dir = crate::store_layout::memory_store_dir(&marker_root);
        let db = crate::index::IndexDb::open_with_memory(&reader, &memory_dir).expect("open the group store");
        assert_eq!(db.notes().get(b"k").unwrap().as_deref(), Some(&b"only copy"[..]));
    }

    /// Give the working view of `workspace` a file map of `schema_ver` and an index holding a note
    /// in the keyspace releases before 0.30 kept notes in.
    fn plant_legacy_notes(workspace: &Path, schema_ver: u16) {
        let working = workspace.join(VIEWS_DIR).join("working");
        let mut map = Index::empty();
        map.schema_ver = schema_ver;
        fs::write(
            working.join(INDEX_FILE),
            rmp_serde::to_vec_named(&map).expect("encode index"),
        )
        .expect("write the file map");
        let db = fjall::Database::builder(working.join(crate::index::INDEX_DIR))
            .open()
            .expect("open the view's index");
        let notes = db
            .keyspace("memory_by_key", fjall::KeyspaceCreateOptions::default)
            .expect("the notes keyspace of a 0.29 view");
        notes
            .insert(b"k".to_vec(), b"only copy".to_vec())
            .expect("write a note");
    }

    fn group_note(memory_dir: &Path, reader: &Path) -> Option<Vec<u8>> {
        let db = crate::index::IndexDb::open_with_memory(reader, memory_dir).expect("open the group store");
        db.notes().get(b"k").expect("read").map(|note| note.to_vec())
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = crate::testing::git_command()
            .args(["-c", "user.name=t", "-c", "user.email=t@e.x"])
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?}");
    }

    /// The notes of a deleted linked worktree belong to its clone's group. Resolved from the root
    /// that is gone, the group fell back to the hash of that root, a group no worktree reads.
    #[test]
    fn reaping_a_deleted_linked_worktree_moves_its_notes_to_its_clones_group() {
        crate::store::init_isolated_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let main = make_root(tmp.path(), "main");
        git(&main, &["init", "--quiet"]);
        fs::write(main.join("src/lib.rs"), b"pub fn f() {}\n").expect("write");
        git(&main, &["add", "."]);
        git(&main, &["commit", "--quiet", "-m", "init"]);
        let linked = tmp.path().join("linked");
        let linked_arg = linked.to_str().expect("utf-8 path");
        git(&main, &["worktree", "add", "--quiet", "-b", "side", linked_arg]);
        let workspace = seed_workspace(&workspaces, "key-linked", Some(&linked), &[]);
        plant_legacy_notes(&workspace, 29);
        fs::remove_dir_all(&linked).expect("delete the linked worktree");

        let report = reap_orphaned_workspaces_in(&workspaces).expect("reap");

        assert_eq!(report.reaped, 1);
        let group = crate::store_layout::memory_store_dir(&main);
        assert_eq!(
            group_note(&group, &tmp.path().join("reader")).as_deref(),
            Some(&b"only copy"[..]),
            "the note is in the group every worktree of the clone reads"
        );
    }

    /// A file map written at 0.30 or later does not prove the notes were moved: a writer open that
    /// met a newer notes store left them in place and still rewrote the map. Collection checks the
    /// index itself.
    #[test]
    fn reaping_moves_notes_a_current_file_map_still_sits_over() {
        crate::store::init_isolated_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let root = make_root(tmp.path(), "gone-current");
        let workspace = seed_workspace(&workspaces, "key-current", Some(&root), &[]);
        plant_legacy_notes(&workspace, crate::extract::SCHEMA_VER);
        let marker_root = read_workspace_marker(&workspace).expect("marker").root;
        fs::remove_dir_all(&root).expect("delete the repo");

        let report = reap_orphaned_workspaces_in(&workspaces).expect("reap");

        assert_eq!(report.reaped, 1);
        let group = crate::store_layout::memory_store_dir(&marker_root);
        assert_eq!(
            group_note(&group, &tmp.path().join("reader")).as_deref(),
            Some(&b"only copy"[..])
        );
    }

    /// A root this process may not stat is not a deleted root: `exists()` read the permission error
    /// as "gone" and the reaper deleted a live workspace's cache.
    #[cfg(unix)]
    #[test]
    fn a_workspace_whose_root_cannot_be_checked_is_never_reaped() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let guarded = tmp.path().join("guarded");
        let root = make_root(&guarded, "repo");
        let workspace = seed_workspace(&workspaces, "key-guarded", Some(&root), &["e".repeat(64).as_str()]);
        fs::set_permissions(&guarded, fs::Permissions::from_mode(0o000)).expect("deny the parent");
        let unreadable = root.try_exists().is_err();

        let report = reap_orphaned_workspaces_in(&workspaces);
        fs::set_permissions(&guarded, fs::Permissions::from_mode(0o755)).expect("restore");
        let report = report.expect("reap");

        assert!(unreadable, "the root could not be checked, or this test proves nothing");
        assert_eq!(report.reaped, 0, "a root that cannot be checked is not a deleted one");
        assert_eq!(report.kept_unverifiable, 1);
        assert!(workspace.exists());
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
        let pinned_stem = "b".repeat(64);
        let pinned_bytes = b"blob-pinned-by-the-orphan";
        fs::write(blobs.join(format!("{live_stem}.fm.msgpack")), b"fm-live").expect("live blob");
        fs::write(blobs.join(format!("{pinned_stem}.fm.msgpack")), pinned_bytes).expect("pinned blob");

        let live_root = make_root(tmp.path(), "live-repo");
        let dead_root = make_root(tmp.path(), "dead-repo");
        seed_workspace(&workspaces, "key-live", Some(&live_root), &[live_stem.as_str()]);
        let dead_ws = seed_workspace(&workspaces, "key-dead", Some(&dead_root), &[pinned_stem.as_str()]);
        fs::remove_dir_all(&dead_root).expect("delete the orphan's root");

        let before = gc_global_blobs_in(&workspaces, &blobs, Duration::ZERO).expect("gc before");
        assert_eq!(before.removed, 0, "nothing reclaimable while the orphan pins the blob");
        assert!(
            blobs.join(format!("{pinned_stem}.fm.msgpack")).exists(),
            "the orphan pins its blob against the pre-reap sweep"
        );

        let reap = reap_orphaned_workspaces_in(&workspaces).expect("reap");
        assert_eq!(reap.reaped, 1, "the orphan is reaped");
        assert!(!dead_ws.exists());

        let referenced = collect_referenced_hashes_global_in(&workspaces).expect("union");
        assert!(
            !referenced.contains(&pinned_stem),
            "the reaped workspace no longer votes in the live set"
        );
        let after = gc_global_blobs_in(&workspaces, &blobs, Duration::ZERO).expect("gc after");
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

        let shared_stem = "a".repeat(64);
        fs::write(blobs.join(format!("{shared_stem}.fm.msgpack")), b"fm-shared").expect("shared blob");

        let live_root = make_root(tmp.path(), "live-repo");
        let dead_root = make_root(tmp.path(), "dead-repo");
        seed_workspace(&workspaces, "key-live", Some(&live_root), &[shared_stem.as_str()]);
        seed_workspace(&workspaces, "key-dead", Some(&dead_root), &[shared_stem.as_str()]);
        fs::remove_dir_all(&dead_root).expect("delete the orphan's root");

        assert_eq!(reap_orphaned_workspaces_in(&workspaces).expect("reap").reaped, 1);

        let report = gc_global_blobs_in(&workspaces, &blobs, Duration::ZERO).expect("gc");
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

        let _held = acquire_lock(&dead_ws).expect("hold the workspace lock");

        let report = reap_orphaned_workspaces_in(&workspaces).expect("reap");

        assert_eq!(report.reaped, 0, "a locked workspace is never reaped");
        assert_eq!(report.skipped_locked, 1);
        assert!(dead_ws.exists(), "the locked workspace dir survives");
    }

    const PAST_GRACE: Duration = Duration::from_secs(8 * 24 * 60 * 60);
    const WITHIN_GRACE: Duration = Duration::from_secs(6 * 24 * 60 * 60);

    /// A marker-less dir as the identity resolver leaves it: `<workspaces>/<key>/` holding `files`.
    fn seed_leftovers(workspaces_dir: &Path, key: &str, files: &[&str]) -> PathBuf {
        let workspace_dir = workspaces_dir.join(key);
        fs::create_dir_all(&workspace_dir).expect("mk leftover dir");
        for file in files {
            fs::write(workspace_dir.join(file), b"session-1-2").expect("write leftover");
        }
        workspace_dir
    }

    #[test]
    fn a_stale_markerless_dir_holding_only_an_agent_id_is_reaped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let identity_only = seed_leftovers(&workspaces, "key-identity", &[AGENT_ID_FILE]);
        let telemetry_only = seed_leftovers(&workspaces, "key-telemetry", &[TELEMETRY_FILENAME]);

        let report = reap_orphaned_workspaces_at(&workspaces, SystemTime::now() + PAST_GRACE).expect("reap");

        assert_eq!(report.scanned, 2);
        assert_eq!(report.reaped, 2, "both leftover dirs are reaped");
        assert_eq!(report.reaped_markerless, 2);
        assert_eq!(report.kept_unverifiable, 0);
        assert!(report.bytes_freed > 0, "the reaped bytes are accounted");
        assert!(!identity_only.exists(), "the agent-id dir is removed");
        assert!(!telemetry_only.exists(), "the telemetry dir is removed");
    }

    #[test]
    fn a_markerless_dir_inside_the_grace_window_is_kept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let fresh = seed_leftovers(&workspaces, "key-fresh", &[AGENT_ID_FILE]);

        for now in [SystemTime::now(), SystemTime::now() + WITHIN_GRACE] {
            let report = reap_orphaned_workspaces_at(&workspaces, now).expect("reap");
            assert_eq!(report.reaped, 0, "a dir younger than the grace window is never reaped");
            assert_eq!(report.kept_unverifiable, 1);
        }
        assert!(fresh.join(AGENT_ID_FILE).exists(), "the identity survives");
    }

    #[test]
    fn a_markerless_dir_holding_anything_else_is_kept_regardless_of_age() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let with_views = seed_leftovers(&workspaces, "key-views", &[AGENT_ID_FILE]);
        fs::create_dir_all(with_views.join(VIEWS_DIR)).expect("mk empty views dir");
        let with_unknown_file = seed_leftovers(&workspaces, "key-unknown", &[AGENT_ID_FILE, "notes.txt"]);
        let with_bad_marker = seed_leftovers(
            &workspaces,
            "key-bad-marker",
            &[AGENT_ID_FILE, crate::store::WORKSPACE_MARKER_FILE],
        );

        let report = reap_orphaned_workspaces_at(&workspaces, SystemTime::now() + PAST_GRACE).expect("reap");

        assert_eq!(report.scanned, 3);
        assert_eq!(report.reaped, 0, "only an allow-listed dir is disposable");
        assert_eq!(report.kept_unverifiable, 3);
        assert!(
            with_views.join(VIEWS_DIR).exists(),
            "a dir with a subdirectory survives"
        );
        assert!(with_unknown_file.join("notes.txt").exists(), "an unknown file survives");
        assert!(
            with_bad_marker.join(AGENT_ID_FILE).exists(),
            "an unparsable marker is not an absent one"
        );
    }

    #[test]
    fn a_locked_markerless_dir_is_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let locked = seed_leftovers(&workspaces, "key-locked", &[AGENT_ID_FILE]);
        let _held = acquire_lock(&locked).expect("hold the workspace lock");

        let report = reap_orphaned_workspaces_at(&workspaces, SystemTime::now() + PAST_GRACE).expect("reap");

        assert_eq!(report.reaped, 0, "a locked dir is never reaped");
        assert_eq!(report.skipped_locked, 1);
        assert!(locked.join(AGENT_ID_FILE).exists(), "the locked dir survives");
    }

    #[test]
    fn the_candidate_report_counts_disposable_dirs_without_touching_them() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let disposable = seed_leftovers(&workspaces, "key-identity", &[AGENT_ID_FILE]);
        let legacy = seed_workspace(&workspaces, "key-legacy", None, &["a".repeat(64).as_str()]);
        let dead_root = make_root(tmp.path(), "dead-repo");
        seed_workspace(&workspaces, "key-dead", Some(&dead_root), &["b".repeat(64).as_str()]);
        fs::remove_dir_all(&dead_root).expect("delete the orphan's root");
        let stale = SystemTime::now() + PAST_GRACE;

        assert_eq!(
            markerless_workspace_candidates_at(&workspaces, SystemTime::now()).expect("fresh report"),
            MarkerlessCandidates::default(),
            "nothing is a candidate inside the grace window"
        );
        let candidates = markerless_workspace_candidates_at(&workspaces, stale).expect("report");

        assert_eq!(candidates.count, 1, "only the leftover dir is a candidate");
        assert_eq!(candidates.bytes, dir_size(&disposable).expect("size"));
        assert!(candidates.bytes > 0);
        assert_eq!(
            fs::read_dir(&disposable).expect("list").count(),
            1,
            "the report adds no lock file and removes nothing"
        );
        assert!(legacy.exists(), "the legacy dir is untouched");

        let reaped = reap_orphaned_workspaces_at(&workspaces, stale).expect("reap");
        assert_eq!(
            reaped.reaped_markerless, candidates.count,
            "the sweep removes what was reported"
        );
        assert_eq!(reaped.reaped, 2, "plus the marked orphan");
    }
}
