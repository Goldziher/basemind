//! The worktree lifecycle of a repository group (ADR-0028) driven through the real binary: three
//! worktrees scanned by `basemind scan` share one group database and keep per-view file maps, each
//! extra worktree adds a bounded number of bytes to it, and once a worktree is removed and
//! `basemind cache gc` has run, the next full pass retires its view and drops the rows of the
//! contents only it held, while the other worktrees still answer `code symbols`. While the removed
//! worktree's view directory is still on disk (its absence is the sweep's only signal that the view
//! is gone), the same pass retires nothing and the retirement check reads red.
//!
//! The single test owns the isolated data home of this binary, so the group count it asserts is
//! exact.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use basemind::data_root::DataRoot;
use basemind::index::GroupRows;
use basemind::index::keys::{CONTENT_KEY_LEN, ContentKey};
use basemind::store::{INDEX_FILE, Index, Store, VIEW_WORKING, VIEWS_DIR, workspace_cache_dir};
use basemind::store_layout::{GROUPS_DIR, group_index_dir};
use serde_json::Value;
use worktree_growth::exceeds_growth_bound;

#[path = "support/worktree_growth.rs"]
mod worktree_growth;

/// Upper bound, in percent of the group database after the first worktree's scan, on the bytes each
/// extra worktree of this fixture adds to it. An extra worktree that shared nothing would write every
/// row of the first scan again, about 100%; one that shares writes the rows of its one file of its
/// own and its view-scoped rows. The sizes are logical lengths, so journal preallocation does not
/// step them. Half the first scan sits between the two with room on either side.
const EXTRA_WORKTREE_GROWTH_PERCENT: u64 = 50;

/// Keyspaces whose rows the sweep drops with the last content that names them.
const SWEPT_KEYSPACES: [&str; 4] = ["symbols_by_path", "symbols_by_name", "calls_by_path", "calls_by_callee"];

/// Rust files every worktree holds byte for byte.
const SHARED_FILES: usize = 24;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
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

