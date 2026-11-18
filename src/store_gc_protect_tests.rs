//! Regression tests for budget-eviction protection (issue #75): a workspace that is the most
//! recently used, locked, or freshly built is never evicted, even when it alone exceeds the budget.

use std::fs;
use std::time::Duration;

use super::tests::seed_workspace_aged;
use super::*;

const DAY: Duration = Duration::from_secs(24 * 3600);

fn roots() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let blobs = tmp.path().join("blobs");
    fs::create_dir_all(&blobs).expect("mk blobs");
    (tmp, workspaces, blobs)
}

/// The incident: one freshly scanned workspace bigger than the budget was evicted minutes after
/// its scan. It must survive, and the over-budget state must be reported.
#[test]
fn a_fresh_workspace_larger_than_the_budget_survives() {
    let (_tmp, workspaces, blobs) = roots();
    let fresh = seed_workspace_aged(&workspaces, "key-fresh", &"a".repeat(64), Duration::from_secs(600));

    let report = enforce_cache_budget_in(&workspaces, &blobs, 1, DAY, Duration::from_secs(6 * 3600)).expect("enforce");

    assert_eq!(report.evicted, 0);
    assert!(fresh.join(crate::store::VIEWS_DIR).exists(), "the index is intact");
    assert!(report.total_bytes_after > 1, "the cache is knowingly left over budget");
    assert_eq!(report.protected.len(), 1);
    assert!(report.protected[0].bytes > 0);
}

/// Even with no grace window and an ancient mtime, the single workspace on the machine is the most
/// recently used one.
#[test]
fn the_most_recently_used_workspace_is_protected_without_a_grace_window() {
    let (_tmp, workspaces, blobs) = roots();
    let old = seed_workspace_aged(&workspaces, "key-old", &"a".repeat(64), 30 * DAY);
    let older = seed_workspace_aged(&workspaces, "key-older", &"b".repeat(64), 60 * DAY);

    let report = enforce_cache_budget_in(&workspaces, &blobs, 1, DAY, Duration::ZERO).expect("enforce");

    assert!(old.join(crate::store::VIEWS_DIR).exists(), "newest is protected");
    assert!(
        !older.join(crate::store::VIEWS_DIR).exists(),
        "the colder one is evicted"
    );
    assert_eq!(report.evicted, 1);
    assert_eq!(report.protected.len(), 1);
    assert_eq!(report.protected[0].reason, ProtectionReason::MostRecentlyUsed);
}

#[test]
fn the_grace_window_protects_a_workspace_that_is_not_the_most_recent() {
    let (_tmp, workspaces, blobs) = roots();
    let newest = seed_workspace_aged(&workspaces, "key-newest", &"a".repeat(64), Duration::from_secs(60));
    let built = seed_workspace_aged(&workspaces, "key-built", &"b".repeat(64), Duration::from_secs(3600));
    let idle = seed_workspace_aged(&workspaces, "key-idle", &"c".repeat(64), 3 * DAY);

    let report = enforce_cache_budget_in(&workspaces, &blobs, 1, DAY, Duration::from_secs(6 * 3600)).expect("enforce");

    assert!(newest.join(crate::store::VIEWS_DIR).exists());
    assert!(
        built.join(crate::store::VIEWS_DIR).exists(),
        "built inside the grace window"
    );
    assert!(
        !idle.join(crate::store::VIEWS_DIR).exists(),
        "idle workspace is evicted"
    );
    let reasons: Vec<_> = report.protected.iter().map(|ws| ws.reason).collect();
    assert!(reasons.contains(&ProtectionReason::MostRecentlyUsed));
    assert!(reasons.contains(&ProtectionReason::RecentlyBuilt));
}

