//! Tests of cache-budget enforcement and the persisted GC state.

use super::*;
use crate::store::{FileEntry, INDEX_FILE, Index, VIEWS_DIR, ensure_workspace_marker};
use std::cell::Cell;
use std::fs;

/// A day, for ages and the hot floor.
const DAY_AGO: Duration = Duration::from_secs(24 * 3600);

/// What runs as a workspace is about to be evicted.
type EvictHook = Box<dyn FnMut(&Path)>;

thread_local! {
    /// File maps [`view_is_newer`] opened on this thread.
    pub(super) static VIEW_READS: Cell<usize> = const { Cell::new(0) };
    /// Run by [`evict_workspace`] on this thread before it looks at the workspace.
    static BEFORE_EVICT: std::cell::RefCell<Option<EvictHook>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn before_evict(dir: &Path) {
    BEFORE_EVICT.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(dir);
        }
    });
}

/// Write a file map of a newer basemind into `workspace`'s working view, cut short after its first
/// bytes: whatever follows the version need not decode as this version's map.
fn write_newer_head(workspace: &Path) {
    let mut index = Index::empty();
    index.schema_ver = crate::extract::SCHEMA_VER + 1;
    let encoded = rmp_serde::to_vec_named(&index).expect("encode index");
    let working = workspace.join(VIEWS_DIR).join("working");
    fs::create_dir_all(&working).expect("mk view");
    fs::write(working.join(INDEX_FILE), &encoded[..24]).expect("write head");
}

/// Seed `<workspaces>/<key>/` with a marker + a working view whose index references `stem`,
/// then force every activity-bearing file's mtime to `age` ago.
pub(super) fn seed_workspace_aged(workspaces_dir: &Path, key: &str, stem: &str, age: Duration) -> PathBuf {
    let workspace_dir = workspaces_dir.join(key);
    let working = workspace_dir.join(VIEWS_DIR).join("working");
    fs::create_dir_all(&working).expect("mk workspace view");
    let mut index = Index::empty();
    index.files.insert(
        crate::path::RelPath::from("src/main.rs"),
        FileEntry {
            hash_hex: stem.to_string(),
            language: "rust".to_string(),
            size_bytes: 2,
            mtime: 0,
            extract_epoch: crate::extract::EXTRACT_EPOCH,
        },
    );
    fs::write(
        working.join(INDEX_FILE),
        rmp_serde::to_vec_named(&index).expect("encode index"),
    )
    .expect("write index");
    ensure_workspace_marker(&workspace_dir, workspaces_dir);
    drop(acquire_lock(&workspace_dir).expect("seed workspace lock files"));

    let stamp = SystemTime::now() - age;
    for file in [workspace_dir.join(WORKSPACE_MARKER_FILE), working.join(INDEX_FILE)] {
        fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|f| f.set_modified(stamp))
            .expect("age file");
    }
    workspace_dir
}

#[test]
fn under_budget_is_a_no_op() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let blobs = tmp.path().join("blobs");
    fs::create_dir_all(&blobs).expect("mk blobs");
    let ws = seed_workspace_aged(&workspaces, "key-a", &"a".repeat(64), Duration::from_secs(9999));

    let report = enforce_cache_budget_in(&workspaces, &blobs, u64::MAX, Duration::ZERO, Duration::ZERO)
        .expect("enforce under budget");

    assert_eq!(report.evicted, 0, "under budget must evict nothing");
    assert_eq!(report.bytes_freed, 0);
    assert!(report.total_bytes_before > 0, "footprint is measured");
    assert_eq!(report.total_bytes_after, report.total_bytes_before);
    assert!(ws.exists(), "workspace untouched");
}

