use super::*;

fn git(root: &Path, args: &[&str]) -> String {
    let out = crate::testing::git_command()
        .args(args)
        .current_dir(root)
        .output()
        .expect("git in PATH");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).expect("utf8")
}

/// A repository of `commits` commits, under an isolated cache.
fn repository(commits: usize) -> tempfile::TempDir {
    crate::store::init_isolated_cache();
    let repo = tempfile::tempdir().expect("repo");
    let root = repo.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    for n in 0..commits {
        std::fs::write(root.join(format!("f{n}.rs")), "1").expect("write");
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", &format!("c{n}")]);
    }
    repo
}

#[test]
fn the_summary_line_names_the_commits_and_the_worker_that_builds_them() {
    let cases = [
        (
            Background::Started,
            "git-history: building in the background (42 commits; progress: `git_history_build` in `basemind status`)",
        ),
        (
            Background::AlreadyRunning,
            "git-history: building in the background (42 commits, by the background pass already running)",
        ),
    ];
    for (background, line) in cases {
        assert_eq!(summary_line(42, background), line, "{background:?}");
    }
}

#[test]
fn the_json_carries_the_commits_and_the_hand_off() {
    assert_eq!(
        json(42, Background::Started),
        serde_json::json!({"pending_commits": 42, "background": "started"})
    );
}

/// Only a worker that started or already runs takes the build; otherwise the record is withdrawn
/// and the scan builds the index itself.
#[test]
fn a_build_no_worker_takes_is_withdrawn() {
    let cases = [
        (Background::Started, true),
        (Background::AlreadyRunning, true),
        (Background::Disabled, false),
        (Background::Unavailable, false),
        (Background::NotNeeded, false),
    ];
    for (background, expected) in cases {
        let dir = tempfile::tempdir().expect("dir");
        progress::record_owed(dir.path(), 5);
        assert_eq!(taken(dir.path(), background), expected, "{background:?}");
        assert_eq!(progress::is_owed(dir.path()), expected, "{background:?}");
    }
}

#[test]
fn only_a_from_scratch_build_is_owed_and_it_counts_the_commits_head_reaches() {
    let repo_dir = repository(3);
    let repo = Repo::discover(repo_dir.path()).expect("discover");
    let cache = tempfile::tempdir().expect("cache");
    let index = GitHistoryIndex::open(cache.path()).expect("open");
    assert_eq!(owed(&index, &repo), Some(3), "never built");
    builder::sync(&index, &repo, cache.path()).expect("build");
    assert_eq!(owed(&index, &repo), None, "HEAD is indexed");
}

#[test]
fn nothing_recorded_builds_nothing() {
    let repo_dir = repository(1);
    let root = repo_dir.path();
    assert_eq!(run(root, &Asks::of(root)), Phase::NotOwed);
    let basemind_dir = crate::git_history::shared_history_basemind_dir(root);
    assert!(
        !GitHistoryIndex::exists(&basemind_dir),
        "a scan run with --no-git-history hands the worker no build"
    );
}

#[test]
fn an_owed_build_is_built_and_its_record_removed() {
    let repo_dir = repository(3);
    let root = repo_dir.path();
    let basemind_dir = crate::git_history::shared_history_basemind_dir(root);
    progress::record_owed(&basemind_dir, 3);

    assert_eq!(run(root, &Asks::of(root)), Phase::Built(3));
    assert!(!progress::is_owed(&basemind_dir), "the built index owes nothing");
    let index = GitHistoryIndex::open(&basemind_dir).expect("the worker released the index");
    let head = git(root, &["rev-parse", "HEAD"]);
    assert!(index.has_tip(head.trim()), "HEAD is indexed");
    assert!(
        crate::store_lock::try_acquire_lock_as(&Asks::of(root).workspace, LockHolder::Scan).is_ok(),
        "the worker released the store lock"
    );
}

/// The worker leaves the build to a `serve` that holds the store lock, which builds the index
/// itself, and never opens the database that serve needs.
#[test]
fn a_long_lived_writer_holding_the_lock_stops_the_phase() {
    let repo_dir = repository(1);
    let root = repo_dir.path();
    let basemind_dir = crate::git_history::shared_history_basemind_dir(root);
    progress::record_owed(&basemind_dir, 1);
    let asks = Asks::of(root);
    let _serve = crate::store_lock::acquire_lock_as(&asks.workspace, LockHolder::Serve).expect("serve takes the lock");

    assert_eq!(run(root, &asks), Phase::Stopped);
    assert!(progress::is_owed(&basemind_dir), "the build is still owed");
    assert!(
        !GitHistoryIndex::exists(&basemind_dir),
        "the worker never opened the index"
    );
}

#[test]
fn the_build_stops_once_a_writer_asks_for_the_lock_and_records_its_progress() {
    let tmp = tempfile::tempdir().expect("tmp");
    let asks = Asks {
        workspace: tmp.path().join("workspace"),
        group: tmp.path().join("group"),
    };
    std::fs::create_dir_all(&asks.workspace).expect("workspace");
    std::fs::create_dir_all(&asks.group).expect("group");
    let control = Yielding {
        asks: &asks,
        basemind_dir: tmp.path(),
        started_unix_ms: crate::embed_progress::now_unix_ms(),
    };
    assert!(!control.stop(), "nobody asked");
    std::fs::write(asks.workspace.join("embed-yield"), std::process::id().to_string()).expect("ask");
    assert!(control.stop(), "a live writer asked");

    control.progress(8192, 20_000);
    let status = progress::status(tmp.path(), crate::embed_progress::now_unix_ms()).expect("a record");
    assert_eq!((status.done, status.total), (8192, 20_000));
}