/// Already-evicted shells (no view) must not steal the most-recently-used slot from a real index.
#[test]
fn an_evicted_shell_never_counts_as_the_most_recent_workspace() {
    let (_tmp, workspaces, blobs) = roots();
    let real = seed_workspace_aged(&workspaces, "key-real", &"a".repeat(64), 5 * DAY);
    let shell = seed_workspace_aged(&workspaces, "key-shell", &"b".repeat(64), 6 * DAY);
    fs::remove_dir_all(shell.join(crate::store::VIEWS_DIR)).expect("drop shell views");
    // The shell's marker is the freshest file on the machine.
    fs::File::options()
        .write(true)
        .open(shell.join(WORKSPACE_MARKER_FILE))
        .and_then(|f| f.set_modified(SystemTime::now()))
        .expect("touch shell marker");

    let report = enforce_cache_budget_in(&workspaces, &blobs, 1, DAY, Duration::ZERO).expect("enforce");

    assert!(real.join(crate::store::VIEWS_DIR).exists(), "the real index is the MRU");
    assert_eq!(report.evicted, 0);
}

#[test]
fn protected_workspaces_are_persisted_and_make_the_state_over_budget() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(GC_STATE_FILE);
    let report = GcReport {
        cache_budget_bytes: Some(100),
        cache_bytes_after: Some(500),
        protected_workspaces: vec![ProtectedWorkspace {
            key: "key-a".to_owned(),
            root: Some("/repo".to_owned()),
            bytes: 400,
            reason: ProtectionReason::MostRecentlyUsed,
        }],
        ..GcReport::default()
    };

    persist_gc_state_at(&path, &report);

    let state = read_gc_state_at(&path).expect("state");
    assert_eq!(state.status, GcStatus::OverBudget);
    assert_eq!(state.protected_workspaces.len(), 1);
    assert!(
        state
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("protected") && d.contains("400 bytes")),
        "{:?}",
        state.detail
    );
}

const SCHEMA: u16 = 7;

/// The budget is a disk budget: blobs in an older schema's namespace and un-namespaced blobs of an
/// older build occupy the same disk and are this binary's to reap, so the footprint counts them.
#[test]
fn the_budget_footprint_counts_the_older_blob_namespaces() {
    let (_tmp, workspaces, root) = roots();
    let current = root.join("v7");
    fs::create_dir_all(&current).expect("mk current");
    fs::create_dir_all(root.join("v6")).expect("mk older");
    fs::write(root.join("v6").join("old.fm.msgpack"), vec![0_u8; 64 * 1024]).expect("older blob");
    fs::write(root.join("flat.fm.msgpack"), vec![0_u8; 64 * 1024]).expect("flat blob");
    seed_workspace_aged(&workspaces, "key-a", &"a".repeat(64), Duration::from_secs(9999));

    let report =
        enforce_cache_budget_at(&workspaces, &root, &current, SCHEMA, u64::MAX, DAY, Duration::ZERO).expect("enforce");

    assert!(
        report.total_bytes_before >= 2 * 64 * 1024,
        "blobs outside the current namespace are part of the footprint: {}",
        report.total_bytes_before
    );
}

/// A newer basemind's namespace is bytes this binary can never free. Counted, they would keep the
/// cache over budget whatever is evicted, and every cycle would evict another idle workspace.
#[test]
fn a_newer_blob_namespace_is_outside_the_budget_and_costs_no_workspace() {
    let (_tmp, workspaces, root) = roots();
    let current = root.join("v7");
    fs::create_dir_all(&current).expect("mk current");
    fs::create_dir_all(root.join("v8")).expect("mk newer");
    fs::write(root.join("v8").join("new.fm.msgpack"), vec![0_u8; 256 * 1024]).expect("newer blob");
    let idle = seed_workspace_aged(
        &workspaces,
        "key-idle",
        &"a".repeat(64),
        Duration::from_secs(9 * 24 * 3600),
    );
    seed_workspace_aged(
        &workspaces,
        "key-mru",
        &"b".repeat(64),
        Duration::from_secs(3 * 24 * 3600),
    );

    // Exactly this version's bytes, as measured here: allocation differs between file systems.
    let budget = dir_size(&workspaces).expect("size") + dir_size(&root).expect("size")
        - dir_size(&root.join("v8")).expect("size");
    let report =
        enforce_cache_budget_at(&workspaces, &root, &current, SCHEMA, budget, DAY, Duration::ZERO).expect("enforce");

    assert!(
        report.total_bytes_before <= budget,
        "the newer namespace is not counted: {}",
        report.total_bytes_before
    );
    assert_eq!(report.evicted, 0);
    assert!(
        idle.join(crate::store::VIEWS_DIR).exists(),
        "the idle workspace is kept"
    );
}

