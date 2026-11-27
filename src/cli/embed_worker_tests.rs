use super::*;

const DOCUMENTS: Pending = Pending {
    documents: 3,
    code: false,
    policy: false,
};

#[test]
fn nothing_owed_starts_no_worker_and_prints_no_line() {
    let pending = Pending::default();
    assert!(!pending.any());
    assert_eq!(hand_off(Path::new("/nonexistent/repo"), pending), Background::NotNeeded);
    assert_eq!(summary_line(pending, Background::NotNeeded), None);
}

#[test]
fn a_running_worker_is_not_started_twice() {
    crate::store::init_isolated_cache();
    let repo = tempfile::tempdir().expect("repo");
    let cache_dir = crate::store::workspace_cache_dir(repo.path());
    let held = WorkerLock::take(&cache_dir).expect("open").expect("no worker runs yet");
    assert!(worker_running(&cache_dir));
    assert!(
        WorkerLock::take(&cache_dir).expect("open").is_none(),
        "a second worker finds the lock held and exits"
    );
    if !background_disabled(std::env::var(EMBED_BACKGROUND_ENV).ok().as_deref()) {
        assert_eq!(hand_off(repo.path(), DOCUMENTS), Background::AlreadyRunning);
    }
    drop(held);
    assert!(!worker_running(&cache_dir), "the lock goes with the worker");
}

#[test]
fn only_a_false_value_turns_the_worker_off() {
    for (value, disabled) in [
        (None, false),
        (Some(""), false),
        (Some("1"), false),
        (Some("on"), false),
        (Some("0"), true),
        (Some("off"), true),
        (Some("false"), true),
        (Some("maybe"), false),
    ] {
        assert_eq!(background_disabled(value), disabled, "{value:?}");
    }
}

#[test]
fn the_summary_line_names_what_is_owed_and_where_it_goes() {
    let cases = [
        (
            DOCUMENTS,
            Background::Started,
            "embeddings: pending 3 documents, embedding in the background (progress: `embeddings` in `basemind status`)",
        ),
        (
            Pending {
                code: true,
                ..DOCUMENTS
            },
            Background::AlreadyRunning,
            "embeddings: pending 3 documents and code chunk vectors, a background embedding pass is already running and \
             takes them",
        ),
        (
            Pending {
                documents: 0,
                code: true,
                policy: false,
            },
            Background::Disabled,
            "embeddings: pending code chunk vectors, background embedding is off (BASEMIND_EMBED_BACKGROUND); run \
             `basemind embed-worker`",
        ),
        (
            Pending {
                documents: 0,
                code: false,
                policy: true,
            },
            Background::Unavailable,
            "embeddings: pending vector rows of the changed embed policy, the background pass did not start; run \
             `basemind embed-worker`",
        ),
    ];
    for (pending, background, line) in cases {
        assert_eq!(summary_line(pending, background).as_deref(), Some(line));
    }
}

#[test]
fn the_json_carries_the_pending_counts_and_the_hand_off() {
    assert_eq!(
        json(DOCUMENTS, Background::Started),
        serde_json::json!({
            "pending_documents": 3,
            "pending_code": false,
            "pending_policy": false,
            "background": "started",
        })
    );
}

#[test]
fn a_poll_trips_the_pass_once_a_writer_asks_and_not_before() {
    let tmp = tempfile::tempdir().expect("tmp");
    let asks = Asks {
        root: tmp.path().to_path_buf(),
        workspace: tmp.path().join("workspace"),
        group: tmp.path().join("group"),
    };
    std::fs::create_dir_all(&asks.workspace).expect("workspace");
    std::fs::create_dir_all(&asks.group).expect("group");
    let cancel = ScanCancel::new();
    let poll = YieldPoll::start(asks.clone(), cancel.clone());
    std::thread::sleep(YIELD_POLL * 3);
    assert!(!cancel.is_cancelled(), "nobody asked");
    std::fs::write(asks.group.join("embed-yield"), std::process::id().to_string()).expect("ask");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cancel.is_cancelled() {
        assert!(Instant::now() < deadline, "the poll never tripped the pass");
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(poll);
}

#[test]
fn a_worker_ends_when_its_workspace_is_gone() {
    let tmp = tempfile::tempdir().expect("tmp");
    let asks = Asks {
        root: tmp.path().join("removed"),
        workspace: tmp.path().join("workspace"),
        group: tmp.path().join("group"),
    };
    let opened = take_writer(&asks, || -> Result<(), StoreError> { Ok(()) }).expect("no error");
    assert_eq!(opened, None, "a removed workspace is not opened");
}
