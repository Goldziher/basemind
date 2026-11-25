//! The file map of a long-lived store across incremental passes: what a pass writes and what a
//! sweep of the group decodes.

use crate::config::ConfigV1;
use crate::scanner::{EmbedMode, ScanReport, ScanSource, scan};
use crate::store::{Store, VIEW_WORKING, init_isolated_cache};

fn one_file_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let status = crate::testing::git_command()
        .args(["init", "--quiet"])
        .current_dir(dir.path())
        .status()
        .expect("run git init");
    assert!(status.success());
    std::fs::write(dir.path().join("alpha.rs"), b"pub fn alpha() -> u32 { 1 }\n").expect("write");
    dir
}

fn full_scan(root: &std::path::Path, store: &mut Store) -> ScanReport {
    scan(
        root,
        store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
        crate::scan_retrieval::Retrieval::inert(),
    )
    .expect("a full scan")
}

fn git_ok(cwd: &std::path::Path, args: &[&str]) {
    let status = crate::testing::git_command()
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=Test",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?}");
}

/// A store, fully scanned, for the repository at `root` and for each new linked worktree `linked`:
/// the views share one group index.
fn scanned_worktrees(root: &std::path::Path, linked: &[&std::path::Path]) -> Vec<Store> {
    git_ok(root, &["add", "."]);
    git_ok(root, &["commit", "-q", "-m", "init"]);
    let mut stores = Vec::new();
    for (i, path) in std::iter::once(root).chain(linked.iter().copied()).enumerate() {
        if i > 0 {
            git_ok(
                root,
                &[
                    "worktree",
                    "add",
                    "-q",
                    path.to_str().expect("utf-8"),
                    "-b",
                    &format!("wt{i}"),
                ],
            );
        }
        let mut store = Store::open(path, VIEW_WORKING).expect("open");
        full_scan(path, &mut store);
        stores.push(store);
    }
    stores
}

fn edit_and_scan(root: &std::path::Path, store: &mut Store, body: &str) -> ScanReport {
    std::fs::write(root.join("alpha.rs"), body).expect("edit");
    crate::scanner::scan_paths(
        root,
        store,
        &ConfigV1::with_defaults(),
        &[root.join("alpha.rs")],
        EmbedMode::Deferred,
        crate::scan_retrieval::Retrieval::inert(),
    )
    .expect("incremental pass")
}

/// A watcher batch that retires a content asks the group which views still hold it. The answer
/// comes from what each sibling's file map named when it was last decoded, so once the siblings are
/// known a batch decodes none of them, and a sibling that wrote since is decoded again.
#[test]
fn an_incremental_pass_decodes_no_sibling_file_map_once_the_siblings_are_known() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    let linked = tempfile::tempdir().expect("tempdir");
    let (b, c) = (linked.path().join("b"), linked.path().join("c"));
    let mut stores = scanned_worktrees(root, &[&b, &c]);
    let mut main = stores.remove(0);
    assert_eq!(
        main.sibling_view_dirs().expect("siblings").len(),
        2,
        "a group of three views"
    );

    edit_and_scan(root, &mut main, "pub fn alpha() -> u32 { 2 }\n");
    let retired = main.lookup("alpha.rs").expect("indexed").hash_hex.clone();
    assert!(
        main.index_db.as_ref().expect("index").grep_bloom_current(&retired),
        "the content about to be retired has a bloom row"
    );

    let before = crate::store::read_index_calls();
    edit_and_scan(root, &mut main, "pub fn alpha() -> u32 { 3 }\n");
    assert_eq!(
        crate::store::read_index_calls(),
        before,
        "no sibling file map was decoded"
    );
    assert!(
        !main.index_db.as_ref().expect("index").grep_bloom_current(&retired),
        "the bloom row of a content no view holds is still dropped"
    );

    let mut sibling = stores.remove(0);
    edit_and_scan(&b, &mut sibling, "pub fn alpha() -> u32 { 9 }\n");
    let before = crate::store::read_index_calls();
    edit_and_scan(root, &mut main, "pub fn alpha() -> u32 { 4 }\n");
    assert_eq!(
        crate::store::read_index_calls() - before,
        1,
        "only the sibling that wrote since is decoded again"
    );
}

