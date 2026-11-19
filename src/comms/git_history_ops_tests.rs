//! Unit tests for the daemon's git-history handle pool. `super` is `git_history_ops`.

use super::*;

fn temp_broker() -> (tempfile::TempDir, Arc<Broker>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let agents = crate::comms::agents_hub::AgentHub::open(dir.path());
    (dir, Arc::new(Broker::new(agents)))
}

/// Opening one history more than the cap closes the least recently used handle nobody holds, keeps
/// one a holder still uses even though it is older, and releases the closed database's fjall lock so
/// it can be opened again.
#[test]
fn the_history_pool_closes_the_least_recently_used_handle_past_its_cap() {
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();
    let repos = tempfile::tempdir().expect("repos");
    let dirs: Vec<_> = (0..=MAX_OPEN_HISTORIES)
        .map(|i| repos.path().join(format!("repo-{i}")))
        .collect();

    let mut held = None;
    for (i, dir) in dirs.iter().enumerate() {
        let index = GitHistoryIndex::open(dir).expect("open history");
        let (entry, shed) = broker.insert_history(dir, index);
        drop(shed);
        if i == 0 {
            held = Some(entry);
        }
        // `idle_for` orders the entries; keep their instants distinct.
        std::thread::sleep(Duration::from_millis(2));
    }

    let open = broker.git_history.lock().expect("map").len();
    assert_eq!(open, MAX_OPEN_HISTORIES, "the pool stays at its cap");
    assert!(
        broker.history_lookup(&dirs[0]).is_some(),
        "a held handle is never closed"
    );
    assert!(
        broker.history_lookup(&dirs[1]).is_none(),
        "the least recently used free handle is the one closed"
    );
    for dir in &dirs[2..] {
        assert!(broker.history_lookup(dir).is_some(), "{} stays open", dir.display());
    }
    GitHistoryIndex::open(&dirs[1]).expect("the closed database's lock was released");
    drop(held);
}
