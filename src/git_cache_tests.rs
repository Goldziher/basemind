use super::*;
use crate::git::BlameHunk;
use crate::path::RelPath;

/// One log entry whose charge is dominated by `body_len`: a commit message is unbounded, and it
/// is exactly the kind of payload an entry cap cannot see.
fn commits(sha: &str, body_len: usize) -> Arc<Vec<CommitInfo>> {
    Arc::new(vec![CommitInfo {
        sha: sha.to_string(),
        short_sha: sha.chars().take(7).collect(),
        summary: "summary line".to_string(),
        author: "author".to_string(),
        author_email: "author@example.com".to_string(),
        author_time_unix: 0,
        body: "b".repeat(body_len),
        files: vec![(RelPath::from("src/git_cache.rs"), ChangeKind::Modified)],
    }])
}

fn log_key(head_sha: &str) -> LogKey {
    LogKey {
        head_sha: head_sha.to_string(),
        path: None,
        limit: 10,
        include_files: false,
    }
}

/// A blame of a file whose every line came from a different commit — one hunk per line, the
/// shape that makes a single blame arbitrarily large.
fn blame_result(hunks: usize) -> BlameResult {
    BlameResult {
        path: RelPath::from("src/git_cache.rs"),
        suspect_sha: "a".repeat(40),
        hunks: (0..hunks)
            .map(|i| BlameHunk {
                commit_sha: format!("{i:040x}"),
                short_sha: format!("{i:07x}"),
                start_line: 1,
                len: 1,
                source_start_line: 1,
                author: "author".to_string(),
                author_time_unix: 0,
                summary: "one line changed".to_string(),
                source_path: None,
            })
            .collect(),
        truncated_reason: None,
    }
}

/// The charge must scale with a value's own heap, not with the entry count — the whole reason
/// the second bound is in bytes. A 100 KB commit body charges ~100 KB more than a 1-byte one,
/// and 1000 blame hunks charge at least their own element storage more than one hunk.
#[test]
fn heap_bytes_track_payload_not_entry_count() {
    let small = commits_heap_bytes(&commits("aaa", 1));
    let large = commits_heap_bytes(&commits("aaa", 100_000));
    assert!(large > small + 99_000, "small={small} large={large}");

    let one_hunk = blame_heap_bytes(&blame_result(1));
    let many_hunks = blame_heap_bytes(&blame_result(1000));
    let element_storage = (1000 * std::mem::size_of::<BlameHunk>()) as u64;
    assert!(
        many_hunks > one_hunk + element_storage,
        "one={one_hunk} many={many_hunks}"
    );

    // Path bytes live on the heap behind each `RelPath`, so a long-path commit must charge more
    // than the tuple array alone.
    let files: Vec<CommitFileChange> = (0..500)
        .map(|i| {
            (
                RelPath::from(format!("crates/pack/src/module_{i:04}/deeply/nested/file.rs")),
                ChangeKind::Added,
            )
        })
        .collect();
    let tuples = (500 * std::mem::size_of::<CommitFileChange>()) as u64;
    assert!(
        commit_files_heap_bytes(&files) > tuples + 500 * 40,
        "paths must be charged"
    );
}

/// Under budget nothing is evicted: the byte ceiling is an additional bound, not a tighter one,
/// so a category the entry cap allows and the budget covers keeps every entry.
#[test]
fn under_budget_keeps_every_entry() {
    let cache = GitCache::with_budget(None, 64, 1024 * 1024);
    for i in 0..8 {
        let value = commits(&format!("sha{i}"), 1024);
        category(&cache.log).admit(log_key(&format!("head{i}")), value);
    }
    let mut guard = category(&cache.log);
    assert_eq!(guard.lru.len(), 8);
    assert!(guard.charged < 1024 * 1024, "charged={}", guard.charged);
    for i in 0..8 {
        assert!(
            guard.lru.get(&log_key(&format!("head{i}"))).is_some(),
            "entry {i} survives"
        );
    }
}