#[test]
fn over_budget_prefers_a_cold_workspace_and_stops_before_a_hot_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let blobs = tmp.path().join("blobs");
    fs::create_dir_all(&blobs).expect("mk blobs");

    let cold = seed_workspace_aged(
        &workspaces,
        "key-cold",
        &"a".repeat(64),
        Duration::from_secs(10 * 24 * 3600),
    );
    let warm = seed_workspace_aged(&workspaces, "key-warm", &"b".repeat(64), Duration::from_secs(60));

    let total = dir_size(&workspaces).expect("size");
    // Any successful cache eviction is sufficient. Directory inode accounting differs
    // between filesystems, so do not predict the exact bytes a removed view will free.
    let budget = total - 1;

    let report = enforce_cache_budget_in(
        &workspaces,
        &blobs,
        budget,
        Duration::from_secs(24 * 3600),
        Duration::ZERO,
    )
    .expect("enforce");

    assert_eq!(report.evicted, 1, "one eviction suffices to reach the budget");
    assert_eq!(report.hot_evicted, 0, "the cold candidate is preferred");
    assert!(report.total_bytes_after <= budget, "the measured footprint converged");
    assert!(cold.exists(), "the cold workspace keeps its metadata shell");
    assert!(!cold.join(VIEWS_DIR).exists(), "the cold workspace payload is evicted");
    assert!(warm.exists(), "the warmer workspace survives");
    assert!(report.bytes_freed > 0, "the rebuildable payload is reclaimed");
}

#[test]
fn a_hot_workspace_is_evicted_when_it_is_the_only_way_to_converge() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let blobs = tmp.path().join("blobs");
    fs::create_dir_all(&workspaces).expect("mk workspaces");
    fs::create_dir_all(&blobs).expect("mk blobs");
    let empty_footprint = 0;

    let hot = seed_workspace_aged(&workspaces, "key-hot", &"a".repeat(64), Duration::from_secs(60));
    // A strictly newer workspace holds the most-recently-used protection.
    let _newest = seed_workspace_aged(&workspaces, "key-newest", &"b".repeat(64), Duration::ZERO);

    let report = enforce_cache_budget_in(
        &workspaces,
        &blobs,
        empty_footprint,
        Duration::from_secs(24 * 3600),
        Duration::ZERO,
    )
    .expect("enforce with hot floor");

    assert_eq!(report.evicted, 1, "the hot floor is a preference, not a budget veto");
    assert_eq!(report.hot_evicted, 1);
    assert_eq!(report.protected.len(), 1, "the newest workspace is kept and reported");
    assert!(
        report.total_bytes_after < report.total_bytes_before,
        "rebuildable bytes are reclaimed even when durable metadata prevents full convergence"
    );
    assert!(hot.exists(), "the workspace metadata shell preserves stable identity");
    assert!(!hot.join(VIEWS_DIR).exists(), "only rebuildable view state is evicted");
}

#[test]
fn budget_eviction_keeps_group_memory_readable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let blobs = tmp.path().join("blobs");
    fs::create_dir_all(&blobs).expect("mk blobs");
    let memory_dir = tmp
        .path()
        .join("repos")
        .join("group")
        .join(crate::store::MEMORY_STORE_DIR);
    let cold = seed_workspace_aged(&workspaces, "key-cold", &"a".repeat(64), Duration::from_secs(9999));
    let _newest = seed_workspace_aged(&workspaces, "key-newest", &"b".repeat(64), Duration::from_secs(60));
    {
        let db = crate::index::IndexDb::open_with_memory(&cold.join(VIEWS_DIR).join("working"), &memory_dir)
            .expect("open index");
        db.notes().insert(b"k".to_vec(), b"v".to_vec()).expect("write memory");
    }

    let report =
        enforce_cache_budget_in(&workspaces, &blobs, 1, Duration::ZERO, Duration::ZERO).expect("evict cache payload");

    assert_eq!(report.evicted, 1);
    assert!(!cold.join(VIEWS_DIR).exists(), "the cold workspace's views are evicted");
    let db = crate::index::IndexDb::open_with_memory(&cold.join(VIEWS_DIR).join("working"), &memory_dir)
        .expect("reopen after eviction");
    assert_eq!(db.notes().get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
}

