//! The blob store's namespaces under `cache stats` and the sweeps: what each reports, reaps and
//! keeps, and what a lock or a newer build's view refuses.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use super::tests::{NO_HOLDS, build_fixture, seed_workspace};
use super::*;
use crate::store::{INDEX_FILE, Index, VIEWS_DIR};
use crate::store_cache_admin::{TELEMETRY_FILENAME, cache_stats_in, clear_component_in};

fn plant_blob(dir: &std::path::Path, stem: char) -> PathBuf {
    fs::create_dir_all(dir).expect("mk namespace");
    let path = dir.join(format!("{}.fm.msgpack", stem.to_string().repeat(64)));
    fs::write(&path, vec![b'x'; 4096]).expect("write blob");
    path
}

/// The footprint is the whole blob store, every namespace and the un-namespaced files of older
/// builds; the current namespace is reported beside it, and blob counts are the current one's.
#[test]
fn cache_stats_measures_every_blob_namespace() {
    let fx = build_fixture();
    let root = fx.blobs().root;
    let only_current = cache_stats_in(&fx.basemind_dir, &fx.blobs()).unwrap();
    plant_blob(&root, 'c');
    plant_blob(&root.join("v1"), 'd');
    plant_blob(&root.join("v65000"), 'e');

    let stats = cache_stats_in(&fx.basemind_dir, &fx.blobs()).unwrap();

    assert_eq!(stats.blobs_current_bytes, only_current.blobs_current_bytes);
    assert_eq!(
        stats.blob_count, only_current.blob_count,
        "counted in the current namespace"
    );
    assert!(
        stats.blobs_bytes >= stats.blobs_current_bytes + 3 * 4096,
        "the other namespaces and the flat blob are measured: {} vs {}",
        stats.blobs_bytes,
        stats.blobs_current_bytes
    );
}

/// `cache clear blobs` takes what this binary and older ones wrote, and leaves a newer
/// basemind's namespace alone, naming it.
#[test]
fn clearing_blobs_takes_older_namespaces_and_flat_blobs_and_spares_a_newer_one() {
    let fx = build_fixture();
    let root = fx.blobs().root;
    let flat = plant_blob(&root, 'c');
    let older = plant_blob(&root.join("v1"), 'd');
    let newer = plant_blob(&root.join("v65000"), 'e');
    let stray = root.join("notes.txt");
    fs::write(&stray, b"not a blob").expect("write stray");

    let report = clear_component_in(&fx.basemind_dir, CacheComponent::Blobs, &fx.blobs()).expect("clear blobs");

    assert_eq!(report.spared_newer_blob_namespaces, ["v65000"]);
    assert!(newer.exists(), "a newer basemind's blobs are not ours to clear");
    assert!(!flat.exists(), "un-namespaced blobs go");
    assert!(!older.exists() && !root.join("v1").exists(), "an older namespace goes");
    assert!(stray.exists(), "a file that is not a blob is left alone");
    assert!(fx.blobs_dir.exists() && fs::read_dir(&fx.blobs_dir).unwrap().next().is_none());
}

/// The sweep of the blob store is both sweeps: the current namespace's unreferenced blobs, and
/// what older builds left beside it. A namespace of an older schema would otherwise never be
/// collected by anything.
#[test]
fn the_blob_store_sweep_reaps_the_current_namespace_and_the_superseded_ones() {
    let fx = build_fixture();
    let paths = fx.blobs();
    let workspaces = fx.basemind_dir.join("no-workspaces");
    let orphan = fx.blobs_dir.join(format!("{}.fm.msgpack", fx.orphan_stem));
    let older = plant_blob(&paths.root.join("v1"), 'd');
    let flat = plant_blob(&paths.root, 'c');
    let newer = plant_blob(&paths.root.join("v65000"), 'e');
    assert!(orphan.exists());

    let report = gc_blob_store_in(&workspaces, &paths.root, paths.schema, Duration::ZERO, || NO_HOLDS).expect("sweep");

    assert!(
        !orphan.exists(),
        "an unreferenced blob of the current namespace is reaped"
    );
    assert!(!older.exists() && !flat.exists(), "and so is what older builds left");
    assert!(newer.exists(), "a newer namespace is never touched");
    assert!(report.removed >= 3, "{report:?}");
}

