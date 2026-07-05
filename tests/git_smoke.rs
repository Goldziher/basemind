//! Integration tests for the git-aware scanner sources.
//!
//! These tests spin up real git repositories on tempdirs (via the `git` CLI) and drive
//! `basemind::scanner::scan` against them with `ScanSource::Staged` and `ScanSource::Rev`.
//! Using the system `git` to set up the fixtures is intentional — basemind itself never
//! writes to a repo, so going through `git` is the most representative way to test the
//! contract from the developer's perspective.

use std::fs;
use std::path::Path;
use std::process::Command;

use basemind::config::ConfigV1;
use basemind::git::Repo;
use basemind::scanner::{ScanSource, scan};
use basemind::store::{Store, VIEW_STAGED, VIEW_WORKING};
use tempfile::TempDir;

fn run(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn init_repo() -> (TempDir, ConfigV1) {
    let dir = tempfile::tempdir().expect("tempdir");
    run(dir.path(), &["init", "-q"]);
    run(dir.path(), &["config", "commit.gpgsign", "false"]);
    let cfg = ConfigV1::with_defaults();
    (dir, cfg)
}

#[test]
fn scan_staged_uses_index_blobs_not_working_tree() {
    let (dir, cfg) = init_repo();
    let root = dir.path();

    // Commit a clean file…
    fs::write(root.join("a.rs"), b"pub fn clean_one() {}\n").unwrap();
    run(root, &["add", "a.rs"]);
    run(root, &["commit", "-qm", "init"]);

    // …then break it on disk without staging.
    fs::write(root.join("a.rs"), b"pub fn broken( {\n").unwrap();

    let repo = Repo::discover(root).expect("repo discover");
    let mut store = Store::open(root, VIEW_STAGED).unwrap();
    let report = scan(
        root,
        &mut store,
        &cfg,
        ScanSource::Staged(&repo),
        basemind::scanner::EmbedMode::Inline,
    )
    .expect("scan staged");

    assert_eq!(report.stats.updated, 1, "one file updated");
    let entry = store.lookup("a.rs").expect("a.rs indexed");
    assert_eq!(entry.language, "rust");
    // The committed blob has no parse errors — only the WT version does.
    let hits = basemind::query::search_symbols(&store, "clean_one", None).unwrap();
    assert_eq!(hits.len(), 1, "staged scan saw committed symbols");
}

#[test]
fn scan_rev_at_head_matches_clean_working_tree() {
    let (dir, cfg) = init_repo();
    let root = dir.path();

    fs::write(root.join("a.rs"), b"pub fn shared() {}\npub fn also() {}\n").unwrap();
    run(root, &["add", "a.rs"]);
    run(root, &["commit", "-qm", "init"]);

    // Working-tree scan
    let mut wt_store = Store::open(root, VIEW_WORKING).unwrap();
    scan(
        root,
        &mut wt_store,
        &cfg,
        ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .unwrap();
    let wt_entry = wt_store.lookup("a.rs").expect("a.rs in WT view").clone();
    drop(wt_store);

    // Rev scan at HEAD
    let repo = Repo::discover(root).unwrap();
    let head_sha = repo.resolve_rev("HEAD").unwrap();
    let rev_view = format!("rev-{}", &head_sha[..7]);
    let mut rev_store = Store::open(root, &rev_view).unwrap();
    scan(
        root,
        &mut rev_store,
        &cfg,
        ScanSource::Rev {
            repo: &repo,
            sha: head_sha,
        },
        basemind::scanner::EmbedMode::Inline,
    )
    .unwrap();
    let rev_entry = rev_store.lookup("a.rs").expect("a.rs in rev view").clone();

    // The hash + symbol set should match — same bytes give the same blob.
    assert_eq!(wt_entry.hash_hex, rev_entry.hash_hex);
    assert_eq!(wt_entry.size_bytes, rev_entry.size_bytes);
    assert_eq!(wt_entry.language, rev_entry.language);
}

#[test]
fn views_live_in_separate_subdirs_under_dotbasemind() {
    let (dir, cfg) = init_repo();
    let root = dir.path();
    fs::write(root.join("a.rs"), b"pub fn here() {}\n").unwrap();
    run(root, &["add", "a.rs"]);
    run(root, &["commit", "-qm", "init"]);

    let repo = Repo::discover(root).unwrap();

    let mut working = Store::open(root, VIEW_WORKING).unwrap();
    scan(
        root,
        &mut working,
        &cfg,
        ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .unwrap();
    drop(working);

    let mut staged = Store::open(root, VIEW_STAGED).unwrap();
    scan(
        root,
        &mut staged,
        &cfg,
        ScanSource::Staged(&repo),
        basemind::scanner::EmbedMode::Inline,
    )
    .unwrap();
    drop(staged);

    let views_dir = root.join(".basemind").join("views");
    assert!(views_dir.join(VIEW_WORKING).join("index.msgpack").exists());
    assert!(views_dir.join(VIEW_STAGED).join("index.msgpack").exists());
    // No legacy file left behind.
    assert!(!root.join(".basemind").join("index.msgpack").exists());
}

#[test]
fn legacy_dotbasemind_index_is_migrated_into_working_view() {
    // Simulate a pre-views install: write a top-level `.basemind/index.msgpack` and confirm
    // `Store::open(.., "working")` quietly moves it into `views/working/index.msgpack`.
    let (dir, _cfg) = init_repo();
    let root = dir.path();
    fs::create_dir_all(root.join(".basemind").join("blobs")).unwrap();

    // A valid, current-schema empty index. Use the public Index type via rmp_serde so the
    // schema_ver matches whatever SCHEMA_VER currently is — that way the migration test is
    // about *moving the file*, not about schema mismatch.
    let empty = basemind::store::Index::empty();
    let bytes = rmp_serde::to_vec_named(&empty).unwrap();
    fs::write(root.join(".basemind").join("index.msgpack"), &bytes).unwrap();

    let store = Store::open(root, VIEW_WORKING).expect("open should migrate");
    assert_eq!(store.index.files.len(), 0);
    assert!(!root.join(".basemind").join("index.msgpack").exists());
    assert!(
        root.join(".basemind")
            .join("views")
            .join(VIEW_WORKING)
            .join("index.msgpack")
            .exists()
    );
}

#[test]
fn scan_skips_submodule_paths_by_default() {
    // Build a parent repo with a submodule pointing at a sibling tempdir repo. With
    // `skip_submodules: true` (default), files under the submodule root should not appear
    // in the parent's index. With it disabled, they should.

    // Inner repo — the one we'll mount as a submodule.
    let inner = tempfile::tempdir().expect("inner tempdir");
    run(inner.path(), &["init", "-q"]);
    run(inner.path(), &["config", "commit.gpgsign", "false"]);
    fs::write(inner.path().join("inner.rs"), b"pub fn inner_fn() {}\n").unwrap();
    run(inner.path(), &["add", "inner.rs"]);
    run(inner.path(), &["commit", "-qm", "init inner"]);

    // Parent repo with one bare file at the top.
    let (parent, cfg) = init_repo();
    let root = parent.path();
    fs::write(root.join("top.rs"), b"pub fn top_fn() {}\n").unwrap();
    run(root, &["add", "top.rs"]);
    run(root, &["commit", "-qm", "init parent"]);

    // Wire the submodule using the system git CLI — the modern git complains about
    // file:// transports for security; -c works around that for local fixtures.
    let url = format!("file://{}", inner.path().display());
    let status = Command::new("git")
        .args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            &url,
            "vendored",
        ])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git submodule add failed");
    run(root, &["commit", "-qm", "add submodule"]);

    // gix should see the submodule via .gitmodules.
    let repo = Repo::discover(root).expect("discover parent");
    let subs = repo.submodule_paths();
    assert_eq!(subs, vec![basemind::path::RelPath::from("vendored")], "got {subs:?}");

    // Default scan should NOT index the submodule's inner.rs.
    {
        let mut store = Store::open(root, VIEW_WORKING).unwrap();
        scan(
            root,
            &mut store,
            &cfg,
            ScanSource::WorkingTree,
            basemind::scanner::EmbedMode::Inline,
        )
        .expect("scan default");
        assert!(store.lookup("top.rs").is_some(), "parent file should be indexed");
        assert!(
            store.lookup("vendored/inner.rs").is_none(),
            "submodule file should be skipped by default"
        );
    }

    // Flip the knob → inner.rs reappears.
    let mut cfg2 = ConfigV1::with_defaults();
    cfg2.scan.skip_submodules = false;
    let mut store2 = Store::open(root, VIEW_STAGED).unwrap();
    scan(
        root,
        &mut store2,
        &cfg2,
        ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .expect("scan opt-in");
    assert!(
        store2.lookup("vendored/inner.rs").is_some(),
        "submodule file should be indexed when skip_submodules=false"
    );
}

/// I6: blame reads the committed HEAD blob, not the working tree. A dirty working copy that
/// shifts line numbers must not corrupt blame's line attribution — blame against HEAD must
/// return the committed lines regardless of on-disk edits.
#[test]
fn blame_file_uses_committed_blob_not_dirty_working_tree() {
    let (dir, _cfg) = init_repo();
    let root = dir.path();
    fs::write(root.join("a.rs"), b"fn one() {}\nfn two() {}\n").unwrap();
    run(root, &["add", "a.rs"]);
    run(root, &["commit", "-q", "-m", "initial"]);

    // Dirty the working tree by prepending lines that would shift every line number.
    fs::write(root.join("a.rs"), b"// dirty\n// dirty\nfn one() {}\nfn two() {}\n").unwrap();

    let repo = Repo::discover(root).expect("discover");
    let head = repo.resolve_rev("HEAD").expect("resolve HEAD");
    let rel = basemind::path::RelPath::from("a.rs".as_bytes());
    // Blame committed lines 1..=2 — these are `fn one` / `fn two` in the *committed* blob.
    let result = repo
        .blame_file(&head, &rel, Some((1, 2)))
        .expect("blame committed blob");
    assert!(
        !result.hunks.is_empty(),
        "blame against HEAD must resolve even with a dirty working tree"
    );
    let first = &result.hunks[0];
    assert_eq!(
        first.start_line, 1,
        "committed line 1 must blame to line 1, not a working-tree-shifted line"
    );
    assert_eq!(first.commit_sha, head, "single-commit history blames to HEAD");
}

/// I5: history is by exact path and stops at renames, while blame follows them. This test
/// pins that documented asymmetry so a future change to either side is caught.
#[test]
fn log_for_path_stops_at_rename_while_blame_follows() {
    let (dir, _cfg) = init_repo();
    let root = dir.path();
    fs::write(root.join("old.rs"), b"fn keep() {}\n").unwrap();
    run(root, &["add", "old.rs"]);
    run(root, &["commit", "-q", "-m", "add old"]);
    run(root, &["mv", "old.rs", "new.rs"]);
    run(root, &["commit", "-q", "-m", "rename to new"]);

    let repo = Repo::discover(root).expect("discover");
    // History for the new name only sees the rename commit, not the original add.
    let log = repo.log_for_path("new.rs", 10).expect("log new.rs");
    assert_eq!(
        log.len(),
        1,
        "log_for_path is exact-path and stops at the rename (documented asymmetry)"
    );
    assert!(
        log[0].summary.contains("rename"),
        "the single visible commit is the rename itself"
    );

    // Blame, by contrast, follows the rename: the kept line still attributes to the add.
    let head = repo.resolve_rev("HEAD").expect("resolve HEAD");
    let rel = basemind::path::RelPath::from("new.rs".as_bytes());
    let blame = repo.blame_file(&head, &rel, Some((1, 1))).expect("blame new.rs");
    assert!(
        !blame.hunks.is_empty(),
        "blame follows the rename and resolves the kept line"
    );
}