#[test]
fn budget_eviction_preserves_identity_and_durable_memory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let blobs = tmp.path().join("blobs");
    fs::create_dir_all(&blobs).expect("mk blobs");
    let workspace = seed_workspace_aged(&workspaces, "key-cold", &"a".repeat(64), Duration::from_secs(9999));
    let _newest = seed_workspace_aged(&workspaces, "key-newest", &"b".repeat(64), Duration::from_secs(60));
    fs::write(workspace.join(crate::store_layout::AGENT_ID_FILE), b"session-stable").expect("agent id");
    let memory = workspace.join(LANCE_WORKSPACE_DIR).join("memory.lance");
    fs::create_dir_all(&memory).expect("memory table");
    fs::write(memory.join("data.bin"), b"durable-memory").expect("memory data");

    let report =
        enforce_cache_budget_in(&workspaces, &blobs, 1, Duration::ZERO, Duration::ZERO).expect("evict cache payload");

    assert_eq!(report.evicted, 1);
    assert_eq!(
        fs::read_to_string(workspace.join(crate::store_layout::AGENT_ID_FILE)).unwrap(),
        "session-stable"
    );
    assert_eq!(fs::read(memory.join("data.bin")).unwrap(), b"durable-memory");
    assert!(workspace.join(WORKSPACE_MARKER_FILE).exists(), "root marker survives");
    assert!(!workspace.join(VIEWS_DIR).exists(), "rebuildable views are removed");
}

#[test]
fn reclaiming_an_evicted_workspaces_blobs_prevents_an_extra_eviction() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let blobs = tmp.path().join("blobs");
    fs::create_dir_all(&blobs).expect("mk blobs");

    let cold_stem = "c".repeat(64);
    let cold = seed_workspace_aged(&workspaces, "key-cold", &cold_stem, Duration::from_secs(10 * 24 * 3600));
    let warm = seed_workspace_aged(
        &workspaces,
        "key-warm",
        &"d".repeat(64),
        Duration::from_secs(2 * 24 * 3600),
    );
    let cold_blob = blobs.join(format!("{cold_stem}.fm.msgpack"));
    fs::write(&cold_blob, vec![7_u8; 64 * 1024]).expect("write cold workspace blob");

    let total = dir_size(&workspaces).unwrap() + dir_size(&blobs).unwrap();
    let reclaimable = dir_size(&cold.join(VIEWS_DIR)).unwrap() + fs::metadata(&cold_blob).unwrap().len();
    let budget = total - reclaimable + 1;

    let report = enforce_cache_budget_in(&workspaces, &blobs, budget, Duration::ZERO, Duration::ZERO).expect("enforce");

    assert_eq!(
        report.evicted, 1,
        "blob reclamation should make a second eviction unnecessary"
    );
    assert_eq!(report.blobs_removed, 1);
    assert_eq!(report.blob_bytes_freed, 64 * 1024);
    assert!(report.total_bytes_after <= budget);
    assert!(cold.exists(), "the evicted workspace keeps durable metadata");
    assert!(!cold.join(VIEWS_DIR).exists());
    assert!(
        !cold_blob.exists(),
        "the evicted workspace's orphan blob is reclaimed in the same pass"
    );
    assert!(
        warm.exists(),
        "the warmer workspace survives once the measured footprint converges"
    );
}

#[test]
fn a_locked_workspace_is_skipped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let blobs = tmp.path().join("blobs");
    fs::create_dir_all(&blobs).expect("mk blobs");

    let locked = seed_workspace_aged(&workspaces, "key-locked", &"a".repeat(64), Duration::from_secs(9999));
    let _newest = seed_workspace_aged(&workspaces, "key-newest", &"b".repeat(64), Duration::from_secs(60));
    let _held = acquire_lock(&locked).expect("hold the workspace lock");

    let report = enforce_cache_budget_in(&workspaces, &blobs, 1, Duration::ZERO, Duration::ZERO).expect("enforce");

    assert_eq!(report.locked_skipped, 1);
    assert!(report.protected.iter().any(|ws| ws.reason == ProtectionReason::Locked));
    assert!(report.total_bytes_after > 1, "the report exposes non-convergence");
    assert!(locked.exists());
}

#[test]
fn gc_state_round_trips_and_tolerates_absence() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("cache").join(GC_STATE_FILE);

    assert!(read_gc_state_at(&path).is_none(), "absent state reads as None");

    let report = GcReport {
        scanned: 10,
        removed: 3,
        bytes_freed: 4096,
        workspaces_reaped: 1,
        workspace_bytes_freed: 2048,
        workspaces_evicted: 2,
        evicted_bytes_freed: 8192,
        ..GcReport::default()
    };
    persist_gc_state_at(&path, &report);

    let state = read_gc_state_at(&path).expect("state persisted");
    assert!(state.at_epoch_secs > 0, "timestamp recorded");
    assert_eq!(state.removed, 3);
    assert_eq!(state.bytes_freed, 4096);
    assert_eq!(state.workspaces_reaped, 1);
    assert_eq!(state.workspaces_evicted, 2);
    assert_eq!(state.evicted_bytes_freed, 8192);
    assert_eq!(state.status, GcStatus::Completed);
    assert_eq!(state.last_attempt_epoch_secs, state.at_epoch_secs);
    assert_eq!(state.consecutive_degraded_cycles, 0);

    fs::write(&path, b"{ not json").expect("corrupt");
    assert!(read_gc_state_at(&path).is_none(), "corrupt state degrades to None");
}