/// An incremental pass writes the entries it changed, not the whole file map, and the map read back
/// holds them.
#[test]
fn an_incremental_pass_writes_only_the_changed_file_map_entries() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    for i in 0..400 {
        std::fs::write(
            root.join(format!("f{i}.rs")),
            format!("pub fn f{i}() -> u32 {{ {i} }}\n"),
        )
        .expect("write");
    }
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    let full = full_scan(root, &mut store);
    assert!(full.file_map_bytes_written > 0);

    let first = edit_and_scan(root, &mut store, "pub fn alpha() -> u32 { 2 }\n");
    let second = edit_and_scan(root, &mut store, "pub fn alpha() -> u32 { 3 }\n");

    assert!(
        first.file_map_bytes_written > 0 && second.file_map_bytes_written > 0,
        "{first:?} {second:?}"
    );
    assert!(
        second.file_map_bytes_written * 20 < full.file_map_bytes_written,
        "one edited file wrote {} bytes against {} for the whole map",
        second.file_map_bytes_written,
        full.file_map_bytes_written
    );
    let entry = store.lookup("alpha.rs").expect("indexed").clone();
    let view_dir = store.view_dir.clone();
    drop(store);
    let on_disk = crate::store::read_index(&view_dir).expect("read").expect("a file map");
    assert_eq!(on_disk.files.len(), 401);
    assert_eq!(on_disk.files.get(bstr::BStr::new("alpha.rs")), Some(&entry));
}

/// A removed file leaves the map read back, and a base replaced behind the log is read without it.
#[test]
fn a_delta_log_replays_removals_and_is_ignored_for_a_replaced_base() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    std::fs::write(root.join("beta.rs"), b"pub fn beta() -> u32 { 1 }\n").expect("write");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    full_scan(root, &mut store);
    edit_and_scan(root, &mut store, "pub fn alpha() -> u32 { 2 }\n");
    std::fs::remove_file(root.join("beta.rs")).expect("remove");
    crate::scanner::scan_paths(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        &[root.join("beta.rs")],
        EmbedMode::Deferred,
        crate::scan_retrieval::Retrieval::inert(),
    )
    .expect("removal pass");
    let view_dir = store.view_dir.clone();
    let expected = store.index.clone();
    drop(store);

    let replayed = crate::store::read_index(&view_dir).expect("read").expect("a file map");
    assert_eq!(replayed.files, expected.files);
    assert!(replayed.files.get(bstr::BStr::new("beta.rs")).is_none());

    let base = view_dir.join(crate::store::INDEX_FILE);
    let mut stale: crate::store::Index = rmp_serde::from_slice(&std::fs::read(&base).expect("read")).expect("decode");
    stale.generation += 1;
    std::fs::write(&base, rmp_serde::to_vec_named(&stale).expect("encode")).expect("replace the base");
    let read = crate::store::read_index(&view_dir).expect("read").expect("a file map");
    assert_eq!(read, stale, "a log of another base is not applied");
}

/// A frame cut short by a crash ends the replay: the map read back is the last flush that finished.
#[test]
fn a_torn_delta_frame_ends_the_replay_at_the_last_whole_flush() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    full_scan(root, &mut store);
    edit_and_scan(root, &mut store, "pub fn alpha() -> u32 { 2 }\n");
    let whole = store.lookup("alpha.rs").expect("indexed").clone();
    edit_and_scan(root, &mut store, "pub fn alpha() -> u32 { 3 }\n");
    let last = store.lookup("alpha.rs").expect("indexed").clone();
    assert_ne!(whole, last);
    let view_dir = store.view_dir.clone();
    drop(store);

    let log = view_dir.join(crate::store::FILE_MAP_LOG);
    let bytes = std::fs::read(&log).expect("the passes appended to a delta log");
    std::fs::write(&log, &bytes[..bytes.len() - 3]).expect("tear the last frame");

    let read = crate::store::read_index(&view_dir).expect("read").expect("a file map");
    assert_eq!(read.files.get(bstr::BStr::new("alpha.rs")), Some(&whole));
}

/// A log that exists but cannot be read is an error: the base alone is not the map it was flushed as.
#[test]
fn an_unreadable_delta_log_fails_the_read_instead_of_returning_the_base() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    full_scan(root, &mut store);
    edit_and_scan(root, &mut store, "pub fn alpha() -> u32 { 2 }\n");
    let view_dir = store.view_dir.clone();
    drop(store);

    let log = view_dir.join(crate::store::FILE_MAP_LOG);
    std::fs::remove_file(&log).expect("remove the log");
    std::fs::create_dir(&log).expect("a directory where the log was");

    let error = crate::store::read_index(&view_dir).expect_err("the log cannot be read");
    assert!(
        matches!(error, crate::store::StoreError::Io { ref path, .. } if *path == log),
        "{error:?}"
    );
}

/// A log replaced by one of the same length is a different stamp, so a cached read of the old one
/// is not served for it.
#[test]
fn a_view_stamp_differs_when_the_delta_log_is_replaced_by_one_of_the_same_length() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join(crate::store::FILE_MAP_LOG);
    std::fs::write(&log, b"aaaa").expect("write");
    let before = crate::store::ViewStamp::of(dir.path());
    assert_eq!(before, crate::store::ViewStamp::of(dir.path()));

    std::fs::remove_file(&log).expect("remove");
    std::fs::write(&log, b"bbbb").expect("write");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&log)
        .and_then(|file| file.set_modified(later))
        .expect("set the modification time");
    assert_ne!(before, crate::store::ViewStamp::of(dir.path()));
}