/// Past the byte budget the least-recently-used entry is the one that goes, with the entry cap
/// nowhere near binding — which is the defect this bound closes.
#[test]
fn byte_budget_evicts_least_recently_used() {
    let cache = GitCache::with_budget(None, 64, 50_000);
    let (a, b, c) = (log_key("aaa"), log_key("bbb"), log_key("ccc"));
    category(&cache.log).admit(a.clone(), commits("aaa", 20_000));
    category(&cache.log).admit(b.clone(), commits("bbb", 20_000));
    // Reading `a` back makes `b` the oldest, so the next insert must take `b` and not `a`.
    assert!(category(&cache.log).lru.get(&a).is_some());
    category(&cache.log).admit(c.clone(), commits("ccc", 20_000));

    let mut guard = category(&cache.log);
    assert!(guard.charged <= 50_000, "charged={}", guard.charged);
    assert!(guard.lru.get(&a).is_some(), "the recently-read entry survives");
    assert!(guard.lru.get(&c).is_some(), "the newest entry survives");
    assert!(
        guard.lru.get(&b).is_none(),
        "the least-recently-used entry is the one evicted"
    );
}

/// An entry larger than the whole budget is admitted anyway — a blame of a generated file must
/// still be returnable — and then yields the moment anything else needs the room.
#[test]
fn oversized_entry_is_admitted_then_displaced() {
    let cache = GitCache::with_budget(None, 64, 8192);
    let huge = log_key("huge");
    category(&cache.log).admit(huge.clone(), commits("huge", 1_000_000));
    {
        let mut guard = category(&cache.log);
        assert!(
            guard.lru.get(&huge).is_some(),
            "a value over budget is still returnable"
        );
        assert_eq!(guard.lru.len(), 1);
    }
    let small = log_key("small");
    category(&cache.log).admit(small.clone(), commits("small", 16));

    let mut guard = category(&cache.log);
    assert!(guard.lru.get(&small).is_some());
    assert!(
        guard.lru.get(&huge).is_none(),
        "the oversized entry goes on the next insert"
    );
    assert!(guard.charged <= 8192, "charged={}", guard.charged);
}

/// An eviction forced by the ENTRY cap must discharge its bytes too. `LruCache::put` on a
/// capacity-bounded map drops the LRU value without returning it, so charging around that
/// instead of through it would leak the charge until the budget evicted on every insert.
#[test]
fn entry_cap_eviction_still_discharges_bytes() {
    let cache = GitCache::with_budget(None, 2, 1024 * 1024);
    for i in 0..8 {
        let value = commits(&format!("sha{i}"), 4096);
        category(&cache.log).admit(log_key(&format!("head{i}")), value);
    }
    let guard = category(&cache.log);
    assert_eq!(guard.lru.len(), 2, "the entry cap still binds");
    assert!(
        guard.charged < 3 * 4096,
        "charge must describe the two live entries only, got {}",
        guard.charged
    );
}

/// Panic while holding `mutex`, as a query that panics under a category's lock does.
fn poison<T>(mutex: &Mutex<T>) {
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = mutex.lock();
        panic!("a query panics under the category lock");
    }));
    assert!(panicked.is_err());
    assert!(mutex.is_poisoned());
}

/// The panic may have left the charge out of step with the entries, so the next caller gets the
/// category back empty and no longer poisoned.
#[test]
fn a_poisoned_category_is_emptied_for_the_next_caller() {
    let cache = GitCache::with_budget(None, 64, 1024 * 1024);
    let key = log_key("head");
    category(&cache.log).admit(key.clone(), commits("sha", 4096));
    poison(&cache.log);

    let mut guard = category(&cache.log);
    assert_eq!(guard.lru.len(), 0);
    assert_eq!(guard.charged, 0);
    assert!(guard.lru.get(&key).is_none());
    drop(guard);
    assert!(!cache.log.is_poisoned(), "the recovery clears the poison");
    category(&cache.log).admit(key.clone(), commits("sha", 4096));
    assert!(
        category(&cache.log).lru.get(&key).is_some(),
        "the category caches again"
    );
}

/// A panic under every category's lock must not fail the next call for the process's life.
#[test]
fn clear_succeeds_after_a_panic_under_every_category_lock() {
    let cache = GitCache::with_budget(None, 64, 1024 * 1024);
    poison(&cache.commit_files);
    poison(&cache.log);
    poison(&cache.blame);

    assert_eq!(cache.clear().expect("clear"), 0);
    assert!(!cache.commit_files.is_poisoned());
    assert!(!cache.log.is_poisoned());
    assert!(!cache.blame.is_poisoned());
}

