use super::*;

#[test]
fn a_walk_is_ordered_only_when_its_ordinals_strictly_descend() {
    // `Ok` is a stored ordinal, `Err(n)` the n-th unstored commit in walk order, which takes the
    // ordinals from `start` in reverse: with three unstored from 10, positions 0, 1, 2 are 12, 11, 10.
    let cases: [(&[Result<u32, usize>], bool); 6] = [
        (&[], true),
        (&[Err(0), Err(1), Err(2)], true),
        (&[Err(0), Ok(7), Ok(3)], true),
        (&[Err(1), Err(0)], false),
        (&[Ok(7), Err(0)], false),
        (&[Err(0), Ok(5), Ok(5)], false),
    ];
    for (walk, expected) in cases {
        let count = walk.iter().filter(|slot| slot.is_err()).count();
        assert_eq!(walk_descends(walk, 10, count), expected, "{walk:?}");
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = crate::testing::git_command()
        .args(args)
        .current_dir(root)
        .output()
        .expect("git in PATH");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).expect("utf8")
}

fn commit(root: &Path, path: &str, content: &str, msg: &str) {
    std::fs::write(root.join(path), content).expect("write the file");
    git(root, &["add", path]);
    git(root, &["commit", "-qm", msg]);
}

fn touching(index: &GitHistoryIndex, path: &str) -> Vec<String> {
    index
        .commits_touching(&RelPath::from(path.as_bytes()), 0, 100)
        .expect("read the posting list")
        .into_iter()
        .map(|c| c.sha)
        .collect()
}