/// Replace the base of `view_dir` the way a release that does not know the delta log writes it:
/// the base read alone, changed by `edit`, written to a temporary file and renamed over the base.
fn rewrite_base_as_a_release_without_the_log(
    view_dir: &std::path::Path,
    edit: impl FnOnce(&mut crate::store::Index),
) -> crate::store::Index {
    let base = view_dir.join(crate::store::INDEX_FILE);
    let mut index: crate::store::Index =
        rmp_serde::from_slice(&std::fs::read(&base).expect("read the base")).expect("the base decodes alone");
    edit(&mut index);
    let tmp = view_dir.join(format!("{}.tmp", crate::store::INDEX_FILE));
    std::fs::write(&tmp, rmp_serde::to_vec_named(&index).expect("encode")).expect("write the new base");
    std::fs::rename(&tmp, &base).expect("rename it over the base");
    index
}

/// The delta log is no layout of its own: a map this binary writes, log and all, has the schema and
/// blob layout an earlier release of the same minor reads, so that release is not refused, and the
/// base decodes alone as a whole map.
#[test]
fn a_file_map_with_a_delta_log_reads_as_the_layout_of_an_earlier_release_of_the_minor() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    full_scan(root, &mut store);
    edit_and_scan(root, &mut store, "pub fn alpha() -> u32 { 2 }\n");
    let view_dir = store.view_dir.clone();
    drop(store);
    assert!(
        view_dir.join(crate::store::FILE_MAP_LOG).is_file(),
        "the incremental pass appended to a delta log"
    );

    let base: crate::store::Index =
        rmp_serde::from_slice(&std::fs::read(view_dir.join(crate::store::INDEX_FILE)).expect("read the base"))
            .expect("the base decodes alone");

    assert_eq!(
        (base.schema_ver, base.blob_layout),
        (crate::extract::SCHEMA_VER, 1),
        "the layout of the namespaced blob store, which every 0.34 release reads"
    );
    assert!(!crate::store::written_by_newer(&view_dir));
}

/// A release that does not know the delta log reads the base alone and writes it again. Its write
/// leaves the log stamped for a base that is gone: this binary reads exactly the base that release
/// wrote, never the log's entries over it, and its next store writes a base and log of its own.
#[test]
fn a_base_rewritten_by_a_release_without_the_log_is_read_without_the_stale_log() {
    init_isolated_cache();
    let dir = one_file_repo();
    let root = dir.path();
    std::fs::write(root.join("beta.rs"), b"pub fn beta() -> u32 { 1 }\n").expect("write");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    full_scan(root, &mut store);
    edit_and_scan(root, &mut store, "pub fn alpha() -> u32 { 2 }\n");
    let logged = store.lookup("alpha.rs").expect("indexed").clone();
    let view_dir = store.view_dir.clone();
    drop(store);
    let base_alone: crate::store::Index =
        rmp_serde::from_slice(&std::fs::read(view_dir.join(crate::store::INDEX_FILE)).expect("read the base"))
            .expect("decode");
    assert_ne!(
        base_alone.files.get(bstr::BStr::new("alpha.rs")),
        Some(&logged),
        "the edit is in the log, not in the base"
    );

    std::fs::remove_file(root.join("beta.rs")).expect("remove");
    let rewritten = rewrite_base_as_a_release_without_the_log(&view_dir, |index| {
        index.files.remove(bstr::BStr::new("beta.rs"));
    });
    assert!(
        view_dir.join(crate::store::FILE_MAP_LOG).is_file(),
        "the release without the log left it in place"
    );

    let read = crate::store::read_index(&view_dir).expect("read").expect("a file map");
    assert_eq!(read, rewritten, "the stale log is not applied over the rewritten base");

    let mut reopened = loop {
        match Store::open(root, VIEW_WORKING) {
            Err(error) if error.is_lock_contention() => std::thread::sleep(std::time::Duration::from_millis(25)),
            opened => break opened.expect("the writer open keeps the map"),
        }
    };
    assert_eq!(reopened.index.files, rewritten.files);
    edit_and_scan(root, &mut reopened, "pub fn alpha() -> u32 { 3 }\n");
    edit_and_scan(root, &mut reopened, "pub fn alpha() -> u32 { 4 }\n");
    let expected = reopened.index.clone();
    drop(reopened);

    let after = crate::store::read_index(&view_dir).expect("read").expect("a file map");
    assert_eq!(
        after.files, expected.files,
        "the next store's own base and log read back whole"
    );
    assert!(after.files.get(bstr::BStr::new("beta.rs")).is_none());
}