/// The sweep the daemon and the CLI run over the machine-global store is the whole-store sweep: a
/// blob of an older namespace and an un-namespaced one, both unwritten for longer than the default
/// 14-day hold and read by no live process, go; a newer namespace stays. The data home is the
/// test's own: blobs other tests plant in the process's shared one would be newer writes into the
/// un-namespaced blobs, and keep them for the hold.
#[test]
fn the_machine_global_sweep_reaps_what_older_builds_left() {
    let home = tempfile::tempdir().expect("data home");
    let data = crate::data_root::DataRoot::new(home.path());
    let root = data.global_blobs_root();
    let aged = |path: &PathBuf| {
        let past_the_hold = Duration::from_secs(15 * 24 * 60 * 60);
        fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(std::time::SystemTime::now() - past_the_hold))
            .expect("age the blob");
    };
    let older = plant_blob(&root.join("v1"), 'f');
    let flat = plant_blob(&root, 'g');
    let newer = plant_blob(&root.join("v65001"), 'h');
    aged(&older);
    aged(&flat);
    aged(&newer);

    gc_global_blobs(&data).expect("sweep");

    assert!(!older.exists(), "an older namespace is swept by the global sweep");
    assert!(!flat.exists(), "so is an un-namespaced blob");
    assert!(newer.exists(), "a newer namespace is not touched");
}

/// Reaping what older builds left needs no live set. A workspace whose references cannot be read
/// fails the sweep of the current namespace, and must not also keep the old namespaces on disk.
#[test]
fn superseded_namespaces_are_reaped_although_the_live_set_cannot_be_read() {
    let fx = build_fixture();
    let paths = fx.blobs();
    let workspaces = fx.basemind_dir.join("workspaces");
    let unreadable = workspaces.join("key").join(VIEWS_DIR).join("working").join(INDEX_FILE);
    fs::create_dir_all(&unreadable).expect("a directory where the file map belongs");
    let older = plant_blob(&paths.root.join("v1"), 'd');
    let flat = plant_blob(&paths.root, 'c');
    let orphan = fx.blobs_dir.join(format!("{}.fm.msgpack", fx.orphan_stem));

    let swept = gc_blob_store_in(&workspaces, &paths.root, paths.schema, Duration::ZERO, || NO_HOLDS);

    assert!(
        swept.is_err(),
        "the current namespace cannot be swept without its live set"
    );
    assert!(orphan.exists(), "and nothing of it was");
    assert!(
        !older.exists() && !flat.exists(),
        "what older builds left is reaped all the same"
    );
}

/// Blobs are namespaced by schema, so a view a newer basemind wrote names nothing in this binary's
/// namespace: it neither votes in the sweep nor stops it, and it is left as it is.
#[test]
fn a_view_of_a_newer_basemind_neither_stops_the_sweep_nor_is_touched_by_it() {
    let fx = build_fixture();
    let paths = fx.blobs();
    let workspaces = fx.basemind_dir.join("workspaces");
    seed_workspace(&workspaces, "key-newer", &[&fx.orphan_stem]);
    let newer_map = workspaces
        .join("key-newer")
        .join(VIEWS_DIR)
        .join("working")
        .join(INDEX_FILE);
    let mut index: Index = rmp_serde::from_slice(&fs::read(&newer_map).expect("read")).expect("decode");
    index.schema_ver = crate::extract::SCHEMA_VER + 1;
    let bytes = rmp_serde::to_vec_named(&index).expect("encode");
    fs::write(&newer_map, &bytes).expect("write");
    let orphan = fx.blobs_dir.join(format!("{}.fm.msgpack", fx.orphan_stem));

    let report = gc_blob_store_in(&workspaces, &paths.root, paths.schema, Duration::ZERO, || NO_HOLDS).expect("sweep");

    assert!(report.removed >= 1, "{report:?}");
    assert!(
        !orphan.exists(),
        "a hash only the newer view names is not kept in this namespace"
    );
    assert_eq!(
        fs::read(&newer_map).expect("still there"),
        bytes,
        "the newer view is untouched"
    );
}

/// The report counts the files of the whole blob store, not of this binary's namespace alone.
#[test]
fn the_report_counts_the_blobs_of_every_namespace() {
    let root = tempfile::tempdir().expect("tempdir");
    plant_blob(root.path(), 'a');
    plant_blob(&root.path().join("v1"), 'b');
    plant_blob(&root.path().join("v2"), 'c');
    plant_blob(&root.path().join("v2"), 'd');

    assert_eq!(count_blob_files(root.path()).expect("count"), 4);
    assert_eq!(count_blob_files(&root.path().join("absent")).expect("count"), 0);
}

/// A workspace can hold a stale view beside one a newer basemind wrote. Neither is removed and
/// neither stops the sweep.
#[test]
fn global_gc_passes_over_a_stale_view_and_the_newer_view_beside_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let views = workspaces.join("key").join(VIEWS_DIR);
    let write = |view: &str, schema_ver: u16| {
        let dir = views.join(view);
        fs::create_dir_all(&dir).expect("mk view");
        let mut index = Index::empty();
        index.schema_ver = schema_ver;
        fs::write(dir.join(INDEX_FILE), rmp_serde::to_vec_named(&index).expect("encode")).expect("write");
        dir
    };
    let stale = write("working", crate::extract::SCHEMA_VER.saturating_sub(1));
    let newer = write("staged", crate::extract::SCHEMA_VER + 1);

    collect_referenced_hashes_global_in(&workspaces).expect("the sweep goes on");

    assert!(stale.join(INDEX_FILE).exists(), "the stale view is kept");
    assert!(
        newer.join(INDEX_FILE).exists(),
        "a newer basemind's view is never removed by GC"
    );
}

