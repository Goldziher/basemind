//! Smoke tests for monorepo rootward `.basemind/` discovery: `discover_root_with_basemind` walks
//! UP from a start dir to the nearest ancestor that already holds a `.basemind/` cache (monorepo /
//! nested-git support), then falls back to git discovery, then to `start` unchanged.

use std::fs;

use basemind::config::{BASEMIND_DIR, discover_root_with_basemind, init_root};

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
fn inner_git_repo_bounds_the_basemind_walk() {
    // A nested subrepo (its own git root) checked out inside a polyrepo that has a root
    // `.basemind/`. Invoking from a subfolder of the subrepo must NOT climb across the subrepo
    // boundary into the outer polyrepo's cache — the subrepo's own root is the ceiling.
    let tmp = tempfile::tempdir().expect("tempdir");
    let outer = tmp.path().canonicalize().expect("canonicalize outer");
    fs::create_dir(outer.join(BASEMIND_DIR)).expect("mkdir outer .basemind");
    let inner = outer.join("crates").join("subrepo");
    fs::create_dir_all(&inner).expect("mkdir inner");
    git_init(&inner);
    let sub = inner.join("src").join("pkg");
    fs::create_dir_all(&sub).expect("mkdir inner subfolder");

    let resolved = discover_root_with_basemind(&sub);
    assert_eq!(
        resolved, inner,
        "the enclosing subrepo bounds the walk: resolves to the subrepo root, not the outer .basemind/"
    );
}

#[test]
fn inner_repo_own_basemind_wins_within_its_bound() {
    // The subrepo has its OWN `.basemind/`: a subfolder of it resolves to the subrepo root even
    // though an outer polyrepo also has one. Confirms the in-bound upward walk still works.
    let tmp = tempfile::tempdir().expect("tempdir");
    let outer = tmp.path().canonicalize().expect("canonicalize outer");
    fs::create_dir(outer.join(BASEMIND_DIR)).expect("mkdir outer .basemind");
    let inner = outer.join("crates").join("subrepo");
    fs::create_dir_all(&inner).expect("mkdir inner");
    git_init(&inner);
    fs::create_dir(inner.join(BASEMIND_DIR)).expect("mkdir inner .basemind");
    let sub = inner.join("src");
    fs::create_dir(&sub).expect("mkdir inner subfolder");

    let resolved = discover_root_with_basemind(&sub);
    assert_eq!(resolved, inner, "subrepo's own .basemind/ is found within its bound");
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
fn init_root_anchors_to_enclosing_git_repo_not_parent_basemind() {
    // The reported `basemind init` bug: run from inside a repo whose parent already has a
    // `.basemind/`, and init must scaffold the CURRENT repo — never travel up to the parent.
    let tmp = tempfile::tempdir().expect("tempdir");
    let parent = tmp.path().canonicalize().expect("canonicalize parent");
    fs::create_dir(parent.join(BASEMIND_DIR)).expect("mkdir parent .basemind");
    let repo = parent.join("project");
    fs::create_dir(&repo).expect("mkdir repo");
    git_init(&repo);
    let sub = repo.join("src");
    fs::create_dir(&sub).expect("mkdir repo subfolder");

    assert_eq!(
        init_root(&sub),
        repo,
        "init anchors to the enclosing git repo, not the parent"
    );
    assert_eq!(init_root(&repo), repo, "init at the repo root stays put");
}

#[test]
fn init_root_falls_back_to_start_when_not_in_a_git_repo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let start = tmp.path().canonicalize().expect("canonicalize start");
    assert_eq!(
        init_root(&start),
        start,
        "no git repo → init targets the start dir (cwd)"
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
