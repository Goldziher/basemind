//! Unit tests for the daemon's git-history handle pool. `super` is `git_history_ops`.

use super::*;

fn temp_broker() -> (tempfile::TempDir, Arc<Broker>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let agents = crate::comms::agents_hub::AgentHub::open(dir.path());
    (dir, Arc::new(Broker::new(agents).expect("broker")))
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

/// Every commit read, at `head`.
fn reads_at(head: Option<&str>) -> Vec<GitHistoryOp> {
    let head = head.map(str::to_owned);
    vec![
        GitHistoryOp::RecentCommits {
            skip: 0,
            take: 10,
            include_files: false,
            head: head.clone(),
        },
        GitHistoryOp::CommitsTouching {
            path: crate::path::RelPath::from("a.rs".as_bytes()),
            skip: 0,
            take: 10,
            head: head.clone(),
        },
        GitHistoryOp::WindowCommits {
            window: 10,
            head: head.clone(),
        },
        GitHistoryOp::SearchCommits {
            query: "c1".to_string(),
            scope: crate::git_history::fts::FtsScope::All,
            skip: 0,
            take: 10,
            head,
        },
    ]
}

/// The index reports the latest tip as indexed while that tip's order is unproven, so it has no
/// ordered view of it. A read there used to succeed with no commits, a reply indistinguishable from
/// a path with no history; it is refused as not ready instead, at the latest tip and at the tip
/// named, and so is a read at a head that is no tip at all.
#[test]
fn a_read_with_no_ordered_view_is_refused_not_answered_with_no_commits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = GitHistoryIndex::open(dir.path()).expect("open history");
    let tip = [0x33u8; 20];
    let tip_hex = crate::git_history::keys::sha_raw_to_hex(&tip);
    let mut writer = index.writer().expect("writer");
    writer.set_ordered(false);
    writer
        .finish_meta(&tip, &crate::git_history::reach::Reach::from_ords([0]), &tip, 1, 0, 1)
        .expect("stamp an unordered tip");
    let other = "44".repeat(20);

    let Ok(GitHistoryReply::IndexedHead(indexed)) =
        run_git_history_op(&index, dir.path(), dir.path(), GitHistoryOp::IndexedHead)
    else {
        panic!("IndexedHead answers");
    };
    assert_eq!(
        indexed.as_deref(),
        Some(tip_hex.as_str()),
        "the unordered tip reads as indexed"
    );

    for head in [None, Some(tip_hex.as_str()), Some(other.as_str())] {
        for op in reads_at(head) {
            let what = format!("{op:?}");
            match run_git_history_op(&index, dir.path(), dir.path(), op) {
                Err(GitHistoryError::NoOrderedView { head: refused }) => {
                    assert_eq!(refused.as_deref(), head, "{what}");
                }
                reply => panic!("{what}: expected NoOrderedView, got {reply:?}"),
            }
        }
    }
}

/// A read of an index that was never built is refused the same way, while its indexed head is none.
#[test]
fn a_read_of_an_unbuilt_index_is_refused_and_its_indexed_head_is_none() {
    assert!(matches!(
        unbuilt_reply(GitHistoryOp::IndexedHead),
        Ok(GitHistoryReply::IndexedHead(None))
    ));
    for op in reads_at(None) {
        let what = format!("{op:?}");
        assert!(
            matches!(unbuilt_reply(op), Err(GitHistoryError::NoOrderedView { head: None })),
            "{what}"
        );
    }
}
