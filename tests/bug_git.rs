//! Regression tests for two git bugs:
//!
//! * #24 (blob-filter half): `list_paths_rev` must skip non-blob tree entries
//!   (submodule gitlinks, mode 160000, and sub-trees) instead of returning them,
//!   so blob-reading code is never handed a gitlink/commit/tree.
//! * #15: `status_porcelain` must report tracked-file modifications and untracked
//!   files (the working tree is not clean after editing a committed file).
//!
//! Fixtures are built with the system `git` CLI — basemind never writes to a repo,
//! so going through `git` is the most representative way to exercise the contract.

use std::fs;
use std::path::Path;
use std::process::Command;

use basemind::git::Repo;
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

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git in PATH");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).expect("utf8 git output")
}

fn init_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    run(dir.path(), &["init", "-q"]);
    run(dir.path(), &["config", "commit.gpgsign", "false"]);
    run(dir.path(), &["config", "protocol.file.allow", "always"]);
    dir
}

/// #24: a tree containing a submodule gitlink (mode 160000) must yield only the
/// real file blobs from `list_paths_rev`, and must not error.
///
/// The gitlink is planted directly via `git update-index --cacheinfo` so the test
/// does not depend on `protocol.file.allow` (which blocks file:// submodule clones
/// on modern git) — only the tree entry mode matters for the contract under test.
#[test]
fn list_paths_rev_skips_submodule_gitlinks() {
    let sup = init_repo();
    fs::write(sup.path().join("real.txt"), b"real file\n").unwrap();
    run(sup.path(), &["add", "real.txt"]);
    run(sup.path(), &["commit", "-q", "-m", "base"]);

    // The gitlink stores a commit oid; it does not need to be reachable in this repo.
    // Use this repo's own HEAD commit oid as a valid 40-hex target.
    let gitlink_oid = git_out(sup.path(), &["rev-parse", "HEAD"]);
    let gitlink_oid = gitlink_oid.trim();
    run(
        sup.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{gitlink_oid},sub"),
        ],
    );
    run(sup.path(), &["commit", "-q", "-m", "add gitlink"]);

    let repo = Repo::discover(sup.path()).expect("discover superproject");
    let paths = repo
        .list_paths_rev("HEAD")
        .expect("list_paths_rev must not error on a gitlink tree");

    // The gitlink path "sub" (mode 160000) must NOT appear — it is not a blob.
    assert!(
        !paths.iter().any(|p| p == "sub"),
        "gitlink 'sub' leaked into blob list: {paths:?}"
    );
    // The real blob must still be present.
    assert!(paths.iter().any(|p| p == "real.txt"), "real.txt missing: {paths:?}");
}

/// #24: nested sub-trees enumerate only their leaf blobs, never the tree entries.
#[test]
fn list_paths_rev_returns_only_blobs_for_nested_trees() {
    let dir = init_repo();
    fs::create_dir_all(dir.path().join("a/b")).unwrap();
    fs::write(dir.path().join("a/b/deep.txt"), b"deep\n").unwrap();
    fs::write(dir.path().join("top.txt"), b"top\n").unwrap();
    run(dir.path(), &["add", "."]);
    run(dir.path(), &["commit", "-q", "-m", "nested"]);

    let repo = Repo::discover(dir.path()).expect("discover");
    let paths = repo.list_paths_rev("HEAD").expect("list_paths_rev");

    assert!(paths.iter().any(|p| p == "a/b/deep.txt"), "{paths:?}");
    assert!(paths.iter().any(|p| p == "top.txt"), "{paths:?}");
    // Tree entries "a" and "a/b" must not appear as paths.
    assert!(!paths.iter().any(|p| p == "a" || p == "a/b"), "{paths:?}");
}

/// #15: editing a committed file plus adding an untracked file means the working
/// tree is NOT clean — `status_porcelain` must report it.
#[test]
fn status_porcelain_detects_modified_and_untracked() {
    let dir = init_repo();
    fs::write(dir.path().join("tracked.txt"), b"original\n").unwrap();
    run(dir.path(), &["add", "tracked.txt"]);
    run(dir.path(), &["commit", "-q", "-m", "base"]);

    // Modify the committed file in the working tree.
    fs::write(dir.path().join("tracked.txt"), b"modified content here\n").unwrap();
    // Add an untracked file.
    fs::write(dir.path().join("untracked.txt"), b"new\n").unwrap();

    let repo = Repo::discover(dir.path()).expect("discover");
    let status = repo.status_porcelain().expect("status_porcelain");

    assert_eq!(
        status.modified.len(),
        1,
        "expected exactly 1 modified, got {:?}",
        status.modified
    );
    assert_eq!(
        status.untracked.len(),
        1,
        "expected exactly 1 untracked, got {:?}",
        status.untracked
    );
    assert_eq!(
        status.modified[0].as_str(),
        Some("tracked.txt"),
        "wrong modified path: {:?}",
        status.modified
    );
    assert_eq!(
        status.untracked[0].as_str(),
        Some("untracked.txt"),
        "wrong untracked path: {:?}",
        status.untracked
    );
    // Nothing was staged, so all staged buckets stay empty.
    assert!(status.staged_added.is_empty());
    assert!(status.staged_modified.is_empty());
    assert!(status.staged_deleted.is_empty());
}
