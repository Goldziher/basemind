//! Smoke tests for monorepo rootward `.basemind/` discovery: `discover_root_with_basemind` walks
//! UP from a start dir to the nearest ancestor that already holds a `.basemind/` cache (monorepo /
//! nested-git support), then falls back to git discovery, then to `start` unchanged.

use std::fs;

use basemind::config::{BASEMIND_DIR, discover_root_with_basemind};

/// `git init` a directory so it becomes its own git repo workdir.
fn git_init(dir: &std::path::Path) {
    let status = std::process::Command::new("git")
        .arg("init")
        .current_dir(dir)
        .status()
        .expect("run git init");
    assert!(status.success(), "git init succeeds in {dir:?}");
}

#[test]
fn resolves_upward_to_ancestor_with_basemind() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize root");
    fs::create_dir(root.join(BASEMIND_DIR)).expect("mkdir .basemind");
    let sub = root.join("crates").join("inner");
    fs::create_dir_all(&sub).expect("mkdir subfolder");

    let resolved = discover_root_with_basemind(&sub);
    assert_eq!(resolved, root, "subfolder resolves up to the dir holding .basemind/");
}

#[test]
fn ancestor_basemind_wins_over_inner_git_repo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let outer = tmp.path().canonicalize().expect("canonicalize outer");
    fs::create_dir(outer.join(BASEMIND_DIR)).expect("mkdir outer .basemind");
    let inner = outer.join("vendor").join("nested");
    fs::create_dir_all(&inner).expect("mkdir inner");
    git_init(&inner);

    let resolved = discover_root_with_basemind(&inner);
    assert_eq!(
        resolved, outer,
        ".basemind precedence: nested git repo resolves to the OUTER dir holding .basemind/"
    );
}

#[test]
fn falls_back_to_git_workdir_when_no_basemind() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().canonicalize().expect("canonicalize repo");
    git_init(&repo);
    let sub = repo.join("src").join("pkg");
    fs::create_dir_all(&sub).expect("mkdir subfolder");

    let resolved = discover_root_with_basemind(&sub);
    assert_eq!(resolved, repo, "no .basemind → resolves to the git workdir");
}

#[test]
fn a_basemind_regular_file_is_ignored_only_a_directory_counts() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize root");
    // ~keep A stray `.basemind` *file* (a merge/corruption artifact) is not a cache directory and must
    // ~keep not be adopted as the root — discovery only matches a real `.basemind/` directory.
    fs::write(root.join(BASEMIND_DIR), b"not a directory").expect("write .basemind file");
    let sub = root.join("child");
    fs::create_dir(&sub).expect("mkdir child");

    let resolved = discover_root_with_basemind(&sub);
    assert_eq!(
        resolved, sub,
        ".basemind as a file is skipped; no git → start unchanged"
    );
}

#[test]
fn returns_start_unchanged_when_neither_basemind_nor_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let start = tmp.path().canonicalize().expect("canonicalize start");
    let sub = start.join("plain");
    fs::create_dir(&sub).expect("mkdir plain subfolder");

    let resolved = discover_root_with_basemind(&sub);
    assert_eq!(resolved, sub, "no .basemind and no git → start unchanged");
}