/// A view a newer basemind wrote is that version's live index, not cache this binary may rebuild.
#[test]
fn a_workspace_written_by_a_newer_basemind_is_never_evicted() {
    let (_tmp, workspaces, blobs) = roots();
    let newer = seed_workspace_aged(
        &workspaces,
        "key-newer",
        &"a".repeat(64),
        Duration::from_secs(9 * 24 * 3600),
    );
    let mut index = crate::store::Index::empty();
    index.schema_ver = crate::extract::SCHEMA_VER + 1;
    write_index(&newer, &index);
    let map = newer
        .join(crate::store::VIEWS_DIR)
        .join("working")
        .join(crate::store::INDEX_FILE);
    fs::File::options()
        .write(true)
        .open(map)
        .and_then(|file| file.set_modified(SystemTime::now() - Duration::from_secs(9 * 24 * 3600)))
        .expect("age the rewritten map");
    let idle = seed_workspace_aged(
        &workspaces,
        "key-idle",
        &"c".repeat(64),
        Duration::from_secs(8 * 24 * 3600),
    );
    seed_workspace_aged(
        &workspaces,
        "key-mru",
        &"b".repeat(64),
        Duration::from_secs(3 * 24 * 3600),
    );

    let report = enforce_cache_budget_in(&workspaces, &blobs, 1, DAY, Duration::ZERO).expect("enforce");

    assert!(
        newer.join(crate::store::VIEWS_DIR).join("working").exists(),
        "the newer view is intact"
    );
    assert!(
        report
            .protected
            .iter()
            .any(|ws| ws.key == "key-newer" && ws.reason == ProtectionReason::NewerVersion),
        "{:?}",
        report.protected
    );
    assert!(
        !idle.join(crate::store::VIEWS_DIR).exists(),
        "an idle workspace of this version is still evicted"
    );
    assert_eq!(report.locked_skipped, 0, "a newer view is not a locked workspace");
}

/// A newer basemind's workspaces are bytes this binary can never free. Counted, they alone keep the
/// cache over budget and every cycle evicts another idle workspace of this version.
#[test]
fn a_newer_baseminds_workspace_is_outside_the_budget_and_costs_no_workspace() {
    let (_tmp, workspaces, blobs) = roots();
    let newer = seed_workspace_aged(&workspaces, "key-newer", &"a".repeat(64), Duration::from_secs(3600));
    let mut index = crate::store::Index::empty();
    index.schema_ver = crate::extract::SCHEMA_VER + 1;
    write_index(&newer, &index);
    fs::write(newer.join("git-cache"), vec![0_u8; 256 * 1024]).expect("newer bulk");
    let idle = seed_workspace_aged(&workspaces, "key-idle", &"c".repeat(64), 9 * DAY);
    seed_workspace_aged(&workspaces, "key-mru", &"b".repeat(64), 3 * DAY);

    // Exactly this version's bytes, as measured here: allocation differs between file systems.
    let budget =
        dir_size(&workspaces).expect("size") + dir_size(&blobs).expect("size") - dir_size(&newer).expect("size");
    let report = enforce_cache_budget_in(&workspaces, &blobs, budget, DAY, Duration::ZERO).expect("enforce");

    assert!(
        report.total_bytes_before <= budget,
        "the newer workspace is not counted: {}",
        report.total_bytes_before
    );
    assert_eq!(report.evicted, 0);
    assert!(
        idle.join(crate::store::VIEWS_DIR).exists(),
        "the idle workspace is kept"
    );
}