#[test]
fn an_over_budget_sweep_is_persisted_as_degraded_until_a_healthy_sweep_completes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(GC_STATE_FILE);
    let constrained = GcReport {
        cache_budget_bytes: Some(100),
        cache_bytes_after: Some(180),
        locked_workspaces_skipped: 2,
        hot_workspaces_evicted: 1,
        ..GcReport::default()
    };

    persist_gc_state_at(&path, &constrained);

    let degraded = read_gc_state_at(&path).expect("degraded state persisted");
    assert_eq!(degraded.status, GcStatus::OverBudget);
    assert_eq!(degraded.consecutive_degraded_cycles, 1);
    assert_eq!(degraded.cache_budget_bytes, Some(100));
    assert_eq!(degraded.cache_bytes_after, Some(180));
    assert_eq!(degraded.locked_workspaces_skipped, 2);
    assert_eq!(degraded.hot_workspaces_evicted, 1);
    assert!(
        degraded
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("80 bytes over budget")),
        "the persisted diagnosis is actionable: {:?}",
        degraded.detail
    );

    persist_gc_state_at(&path, &GcReport::default());

    let recovered = read_gc_state_at(&path).expect("healthy state persisted");
    assert_eq!(recovered.status, GcStatus::Completed);
    assert_eq!(recovered.consecutive_degraded_cycles, 0);
    assert_eq!(recovered.detail, None);
}

#[test]
fn failed_attempts_preserve_the_last_success_and_accumulate_until_recovery() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(GC_STATE_FILE);
    let completed = GcReport {
        scanned: 12,
        removed: 4,
        bytes_freed: 4096,
        ..GcReport::default()
    };
    persist_gc_state_at(&path, &completed);
    let completed_at = read_gc_state_at(&path).expect("completed state").at_epoch_secs;

    persist_gc_error_at(&path, &GcError::Starved(Duration::from_secs(300)));
    persist_gc_error_at(&path, &GcError::Join("maintenance worker panicked".to_owned()));

    let failed = read_gc_state_at(&path).expect("failed state persisted");
    assert_eq!(failed.status, GcStatus::Failed);
    assert_eq!(failed.consecutive_degraded_cycles, 2);
    assert_eq!(
        failed.at_epoch_secs, completed_at,
        "last successful timestamp is preserved"
    );
    assert_eq!(failed.scanned, 12, "last successful counters are preserved");
    assert_eq!(failed.removed, 4);
    assert_eq!(failed.bytes_freed, 4096);
    assert!(
        failed
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("maintenance worker panicked")),
        "the latest failure replaces the prior diagnosis: {:?}",
        failed.detail
    );
}

#[test]
fn legacy_gc_state_defaults_attempt_health_fields() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(GC_STATE_FILE);
    fs::write(
        &path,
        br#"{
            "at_epoch_secs": 42,
            "scanned": 5,
            "removed": 1,
            "bytes_freed": 128,
            "workspaces_reaped": 0,
            "workspace_bytes_freed": 0
        }"#,
    )
    .expect("write legacy state");

    let state = read_gc_state_at(&path).expect("legacy state remains readable");
    assert_eq!(state.at_epoch_secs, 42);
    assert_eq!(state.status, GcStatus::Completed);
    assert_eq!(state.last_attempt_epoch_secs, 0);
    assert_eq!(state.consecutive_degraded_cycles, 0);
    assert_eq!(state.detail, None);
}