/// A workspace another process holds no longer stops the machine-wide mark: passing over its stale
/// view takes no lock. Healing it took one, and `Locked` aborted the mark of every workspace, so
/// the global sweep did nothing for as long as an older basemind held its workspace.
#[test]
fn global_gc_marks_past_a_locked_workspace_holding_a_stale_view() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let live = "a".repeat(64);
    seed_workspace(&workspaces, "key-live", &[&live]);
    let held = workspaces.join("key-held");
    let stale_view = held.join(VIEWS_DIR).join("working");
    fs::create_dir_all(&stale_view).expect("mk view");
    let mut stale = Index::empty();
    stale.schema_ver = crate::extract::SCHEMA_VER.saturating_sub(1);
    fs::write(
        stale_view.join(INDEX_FILE),
        rmp_serde::to_vec_named(&stale).expect("encode"),
    )
    .expect("write");
    let _held = crate::store::acquire_lock(&held).expect("an older basemind holds the workspace");

    let referenced = collect_referenced_hashes_global_in(&workspaces).expect("the mark goes on");

    assert!(referenced.code.contains(&live));
    assert!(stale_view.join(INDEX_FILE).exists());
}

/// The per-repository group directories were neither counted nor reaped. Stats report their size and
/// how many no workspace marker names, and nothing else about the cache changes.
#[test]
fn cache_stats_report_the_repository_groups_without_counting_them_in_the_total() {
    let fx = build_fixture();
    let before = cache_stats_in(&fx.basemind_dir, &fx.blobs()).expect("stats");
    let workspace_before = crate::store_cache_admin::dir_size(&fx.basemind_dir).expect("size");
    let repos = fx.blobs().repos;
    for (group, bytes) in [("named", 3usize), ("orphan", 5)] {
        fs::create_dir_all(repos.join(group).join(crate::store::MEMORY_STORE_DIR)).expect("mk group");
        fs::write(
            repos.join(group).join(crate::store::MEMORY_STORE_DIR).join("data"),
            vec![0u8; bytes],
        )
        .expect("write");
    }
    fs::write(
        fx.basemind_dir.join(crate::store::WORKSPACE_MARKER_FILE),
        br#"{"root":"/gone","updated_unix":0,"group":"named"}"#,
    )
    .expect("write marker");

    let stats = cache_stats_in(&fx.basemind_dir, &fx.blobs()).expect("stats");

    assert_eq!((stats.repo_groups, stats.repo_groups_unnamed), (2, 1));
    let measured = crate::store_cache_admin::dir_size(&repos.join("named")).expect("size")
        + crate::store_cache_admin::dir_size(&repos.join("orphan")).expect("size");
    assert!(measured >= 8);
    assert_eq!(stats.repo_groups_bytes, measured);
    let marker = crate::store_cache_admin::dir_size(&fx.basemind_dir).expect("size") - workspace_before;
    assert_eq!(
        stats.total_bytes,
        before.total_bytes + marker,
        "only the marker the test wrote grew the total; the groups are not in it"
    );
}

/// A clear holds the workspace lock through the delete instead of probing it first: under a live
/// writer every workspace component is refused, naming the lock, and nothing is removed.
#[test]
fn a_clear_under_a_held_workspace_lock_is_refused() {
    let fx = build_fixture();
    fs::write(fx.basemind_dir.join(TELEMETRY_FILENAME), b"{}\n").expect("telemetry");
    let _writer = crate::store_lock::acquire_lock(&fx.basemind_dir).expect("writer lock");

    for component in [
        CacheComponent::Views,
        CacheComponent::Telemetry,
        CacheComponent::Lance,
        CacheComponent::All,
    ] {
        let refused = clear_component_in(&fx.basemind_dir, component, &fx.blobs());
        assert!(
            matches!(refused, Err(GcError::Store(StoreError::Locked { .. }))),
            "{component:?}: {refused:?}"
        );
    }
    let refused = clear_single_view(&fx.basemind_dir, "working");
    assert!(
        matches!(refused, Err(GcError::Store(StoreError::Locked { .. }))),
        "{refused:?}"
    );
    assert!(
        fx.basemind_dir
            .join(VIEWS_DIR)
            .join("working")
            .join(INDEX_FILE)
            .exists()
    );
    assert!(fx.basemind_dir.join(TELEMETRY_FILENAME).exists());
}
