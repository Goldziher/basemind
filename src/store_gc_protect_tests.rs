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
