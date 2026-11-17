//! The group index (ADR-0028): every worktree of a repository shares one index database, keyed by
//! view, and a view's failures stay its own.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use basemind::config::ConfigV1;
use basemind::index::view::IndexView;
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::store::{INDEX_FILE, Store, VIEW_WORKING, VIEWS_DIR, workspace_cache_dir};

fn layout() {
    basemind::store::init_isolated_cache();
}

fn git(args: &[&str], cwd: &Path) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository with `shared.rs` in both worktrees and `only_b.rs` in the linked one only.
fn group() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir(&main).expect("main");
    git(&["init", "-q", "-b", "main"], &main);
    git(&["config", "user.email", "t@example.com"], &main);
    git(&["config", "user.name", "Test"], &main);
    git(&["config", "core.autocrlf", "false"], &main);
    fs::write(main.join("shared.rs"), "pub fn group_shared_fn() { group_callee(); }\n").expect("write");
    git(&["add", "."], &main);
    git(&["commit", "-qm", "init"], &main);
    let linked = tmp.path().join("linked");
    git(
        &["worktree", "add", "-q", linked.to_str().expect("utf-8"), "-b", "linked"],
        &main,
    );
    fs::write(linked.join("only_b.rs"), "pub fn only_in_b() { b_callee(); }\n").expect("write");
    (tmp, main, linked)
}

fn scan_working(root: &Path) -> Store {
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    let mut config = ConfigV1::with_defaults();
    config.documents.enabled = false;
    scan(root, &mut store, &config, ScanSource::WorkingTree, EmbedMode::Inline).expect("scan");
    store
}

fn defines(store: &Store, name: &str) -> bool {
    !store
        .index_view()
        .expect("a writer store holds its index")
        .symbols_by_name_lookup(name, 10)
        .is_empty()
}

fn hash_of(store: &Store, rel: &str) -> String {
    store.lookup(rel).expect("indexed").hash_hex.clone()
}

#[test]
fn worktrees_of_a_group_share_one_database_and_see_only_their_own_files() {
    layout();
    let (_tmp, main, linked) = group();
    let a = scan_working(&main);
    let b = scan_working(&linked);

    assert!(defines(&a, "group_shared_fn") && defines(&b, "group_shared_fn"));
    assert!(defines(&b, "only_in_b"));
    assert!(!defines(&a, "only_in_b"), "a view never reads another view's rows");
    for root in [&main, &linked] {
        assert!(
            !workspace_cache_dir(root)
                .join(VIEWS_DIR)
                .join(VIEW_WORKING)
                .join("index.fjall")
                .exists(),
            "no view keeps a database of its own"
        );
    }
    let group_dir = basemind::store_layout::group_index_dir(&main);
    assert_eq!(group_dir, basemind::store_layout::group_index_dir(&linked));
    assert!(group_dir.join("index.fjall").is_dir(), "one database for the group");
}

/// The bloom row of a content only the linked worktree holds survives a full scan of the main one,
/// whose own file map does not name it (stage 2: the sweep consults every view of the group).
#[test]
fn a_full_scan_of_one_worktree_keeps_the_bloom_rows_another_worktree_needs() {
    layout();
    let (_tmp, main, linked) = group();
    let b = scan_working(&linked);
    let only_b = hash_of(&b, "only_b.rs");
    drop(b);

    let a = scan_working(&main);
    let view = a.index_view().expect("index");
    assert!(
        view.grep_bloom_row(&only_b).is_some(),
        "the main worktree's sweep must not drop a row the linked worktree holds"
    );
}

/// A view whose file map claims a generation its rows never reached is rebuilt alone: the other
/// view of the group keeps its rows and its file map.
#[test]
fn a_view_behind_its_file_map_is_rebuilt_without_touching_its_sibling() {
    layout();
    let (_tmp, main, linked) = group();
    drop(scan_working(&main));
    drop(scan_working(&linked));

    let map_path = workspace_cache_dir(&main)
        .join(VIEWS_DIR)
        .join(VIEW_WORKING)
        .join(INDEX_FILE);
    let mut map: basemind::store::Index = rmp_serde::from_slice(&fs::read(&map_path).expect("read")).expect("decode");
    map.generation += 5;
    fs::write(&map_path, rmp_serde::to_vec_named(&map).expect("encode")).expect("write");

    let a = Store::open(&main, VIEW_WORKING).expect("reopen");
    assert!(a.rescan_owed(), "the lost tail is detected for this view");
    assert!(a.index.files.is_empty(), "its file map is emptied for the rebuild");
    drop(a);
    let b = Store::open(&linked, VIEW_WORKING).expect("reopen sibling");
    assert!(!b.rescan_owed(), "the sibling is untouched");
    assert!(defines(&b, "only_in_b"));
}

/// A file map written against another incarnation of the group database names rows that are gone.
#[test]
fn a_file_map_of_another_incarnation_is_emptied() {
    layout();
    let (_tmp, main, _linked) = group();
    drop(scan_working(&main));
    let map_path = workspace_cache_dir(&main)
        .join(VIEWS_DIR)
        .join(VIEW_WORKING)
        .join(INDEX_FILE);
    let mut map: basemind::store::Index = rmp_serde::from_slice(&fs::read(&map_path).expect("read")).expect("decode");
    assert_ne!(map.group_incarnation, 0, "a group view records the incarnation");
    map.group_incarnation ^= 1;
    fs::write(&map_path, rmp_serde::to_vec_named(&map).expect("encode")).expect("write");

    let reopened = Store::open(&main, VIEW_WORKING).expect("reopen");
    assert!(reopened.rescan_owed());
    assert!(reopened.index.files.is_empty());
}