/// The sweep after an eviction takes only blobs older than the look at the locks: a scan in another
/// process that started since has written blobs no file map names yet.
#[test]
fn a_blob_written_after_the_lock_probe_survives_the_sweep_that_follows_an_eviction() {
    let (_tmp, workspaces, blobs) = roots();
    fs::create_dir_all(&workspaces).expect("mk workspaces");
    let orphaned = blobs.join(format!("{}.fm.msgpack", "a".repeat(64)));
    fs::write(&orphaned, b"orphaned by the eviction").expect("old blob");
    std::thread::sleep(Duration::from_millis(30));
    let probed = SystemTime::now();
    std::thread::sleep(Duration::from_millis(30));
    let fresh = blobs.join(format!("{}.fm.msgpack", "b".repeat(64)));
    fs::write(&fresh, b"written by a scan that started after the probe").expect("fresh blob");

    let grace = orphan_grace(&workspaces, probed);
    gc_global_blobs_in(&workspaces, &blobs, grace).expect("sweep");

    assert!(fresh.exists(), "younger than the probe: not this eviction's orphan");
    assert!(!orphaned.exists(), "older than the probe: reclaimed at once");
}

/// With the slack of a real sweep, a blob whose whole-second mtime was rounded to just before the
/// probe survives; one written well before it is reclaimed.
#[test]
fn the_sweep_after_an_eviction_spares_a_blob_within_the_mtime_slack_of_the_probe() {
    let (_tmp, workspaces, blobs) = roots();
    fs::create_dir_all(&workspaces).expect("mk workspaces");
    let probed = SystemTime::now();
    let age = |path: &std::path::Path, by: Duration| {
        fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(probed - by))
            .expect("age blob");
    };
    let rounded = blobs.join(format!("{}.fm.msgpack", "a".repeat(64)));
    fs::write(&rounded, b"written just after the probe, its mtime rounded down").expect("blob");
    age(&rounded, PROBE_MTIME_SLACK / 2);
    let orphaned = blobs.join(format!("{}.fm.msgpack", "b".repeat(64)));
    fs::write(&orphaned, b"orphaned by the eviction").expect("blob");
    age(&orphaned, PROBE_MTIME_SLACK * 3);

    let grace = orphan_grace_with(&workspaces, probed, PROBE_MTIME_SLACK);
    gc_global_blobs_in(&workspaces, &blobs, grace).expect("sweep");

    assert!(rounded.exists(), "within the slack: possibly a scan's fresh blob");
    assert!(!orphaned.exists(), "older than the slack: reclaimed");
}

fn write_index(workspace_dir: &std::path::Path, index: &crate::store::Index) {
    let working = workspace_dir.join(crate::store::VIEWS_DIR).join("working");
    fs::create_dir_all(&working).expect("mk view");
    fs::write(
        working.join(crate::store::INDEX_FILE),
        rmp_serde::to_vec_named(index).expect("encode index"),
    )
    .expect("write index");
}

/// A file map emptied for a rebuild lists no file for as long as the rebuild takes. The blobs it
/// held are what the rebuild re-indexes from, so the sweep still counts them as referenced.
#[test]
fn blobs_of_a_file_map_emptied_for_a_rebuild_survive_the_sweep() {
    let (_tmp, workspaces, blobs) = roots();
    let (pinned, loose) = ("a".repeat(64), "b".repeat(64));
    for stem in [&pinned, &loose] {
        fs::write(blobs.join(format!("{stem}.fm.msgpack")), b"blob").expect("blob");
    }
    let mut index = crate::store::Index::empty();
    index.rebuild_pending = true;
    index.rebuild_pins = vec![pinned.clone()];
    write_index(&workspaces.join("key-rebuilding"), &index);

    crate::store_gc::gc_global_blobs_in(&workspaces, &blobs, Duration::ZERO).expect("sweep");

    assert!(
        blobs.join(format!("{pinned}.fm.msgpack")).exists(),
        "pinned for the rebuild"
    );
    assert!(
        !blobs.join(format!("{loose}.fm.msgpack")).exists(),
        "nothing references this one"
    );
}