/// A sync killed after the writer's first batch commit leaves the new commits' rows durable and
/// their postings and head not. The retry used to skip every commit it found a row for, so they
/// never reached a posting list and the index then claimed to be fresh.
#[test]
fn an_append_cut_off_after_a_batch_commit_is_rebuilt_not_skipped() {
    let repo_dir = tempfile::tempdir().expect("tempdir");
    let root = repo_dir.path();
    git(root, &["init", "-q"]);
    git(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    commit(root, "a.rs", "1", "c1");
    let cache = tempfile::tempdir().expect("cache dir");
    let index = GitHistoryIndex::open(cache.path()).expect("open the index");
    let repo = Repo::discover(root).expect("discover");
    sync(&index, &repo, cache.path()).expect("first build");

    commit(root, "a.rs", "2", "c2");
    commit(root, "a.rs", "3", "c3");
    let repo = Repo::discover(root).expect("rediscover");
    let last = index.last_indexed_head_hex().expect("an indexed head");
    let new_shas = repo.new_commit_shas(&last).expect("new commits");
    assert_eq!(new_shas.len(), 2);
    let chrono: Vec<&String> = new_shas.iter().rev().collect();
    {
        let mut writer = index.writer().expect("writer");
        writer.begin().expect("mark the append");
        let mut interner = PathInterner::new(&index, index.next_path_id());
        let mut postings = AHashMap::new();
        let mut term_postings = AHashMap::new();
        fold_chunked(
            &repo,
            &chrono,
            index.next_ord(),
            &mut interner,
            &mut postings,
            &mut term_postings,
            &mut Reach::default(),
            &mut writer,
            &Unattended,
        )
        .expect("fold the new commits");
        writer.flush().expect("commit the batch");
    }
    assert_eq!(
        index.last_indexed_head_hex(),
        None,
        "a cut-off append is never served as fresh"
    );

    let outcome = sync(&index, &repo, cache.path()).expect("resync");
    assert_eq!(
        outcome,
        RebuildOutcome::FullRebuild {
            reason: "interrupted",
            commits: 3
        }
    );
    let head = git(root, &["rev-parse", "HEAD"]);
    assert_eq!(index.last_indexed_head_hex().as_deref(), Some(head.trim()));
    let expected: Vec<String> = git(root, &["log", "--full-history", "--format=%H", "--", "a.rs"])
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(expected.len(), 3);
    assert_eq!(
        touching(&index, "a.rs"),
        expected,
        "every commit is in the posting list"
    );
}

/// A build that fails at a commit it cannot read fails again at that commit: a sync at the same
/// HEAD leaves the marked index to the live walk for an hour instead of walking the whole
/// history each time, then tries again.
#[test]
fn a_failed_build_is_not_retried_at_the_same_head_within_the_hour() {
    let repo_dir = tempfile::tempdir().expect("tempdir");
    let root = repo_dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    commit(root, "a.rs", "1", "c1");
    let cache = tempfile::tempdir().expect("cache dir");
    let index = GitHistoryIndex::open(cache.path()).expect("open the index");
    let repo = Repo::discover(root).expect("discover");
    let head = git(root, &["rev-parse", "HEAD"]);
    let head20 = keys::sha_hex_to_raw(head.trim()).expect("a sha");
    index
        .writer()
        .expect("writer")
        .begin()
        .expect("a build that was cut off");
    index.record_failed_build(&head20, now_unix());

    let outcome = sync(&index, &repo, cache.path()).expect("sync");
    assert!(matches!(outcome, RebuildOutcome::Skipped { .. }), "{outcome:?}");
    assert!(index.build_interrupted(), "the marked index is left as it was");

    index.record_failed_build(&head20, now_unix() - FAILED_BUILD_BACKOFF_SECS);
    let outcome = sync(&index, &repo, cache.path()).expect("sync");
    assert_eq!(
        outcome,
        RebuildOutcome::FullRebuild {
            reason: "interrupted",
            commits: 1
        }
    );
    assert_eq!(index.failed_build_at(&head20), None, "a stamped head clears the record");
}

/// An append cut off after a batch commit left a new path's id rows durable while the next free
/// id was never advanced. A retry that reused the id gave it to another path, whose history then
/// served the first path's commits. The cut-off index is rebuilt instead, so no id names two
/// paths and each path's history is its own.
#[test]
fn a_retry_after_a_cut_off_append_never_gives_one_path_id_to_two_paths() {
    let repo_dir = tempfile::tempdir().expect("tempdir");
    let root = repo_dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    commit(root, "a.rs", "1", "c1");
    let cache = tempfile::tempdir().expect("cache dir");
    let index = GitHistoryIndex::open(cache.path()).expect("open the index");
    sync(&index, &Repo::discover(root).expect("discover"), cache.path()).expect("first build");

    commit(root, "b.rs", "1", "adds b");
    let repo = Repo::discover(root).expect("rediscover");
    let last = index.last_indexed_head_hex().expect("an indexed head");
    let new_shas = repo.new_commit_shas(&last).expect("new commits");
    let chrono: Vec<&String> = new_shas.iter().rev().collect();
    {
        let mut writer = index.writer().expect("writer");
        writer.begin().expect("mark the append");
        let mut interner = PathInterner::new(&index, index.next_path_id());
        let (mut postings, mut term_postings) = (AHashMap::new(), AHashMap::new());
        fold_chunked(
            &repo,
            &chrono,
            index.next_ord(),
            &mut interner,
            &mut postings,
            &mut term_postings,
            &mut Reach::default(),
            &mut writer,
            &Unattended,
        )
        .expect("fold");
        writer.flush().expect("commit the batch: b.rs's id rows are durable");
    }
    commit(root, "c.rs", "1", "adds c");
    commit(root, "b.rs", "2", "edits b");
    let repo = Repo::discover(root).expect("rediscover");

    sync(&index, &repo, cache.path()).expect("retry");

    let local = index.local();
    let paths: Vec<RelPath> = (0..index.next_path_id())
        .map(|id| local.path_for_id(id).expect("every id names a path"))
        .collect();
    let distinct: ahash::AHashSet<&RelPath> = paths.iter().collect();
    assert_eq!(distinct.len(), paths.len(), "no id names two paths: {paths:?}");
    for path in ["a.rs", "b.rs", "c.rs"] {
        let expected: Vec<String> = git(root, &["log", "--full-history", "--format=%H", "--", path])
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(touching(&index, path), expected, "{path}'s history is its own");
    }
}

/// Every table file of the git-history database under `cache`, by path.
fn table_files(cache: &Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read the index dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.parent().and_then(Path::file_name) == Some("tables".as_ref()) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&cache.join(super::super::GIT_HISTORY_DIR), &mut out);
    out.sort();
    out
}

/// A one-commit append used to flush and major-compact all eight keyspaces, rewriting every
/// table of the history (seconds on a deep repository). It now leaves the tables as they were,
/// and the appended commit is still indexed after a reopen.
#[test]
fn a_one_commit_append_rewrites_no_table() {
    let repo_dir = tempfile::tempdir().expect("tempdir");
    let root = repo_dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    commit(root, "a.rs", "1", "c1");
    commit(root, "b.rs", "1", "c2");
    let cache = tempfile::tempdir().expect("cache dir");
    let index = GitHistoryIndex::open(cache.path()).expect("open the index");
    sync(&index, &Repo::discover(root).expect("discover"), cache.path()).expect("first build");
    let built = table_files(cache.path());
    assert!(
        built.len() >= 7,
        "the build compacted each written keyspace to a table: {built:?}"
    );

    commit(root, "a.rs", "2", "c3");
    let outcome = sync(&index, &Repo::discover(root).expect("rediscover"), cache.path()).expect("append");
    assert_eq!(outcome, RebuildOutcome::Incremental { added: 1 });
    assert_eq!(
        table_files(cache.path()),
        built,
        "the append flushed or compacted a table"
    );

    drop(index);
    let index = GitHistoryIndex::open(cache.path()).expect("reopen the index");
    let head = git(root, &["rev-parse", "HEAD"]);
    assert_eq!(index.last_indexed_head_hex().as_deref(), Some(head.trim()));
    let expected: Vec<String> = git(root, &["log", "--format=%H", "--", "a.rs"])
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(touching(&index, "a.rs"), expected);
}