#[test]
fn cache_budget_env_parses_default_zero_and_garbage() {
    const VAR: &str = CACHE_BUDGET_ENV;
    // SAFETY: test-only env mutation; no other thread in this test binary reads this var
    unsafe { std::env::remove_var(VAR) };
    assert_eq!(cache_budget_bytes(), Some(DEFAULT_CACHE_BUDGET_MIB * 1024 * 1024));
    unsafe { std::env::set_var(VAR, "0") };
    assert_eq!(cache_budget_bytes(), None, "0 disables enforcement");
    unsafe { std::env::set_var(VAR, "512") };
    assert_eq!(cache_budget_bytes(), Some(512 * 1024 * 1024));
    unsafe { std::env::set_var(VAR, "not-a-number") };
    assert_eq!(cache_budget_bytes(), None, "garbage disables enforcement (logged)");
    unsafe { std::env::remove_var(VAR) };
}

/// A pass within budget evicts nothing, so it has no reason to read any workspace's file map.
#[test]
fn a_pass_within_budget_reads_no_file_map() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (workspaces, blobs) = (tmp.path().join("workspaces"), tmp.path().join("blobs"));
    fs::create_dir_all(&blobs).expect("mk blobs");
    let newer = seed_workspace_aged(&workspaces, "key-newer", &"a".repeat(64), DAY_AGO);
    write_newer_head(&newer);
    seed_workspace_aged(&workspaces, "key-other", &"b".repeat(64), DAY_AGO);

    VIEW_READS.with(|reads| reads.set(0));
    let report = enforce_cache_budget_in(&workspaces, &blobs, u64::MAX, DAY_AGO, Duration::ZERO).expect("enforce");

    assert_eq!(report.evicted, 0);
    assert_eq!(VIEW_READS.with(Cell::get), 0, "no view was read");
}

/// The version is the file map's first field: a newer map is told from its head, however long the
/// rest and whether or not the rest decodes as this version's map.
#[test]
fn a_newer_file_map_is_told_from_its_head() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().join("key-newer");
    write_newer_head(&workspace);
    let view = workspace.join(VIEWS_DIR).join("working");

    assert!(view_is_newer(&view));
    assert!(holds_a_newer_view(&workspace));
    let current = rmp_serde::to_vec_named(&Index::empty()).expect("encode index");
    fs::write(view.join(INDEX_FILE), current).expect("write current");
    assert!(!view_is_newer(&view), "a map of this version");
    assert_eq!(
        leading_schema_ver(&[
            0x81, 0xaa, b's', b'c', b'h', b'e', b'm', b'a', b'_', b'v', b'e', b'r', 0xcd, 1, 2
        ]),
        Some(0x0102)
    );
    assert_eq!(
        leading_schema_ver(&[0x81, 0xa5, b'f', b'i', b'l', b'e', b's', 0x01]),
        None
    );
}

/// A newer basemind writes a view into the coldest workspace while the pass runs. The eviction is
/// refused, and those bytes are no longer the pass's to count: counted, they would cost another
/// workspace of this version its index.
#[test]
fn a_workspace_a_newer_basemind_took_during_the_pass_costs_no_other_workspace() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (workspaces, blobs) = (tmp.path().join("workspaces"), tmp.path().join("blobs"));
    fs::create_dir_all(&blobs).expect("mk blobs");
    let taken = seed_workspace_aged(&workspaces, "key-taken", &"a".repeat(64), 9 * DAY_AGO);
    fs::write(taken.join("git-cache"), vec![0_u8; 256 * 1024]).expect("bulk");
    let idle = seed_workspace_aged(&workspaces, "key-idle", &"c".repeat(64), 8 * DAY_AGO);
    seed_workspace_aged(&workspaces, "key-mru", &"b".repeat(64), 3 * DAY_AGO);
    let target = taken.clone();
    BEFORE_EVICT.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |dir: &Path| {
            if dir == target {
                write_newer_head(dir);
            }
        }));
    });

    let report = enforce_cache_budget_in(&workspaces, &blobs, 128 * 1024, DAY_AGO, Duration::ZERO);
    BEFORE_EVICT.with(|hook| hook.borrow_mut().take());
    let report = report.expect("enforce");

    assert!(
        report
            .protected
            .iter()
            .any(|ws| ws.key == "key-taken" && ws.reason == ProtectionReason::NewerVersion),
        "{:?}",
        report.protected
    );
    assert_eq!(
        report.evicted, 0,
        "within budget once the taken workspace is not counted"
    );
    assert!(idle.join(VIEWS_DIR).exists(), "the idle workspace keeps its index");
    assert!(report.total_bytes_after <= 128 * 1024, "{}", report.total_bytes_after);
}