/// Disk writes land in the subdirectories, so a clear must leave every one of them in place.
#[test]
fn clear_recreates_every_disk_subdirectory() {
    let dir = tempfile::tempdir().unwrap();
    let cache = GitCache::open(dir.path(), 8, true).unwrap();
    cache.clear().unwrap();
    for sub in Subdir::ALL {
        assert!(
            dir.path().join(GIT_CACHE_DIR).join(sub.name()).is_dir(),
            "{} survives a clear",
            sub.name()
        );
    }
}

/// Opening a persistent cache no longer sweeps on the caller's thread, but it still sweeps: what
/// an earlier process left over budget is evicted in the background, oldest first.
#[test]
fn open_sweeps_what_earlier_processes_left() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(GIT_CACHE_DIR);
    let blame = root.join(Subdir::Blame.name());
    fs::create_dir_all(&blame).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    for (name, mtime) in [("old.msgpack", Some(old)), ("new.msgpack", None)] {
        let path = blame.join(name);
        fs::write(&path, vec![0u8; 600]).unwrap();
        if let Some(mtime) = mtime {
            fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(mtime))
                .unwrap();
        }
    }

    let disk = open_disk(&root, 1000).unwrap();
    disk.wait_for_sweep();

    assert!(!blame.join("old.msgpack").exists(), "the oldest payload is evicted");
    assert!(blame.join("new.msgpack").exists(), "the newest stays");
    assert_eq!(disk.tracked(Subdir::Blame), 600);
    for sub in Subdir::ALL {
        assert!(root.join(sub.name()).is_dir(), "{} is created", sub.name());
    }
}

/// Every payload the cache writes is charged to its directory's running total, the count that
/// decides when the next sweep runs.
#[test]
fn a_disk_write_charges_its_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let disk = open_disk(&dir.path().join(GIT_CACHE_DIR), 0).unwrap();
    disk.wait_for_sweep();
    let cache = GitCache::with_budget(Some(Arc::clone(&disk)), 4, CATEGORY_MEM_BUDGET_BYTES);
    let key = log_key("head");

    cache.write_log_disk(&key, &commits("aaa", 512));

    let written = fs::metadata(cache.log_path(&key).unwrap()).unwrap().len();
    assert!(written > 512, "the payload carries the body, got {written}");
    assert_eq!(disk.tracked(Subdir::Log), written);
    assert_eq!(disk.tracked(Subdir::Blame), 0);
}

/// Two worktrees of one repository open the same shared directory: the commit one computes is
/// the commit the other reads, and the repository holds one `commit_files` entry for it.
#[test]
fn worktrees_share_one_commit_files_entry_per_commit() {
    let shared = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let one = GitCache::open_shared(first.path(), shared.path(), 8, true).unwrap();
    let two = GitCache::open_shared(second.path(), shared.path(), 8, true).unwrap();
    let files = vec![(RelPath::from("src/lib.rs"), crate::git::ChangeKind::Modified)];

    one.write_commit_files_disk("abc123", &files);

    assert_eq!(two.read_commit_files_disk("abc123"), Some(files));
    let entries = |root: &Path| count_files(&root.join(GIT_CACHE_DIR).join(Subdir::CommitFiles.name()));
    assert_eq!(entries(shared.path()), 1);
    assert_eq!(entries(first.path()), 0);
    assert_eq!(entries(second.path()), 0);
}

/// A cache opened without a shared directory keeps `commit_files/` beside `log/` and `blame/`.
#[test]
fn open_keeps_commit_files_in_the_workspace_directory() {
    let dir = tempfile::tempdir().unwrap();
    let cache = GitCache::open(dir.path(), 8, true).unwrap();
    let files = vec![(RelPath::from("a.rs"), crate::git::ChangeKind::Added)];

    cache.write_commit_files_disk("def456", &files);

    let own = dir.path().join(GIT_CACHE_DIR).join(Subdir::CommitFiles.name());
    assert_eq!(count_files(&own), 1);
}