/// A sync resolves HEAD once, then appends the tip. When HEAD moves in between, the append used
/// to walk from the moved HEAD and stamp the captured head with that walk's commits: nothing
/// appended, and the tip claiming another checkout's history. It walks from the captured head,
/// with an indexed ancestor tip (`side` forks off the synced HEAD) and without one (`side` forks
/// off an older commit).
#[test]
fn an_append_walks_the_head_it_was_given_not_a_head_moved_since() {
    for (fork_at, expected_len) in [("HEAD~1", 2), ("HEAD", 3)] {
        let repo_dir = tempfile::tempdir().expect("tempdir");
        let root = repo_dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        commit(root, "a.rs", "1", "c1");
        commit(root, "a.rs", "2", "c2");
        let cache = tempfile::tempdir().expect("cache dir");
        let index = GitHistoryIndex::open(cache.path()).expect("open the index");
        sync(&index, &Repo::discover(root).expect("discover"), cache.path()).expect("build main");

        git(root, &["checkout", "-q", "-b", "side", fork_at]);
        commit(root, "b.rs", "1", "side");
        let captured = git(root, &["rev-parse", "HEAD"]).trim().to_string();
        git(root, &["checkout", "-q", "main"]);

        let repo = Repo::discover(root).expect("rediscover");
        let outcome = append_tip(&index, &repo, &captured, &Unattended).expect("append the captured head");
        assert_eq!(outcome, RebuildOutcome::Incremental { added: 1 }, "fork at {fork_at}");
        let listed: Vec<String> = index
            .at(&captured)
            .expect("an ordered view of the captured head")
            .recent_commits(0, 10, false)
            .into_iter()
            .map(|c| c.sha)
            .collect();
        let expected: Vec<String> = git(root, &["log", "--format=%H", &captured])
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(expected.len(), expected_len, "fork at {fork_at}");
        assert_eq!(listed, expected, "fork at {fork_at}: the captured head's history");
    }
}

/// Syncing A, a diverged B, then A again leaves A the latest tip: a read at no head answers for
/// the checkout that synced last.
#[test]
fn syncing_an_indexed_tip_again_makes_it_the_latest_tip() {
    let repo_dir = tempfile::tempdir().expect("tempdir");
    let root = repo_dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    commit(root, "a.rs", "1", "c1");
    let cache = tempfile::tempdir().expect("cache dir");
    let index = GitHistoryIndex::open(cache.path()).expect("open the index");
    sync(&index, &Repo::discover(root).expect("discover"), cache.path()).expect("sync A");
    let a = git(root, &["rev-parse", "HEAD"]).trim().to_string();

    git(root, &["checkout", "-q", "-b", "other"]);
    commit(root, "b.rs", "1", "c2");
    let b = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    sync(&index, &Repo::discover(root).expect("rediscover"), cache.path()).expect("sync B");
    assert_eq!(index.last_indexed_head_hex().as_deref(), Some(b.as_str()));

    git(root, &["checkout", "-q", "main"]);
    let outcome = sync(&index, &Repo::discover(root).expect("rediscover"), cache.path()).expect("sync A again");
    assert_eq!(outcome, RebuildOutcome::Fresh);
    assert_eq!(index.last_indexed_head_hex().as_deref(), Some(a.as_str()));
    assert!(index.has_tip(&b), "B stays an indexed tip");
    assert_eq!(
        index
            .latest()
            .expect("a latest view")
            .recent_commits(0, 10, false)
            .len(),
        1,
        "a read at no head sees A's history, not B's"
    );
}

/// A repository at `root` with `commits` commits on `main`, one file each.
fn repository(root: &Path, commits: usize) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    for n in 0..commits {
        commit(root, &format!("f{n}.rs"), "1", &format!("c{n}"));
    }
}

/// A control that stops before the chunk it is asked about once `stop_after` chunks were reported,
/// and records every report.
struct Recording {
    stop_after: usize,
    reports: std::sync::Mutex<Vec<(u32, u32)>>,
}

impl Recording {
    fn stopping_after(stop_after: usize) -> Self {
        Self {
            stop_after,
            reports: std::sync::Mutex::default(),
        }
    }

    fn reports(&self) -> Vec<(u32, u32)> {
        self.reports.lock().expect("reports").clone()
    }
}

impl BuildControl for Recording {
    fn stop(&self) -> bool {
        self.reports.lock().expect("reports").len() >= self.stop_after
    }

    fn progress(&self, done: u32, total: u32) {
        self.reports.lock().expect("reports").push((done, total));
    }
}