/// Run `basemind --root <root> --json <args>` and parse its stdout.
fn basemind_json(root: &Path, args: &[&str]) -> Value {
    let out = Command::new(bin())
        .args(["--root", root.to_str().expect("utf-8"), "--json"])
        .args(args)
        .output()
        .expect("run basemind");
    assert!(
        out.status.success(),
        "basemind {args:?} in {}: {}",
        root.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "basemind {args:?} printed no JSON ({error}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn scan(root: &Path) {
    let out = Command::new(bin())
        .args(["--root", root.to_str().expect("utf-8"), "scan", "--quiet"])
        .output()
        .expect("run basemind scan");
    assert!(
        out.status.success(),
        "basemind scan in {}: {}",
        root.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn shared_source(i: usize) -> String {
    format!(
        "pub fn shared_widget_{i}(queue: u32) -> u32 {{ shared_helper_{i}(queue) + {i} }}\n\n\
         fn shared_helper_{i}(queue: u32) -> u32 {{ queue * 2 }}\n\n\
         pub struct SharedFrame{i} {{ pub width: u32, pub height: u32 }}\n"
    )
}

/// A repository with the main worktree and two linked ones, `b` and `c`. Each holds the same shared
/// files plus one file of its own, `only_<name>.rs`, defining `only_in_<name>`.
fn three_worktrees(tmp: &Path) -> [PathBuf; 3] {
    let main = tmp.join("main");
    fs::create_dir(&main).expect("main");
    git(&["init", "-q", "-b", "main"], &main);
    git(&["config", "user.email", "t@example.com"], &main);
    git(&["config", "user.name", "Test"], &main);
    // Windows runners default to CRLF checkouts, which would give each worktree its own content hashes. ~keep
    git(&["config", "core.autocrlf", "false"], &main);
    // The fixture must not depend on the user's signing setup. ~keep
    git(&["config", "commit.gpgsign", "false"], &main);
    for i in 0..SHARED_FILES {
        fs::write(main.join(format!("shared_{i}.rs")), shared_source(i)).expect("write");
    }
    git(&["add", "."], &main);
    git(&["commit", "-qm", "init"], &main);
    let b = tmp.join("b");
    let c = tmp.join("c");
    for (dir, branch) in [(&b, "b"), (&c, "c")] {
        git(
            &["worktree", "add", "-q", dir.to_str().expect("utf-8"), "-b", branch],
            &main,
        );
    }
    for (dir, name) in [(&main, "main"), (&b, "b"), (&c, "c")] {
        fs::write(
            dir.join(format!("only_{name}.rs")),
            format!("pub fn only_in_{name}() -> u32 {{ 7 }}\n"),
        )
        .expect("write");
    }
    [main, b, c]
}

fn file_map_path(root: &Path) -> PathBuf {
    workspace_cache_dir(root)
        .join(VIEWS_DIR)
        .join(VIEW_WORKING)
        .join(INDEX_FILE)
}

fn file_map(root: &Path) -> Index {
    rmp_serde::from_slice(&fs::read(file_map_path(root)).expect("read the file map")).expect("decode the file map")
}

fn content_key_of(map: &Index, rel: &str) -> ContentKey {
    map.files
        .iter()
        .find(|(path, _)| path.as_str() == Some(rel))
        .and_then(|(_, entry)| entry.content_key)
        .unwrap_or_else(|| panic!("{rel} is indexed with a content key"))
}

fn dir_size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| {
            let entry = entry.expect("dir entry");
            let kind = entry.file_type().expect("file type");
            if kind.is_dir() {
                dir_size(&entry.path())
            } else {
                entry.metadata().expect("metadata").len()
            }
        })
        .sum()
}

fn group_rows(root: &Path) -> GroupRows {
    Store::open(root, VIEW_WORKING)
        .expect("open the writer")
        .index_db
        .as_ref()
        .expect("a writer holds its index")
        .group_rows()
        .expect("read the group's rows")
}

/// Shared rows of `keyspace` whose key names `content`.
fn rows_naming(rows: &GroupRows, keyspace: &str, content: &ContentKey) -> usize {
    rows.shared[keyspace]
        .iter()
        .filter(|(key, _)| key.windows(CONTENT_KEY_LEN).any(|window| window == content))
        .count()
}

fn symbols(root: &Path, name: &str) -> Value {
    basemind_json(root, &["code", "symbols", name])["results"].clone()
}

/// What the retirement of a view changes in its group: the views with rows of their own, the rows
/// of each [`SWEPT_KEYSPACES`] keyspace that name a content only the retired view held, and the
/// shared `symbols_by_path` rows.
#[derive(Debug, PartialEq, Eq)]
struct Retirement {
    views_with_rows: BTreeSet<u32>,
    rows_of_its_content: Vec<(&'static str, usize)>,
    shared_symbols: usize,
}

fn retirement(rows: &GroupRows, content: &ContentKey) -> Retirement {
    Retirement {
        views_with_rows: rows.views_with_rows.clone(),
        rows_of_its_content: SWEPT_KEYSPACES
            .iter()
            .map(|keyspace| (*keyspace, rows_naming(rows, keyspace, content)))
            .collect(),
        shared_symbols: rows.shared["symbols_by_path"].len(),
    }
}

#[test]
fn a_removed_worktree_is_retired_from_the_shared_group_and_its_siblings_keep_answering() {
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let [main, b, c] = three_worktrees(tmp.path());
    let group_dir = group_index_dir(&main);

    let mut sizes = Vec::new();
    for root in [&main, &b, &c] {
        scan(root);
        sizes.push(dir_size(&group_dir));
    }

    for root in [&b, &c] {
        assert_eq!(group_index_dir(root), group_dir, "every worktree maps to one group");
    }
    let groups: Vec<_> = fs::read_dir(DataRoot::current().cache_dir().join(GROUPS_DIR))
        .expect("read the groups dir")
        .map(|entry| entry.expect("group entry").path())
        .collect();
    assert_eq!(
        groups,
        vec![group_dir.clone()],
        "one group database for the three worktrees"
    );
    assert!(group_dir.join("index.fjall").is_dir());
    for root in [&main, &b, &c] {
        let view = workspace_cache_dir(root).join(VIEWS_DIR).join(VIEW_WORKING);
        assert!(
            view.join(INDEX_FILE).is_file(),
            "{} keeps its own file map",
            root.display()
        );
        assert!(
            !view.join("index.fjall").exists(),
            "no view keeps a database of its own"
        );
        assert_eq!(file_map(root).files.len(), SHARED_FILES + 1);
    }

    let first = sizes[0];
    for (extra, pair) in sizes.windows(2).enumerate() {
        let added = pair[1].saturating_sub(pair[0]);
        assert!(
            !exceeds_growth_bound(added, first, EXTRA_WORKTREE_GROWTH_PERCENT),
            "extra worktree {} added {added} bytes to a {first}-byte group (sizes {sizes:?}); the bound is \
             {EXTRA_WORKTREE_GROWTH_PERCENT}% of the first",
            extra + 1
        );
    }

    let views: [u32; 3] = [&main, &b, &c].map(|root| file_map(root).view_id);
    assert_eq!(
        views.iter().copied().collect::<BTreeSet<_>>().len(),
        3,
        "each worktree has a view id of its own: {views:?}"
    );
    let only_c = content_key_of(&file_map(&c), "only_c.rs");
    let before = group_rows(&main);
    assert_eq!(before.views_with_rows, views.into_iter().collect::<BTreeSet<_>>());
    assert_eq!(
        rows_naming(&before, "symbols_by_path", &only_c),
        1,
        "only_in_c is one symbol"
    );
    assert_eq!(rows_naming(&before, "symbols_by_name", &only_c), 1);

    // Keyed by the canonical root, which cannot be resolved once the worktree is gone. ~keep
    let c_cache = workspace_cache_dir(&c);
    let c_map = fs::read(file_map_path(&c)).expect("read the file map");
    git(&["worktree", "remove", "--force", c.to_str().expect("utf-8")], &main);
    let gc = basemind_json(&main, &["cache", "gc"]);
    assert_eq!(
        gc["workspaces_reaped"], 1,
        "cache gc reaps the removed worktree's workspace: {gc}"
    );
    assert!(!c_cache.exists(), "the removed worktree's workspace is gone");

    let retired = Retirement {
        views_with_rows: [views[0], views[1]].into_iter().collect(),
        rows_of_its_content: SWEPT_KEYSPACES.iter().map(|keyspace| (*keyspace, 0)).collect(),
        shared_symbols: before.shared["symbols_by_path"].len() - 1,
    };
    let c_view = c_cache.join(VIEWS_DIR).join(VIEW_WORKING);
    fs::create_dir_all(&c_view).expect("put the removed worktree's view directory back");
    fs::write(c_view.join(INDEX_FILE), &c_map).expect("put its file map back");
    scan(&main);
    let kept = retirement(&group_rows(&main), &only_c);
    assert_eq!(
        kept,
        retirement(&before, &only_c),
        "a view whose directory is on disk is not retired, and the contents its file map names stay"
    );
    assert_ne!(kept, retired, "the retirement check reads red when no view is retired");

    fs::remove_dir_all(&c_cache).expect("remove the removed worktree's workspace again");
    scan(&main);
    assert_eq!(
        retirement(&group_rows(&main), &only_c),
        retired,
        "the removed worktree's view is retired with its rows, and only its own content is dropped"
    );

    assert_eq!(
        symbols(&main, "only_in_main"),
        serde_json::json!(["only_main.rs:1: pub fn only_in_main() -> u32"])
    );
    assert_eq!(
        symbols(&b, "only_in_b"),
        serde_json::json!(["only_b.rs:1: pub fn only_in_b() -> u32"])
    );
    for root in [&main, &b] {
        assert_eq!(
            symbols(root, "shared_widget_3"),
            serde_json::json!(["shared_3.rs:1: pub fn shared_widget_3(queue: u32) -> u32"]),
            "{} still answers from the shared rows",
            root.display()
        );
    }
}