/// Clearing a linked worktree's cache drops the shared `commit_files/` it reads from, and leaves
/// the directory in place for the next write.
#[test]
fn clear_empties_the_shared_commit_files_directory() {
    let shared = tempfile::tempdir().unwrap();
    let own = tempfile::tempdir().unwrap();
    let cache = GitCache::open_shared(own.path(), shared.path(), 8, true).unwrap();
    let files = vec![(RelPath::from("a.rs"), crate::git::ChangeKind::Added)];
    cache.write_commit_files_disk("def456", &files);

    let removed = cache.clear().unwrap();

    assert_eq!(removed, 1);
    let dir = shared.path().join(GIT_CACHE_DIR).join(Subdir::CommitFiles.name());
    assert!(dir.is_dir());
    assert_eq!(count_files(&dir), 0);
}

/// Two caches opened over one shared directory read each other's RAM entries, and the entries
/// are charged once.
#[test]
fn worktrees_of_one_repository_share_ram_entries() {
    let shared = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let one = GitCache::open_shared(first.path(), shared.path(), 8, true).unwrap();
    let two = GitCache::open_shared(second.path(), shared.path(), 8, true).unwrap();
    let key = log_key("head");

    category(&one.log).admit(key.clone(), commits("aaa", 4096));

    let seen = category(&two.log)
        .lru
        .get(&key)
        .cloned()
        .expect("the sibling sees the entry");
    assert_eq!(seen[0].sha, "aaa");
    let charged = category(&two.log).charged;
    assert_eq!(charged, category(&one.log).charged);
    assert_eq!(category(&one.log).lru.len(), 1);
}

/// Different repositories never see each other's entries.
#[test]
fn distinct_repositories_do_not_share_ram_entries() {
    let (shared_a, shared_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (own_a, own_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = GitCache::open_shared(own_a.path(), shared_a.path(), 8, true).unwrap();
    let b = GitCache::open_shared(own_b.path(), shared_b.path(), 8, true).unwrap();
    let key = log_key("head");

    category(&a.log).admit(key.clone(), commits("aaa", 16));

    assert!(category(&b.log).lru.get(&key).is_none());
}

/// A shared category holds the largest entry cap any opener asks for, not their sum, and the
/// byte budget is the single per-repository ceiling.
#[test]
fn shared_ram_is_widened_not_summed() {
    let shared = tempfile::tempdir().unwrap();
    let (own_a, own_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = GitCache::open_shared(own_a.path(), shared.path(), 2, true).unwrap();
    let _b = GitCache::open_shared(own_b.path(), shared.path(), 4, true).unwrap();

    for i in 0..8 {
        category(&a.log).admit(log_key(&format!("head{i}")), commits("s", 16));
    }

    let guard = category(&a.log);
    assert_eq!(guard.lru.len(), 4);
    assert_eq!(guard.budget_bytes, CATEGORY_MEM_BUDGET_BYTES);
}

/// The shared tiers die with their last cache; a later open starts empty.
#[test]
fn shared_ram_is_freed_with_its_last_cache() {
    let shared = tempfile::tempdir().unwrap();
    let own = tempfile::tempdir().unwrap();
    let key = log_key("head");
    {
        let cache = GitCache::open_shared(own.path(), shared.path(), 8, true).unwrap();
        category(&cache.log).admit(key.clone(), commits("aaa", 16));
    }
    let cache = GitCache::open_shared(own.path(), shared.path(), 8, true).unwrap();
    assert!(category(&cache.log).lru.get(&key).is_none());
}

/// A private cache (no shared directory) is unaffected by shared ones.
#[test]
fn a_private_cache_does_not_join_shared_ram() {
    let shared = tempfile::tempdir().unwrap();
    let own = tempfile::tempdir().unwrap();
    let sharing = GitCache::open_shared(own.path(), shared.path(), 8, true).unwrap();
    let private = GitCache::with_budget(None, 8, CATEGORY_MEM_BUDGET_BYTES);
    let key = log_key("head");

    category(&sharing.log).admit(key.clone(), commits("aaa", 16));

    assert!(category(&private.log).lru.get(&key).is_none());
}