/// The scan hands the background only a from-scratch rebuild; an append, a current index or a build
/// backing off stays with the sync it would have run inline.
#[test]
fn a_full_rebuild_is_due_only_for_an_index_that_has_to_be_built_from_scratch() {
    let repo_dir = tempfile::tempdir().expect("tempdir");
    let root = repo_dir.path();
    repository(root, 2);
    let cache = tempfile::tempdir().expect("cache dir");
    let index = GitHistoryIndex::open(cache.path()).expect("open the index");
    let repo = Repo::discover(root).expect("discover");
    assert_eq!(full_rebuild_due(&index, &repo), Some("initial"), "never built");

    sync(&index, &repo, cache.path()).expect("build");
    assert_eq!(full_rebuild_due(&index, &repo), None, "HEAD is indexed");

    commit(root, "a.rs", "1", "one more");
    let repo = Repo::discover(root).expect("rediscover");
    assert_eq!(full_rebuild_due(&index, &repo), None, "one new commit is appended");

    index.writer().expect("writer").begin().expect("a build cut off");
    assert_eq!(full_rebuild_due(&index, &repo), Some("interrupted"));

    let head = git(root, &["rev-parse", "HEAD"]);
    index.record_failed_build(&keys::sha_hex_to_raw(head.trim()).expect("a sha"), now_unix());
    assert_eq!(full_rebuild_due(&index, &repo), None, "a failed build backs off");
}

/// A build stopped by its control leaves nothing a reader trusts: no head, no tip, the index marked
/// in progress. It records no failure, so the next sync rebuilds it at once instead of leaving it to
/// the live walk for the hour, and clears the progress record a handed-off build left.
#[test]
fn a_stopped_build_is_untrusted_and_rebuilt_by_the_next_sync() {
    let repo_dir = tempfile::tempdir().expect("tempdir");
    let root = repo_dir.path();
    repository(root, 3);
    let cache = tempfile::tempdir().expect("cache dir");
    let index = GitHistoryIndex::open(cache.path()).expect("open the index");
    let repo = Repo::discover(root).expect("discover");
    let head = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    super::super::progress::record_owed(cache.path(), 3);

    let stopping = Recording::stopping_after(0);
    let Err(GitHistoryError::Cancelled) = sync_with(&index, &repo, cache.path(), &stopping) else {
        panic!("the build was asked to stop");
    };
    assert!(index.build_interrupted(), "the index stays marked in progress");
    assert!(index.is_empty());
    assert!(!index.has_tip(&head));
    assert!(index.at(&head).is_none(), "no reader is served from it");
    assert_eq!(
        index.failed_build_at(&keys::sha_hex_to_raw(&head).expect("a sha")),
        None
    );
    assert!(
        super::super::progress::is_owed(cache.path()),
        "a stopped build is still owed"
    );

    let outcome = sync(&index, &repo, cache.path()).expect("resync");
    assert_eq!(
        outcome,
        RebuildOutcome::FullRebuild {
            reason: "interrupted",
            commits: 3
        }
    );
    assert!(index.has_tip(&head));
    assert!(
        !super::super::progress::is_owed(cache.path()),
        "the built index owes nothing"
    );
}

/// A build reports how far it got before each chunk of commits, and counts what the rebuild
/// stores: the commits HEAD reaches, merges and side branches included.
#[test]
fn a_build_reports_its_progress_and_the_count_matches_what_it_stores() {
    let repo_dir = tempfile::tempdir().expect("tempdir");
    let root = repo_dir.path();
    repository(root, 2);
    git(root, &["checkout", "-q", "-b", "side"]);
    commit(root, "side.rs", "1", "side");
    git(root, &["checkout", "-q", "main"]);
    commit(root, "main.rs", "1", "main");
    git(root, &["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
    let cache = tempfile::tempdir().expect("cache dir");
    let index = GitHistoryIndex::open(cache.path()).expect("open the index");
    let repo = Repo::discover(root).expect("discover");
    assert_eq!(repo.reachable_commit_count().expect("count"), 5);

    let recording = Recording::stopping_after(usize::MAX);
    let outcome = sync_with(&index, &repo, cache.path(), &recording).expect("build");
    assert_eq!(
        outcome,
        RebuildOutcome::FullRebuild {
            reason: "initial",
            commits: 5
        }
    );
    assert_eq!(recording.reports(), [(0, 5)], "one chunk holds every commit");
}

#[test]
fn an_unborn_head_reaches_no_commit() {
    let repo_dir = tempfile::tempdir().expect("tempdir");
    git(repo_dir.path(), &["init", "-q"]);
    let repo = Repo::discover(repo_dir.path()).expect("discover");
    assert_eq!(repo.reachable_commit_count().expect("count"), 0);
}