/// Emptying the map for a rebuild pins what it held; the scan that finishes the rebuild releases.
#[test]
fn emptying_a_file_map_pins_its_hashes_until_the_rebuild_is_done() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("a.rs"), "pub fn a() {}\n").expect("write");
    let config = crate::config::ConfigV1::with_defaults();
    let scan = |store: &mut crate::store::Store| {
        crate::scanner::scan(
            tmp.path(),
            store,
            &config,
            crate::scanner::ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Deferred,
            crate::scan_retrieval::Retrieval::inert(),
        )
        .expect("scan");
    };
    let mut store = crate::store::Store::open(tmp.path(), crate::store::VIEW_WORKING).expect("open");
    scan(&mut store);
    let hash = store.lookup("a.rs").expect("entry").hash_hex.clone();
    // An index behind its file map: the recovery empties the map and orders a rebuild.
    store.index.generation += 1;

    assert!(store.rebuild_index_if_behind().expect("recover"));
    let on_disk = crate::store::read_index(&store.view_dir).expect("read").expect("map");
    assert!(on_disk.files.is_empty());
    assert_eq!(
        on_disk.rebuild_pins,
        [hash],
        "the emptied map on disk still pins the blob"
    );

    scan(&mut store);
    assert!(store.index.rebuild_pins.is_empty(), "released once the rebuild is done");
}

/// The sweep after an eviction gives unreferenced blobs no grace, which is safe only against scans
/// the caller excludes. A writer lock held by another process may be a scan whose fresh blobs no
/// file map lists yet: with one present, the sweep keeps the grace period.
#[test]
fn an_eviction_sweep_keeps_the_grace_period_while_another_process_holds_a_writer_lock() {
    let (_tmp, workspaces, blobs) = roots();
    let cold = seed_workspace_aged(&workspaces, "key-cold", &"a".repeat(64), DAY * 30);
    let scanning = seed_workspace_aged(&workspaces, "key-scanning", &"b".repeat(64), Duration::from_secs(5));
    let fresh = blobs.join(format!("{}.fm.msgpack", "c".repeat(64)));
    fs::write(&fresh, b"written by a scan whose file map is not flushed yet").expect("fresh blob");
    let _held = crate::store::acquire_lock_as(&scanning, crate::store::LockHolder::Scan).expect("lock");
    let foreign = serde_json::json!({ "command": "basemind scan", "pid": u32::MAX - 7, "acquired_unix": 0 });
    fs::write(scanning.join(crate::store::LOCK_META_FILE), foreign.to_string()).expect("lock meta");

    let report = enforce_cache_budget_in(&workspaces, &blobs, 1, Duration::ZERO, Duration::ZERO).expect("enforce");

    assert!(
        report.evicted >= 1 && !cold.join(crate::store::VIEWS_DIR).exists(),
        "{report:?}"
    );
    assert!(
        fresh.exists(),
        "a blob a scan in another process just wrote is not reaped"
    );

    drop(_held);
    fs::remove_file(scanning.join(crate::store::LOCK_META_FILE)).expect("release");
    let cold_again = seed_workspace_aged(&workspaces, "key-cold", &"a".repeat(64), DAY * 30);
    enforce_cache_budget_in(&workspaces, &blobs, 1, Duration::ZERO, Duration::ZERO).expect("enforce");
    assert!(!cold_again.join(crate::store::VIEWS_DIR).exists());
    assert!(
        !fresh.exists(),
        "with no writer anywhere the unreferenced blob goes at once"
    );
}
