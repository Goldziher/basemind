//! Issue #62: a workspace root that is a *subdirectory* of a git repository must not have that
//! ancestor repository's history indexed for it.
//!
//! Git discovery ascends, and `config::discover_root_with_basemind` checks `basemind.toml` first, so
//! a subdirectory carrying that marker becomes the workspace root without being lifted to the git
//! root. The reporter's workspace was 4.4k files; the monorepo above it held 101 655 objects and
//! 275 MiB of packs, and the git-history sync drove gix over all of them.
//!
//! Discovery itself is deliberately left ascending (`tests/root_discovery_smoke.rs` pins that
//! behaviour) — it is the git-history *integration* that is scoped, via
//! `git_history::history_scope_ok`.
//!
//! Deliberately the ONLY test function in this binary: it sets `BASEMIND_GIT_DISCOVER_PARENTS`, and
//! `std::env::{set_var, remove_var}` are unsound with other threads running.

use std::fs;
use std::path::Path;
use std::process::Command;

use basemind::git::Repo;
use basemind::git_history::builder::{self, RebuildOutcome};
use basemind::git_history::{GitHistoryError, GitHistoryIndex};

const HATCH: &str = "BASEMIND_GIT_DISCOVER_PARENTS";

fn run(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn canonical(path: &Path) -> std::path::PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// A fresh, empty directory to hold one git-history index. Kept OUTSIDE the fixture repository so
/// the index never shows up as repository content.
fn index_dir(scratch: &Path, name: &str) -> std::path::PathBuf {
    let dir = scratch.join(name);
    fs::create_dir_all(&dir).expect("mkdir index dir");
    dir
}

#[test]
fn subdirectory_workspace_never_inherits_the_parent_repositorys_history() {
    let repo_tmp = tempfile::tempdir().expect("tempdir");
    let scratch_tmp = tempfile::tempdir().expect("tempdir");
    let outer = canonical(repo_tmp.path());
    let scratch = canonical(scratch_tmp.path());

    run(&outer, &["init", "-q", "-b", "main"]);
    run(&outer, &["config", "commit.gpgsign", "false"]);
    fs::write(outer.join("top.rs"), "fn top() {}\n").unwrap();
    run(&outer, &["add", "."]);
    run(&outer, &["commit", "-qm", "top"]);

    // The reporter's shape: a subdirectory that carries `basemind.toml` (so the root guard admits
    // it and it becomes the workspace root) but has no `.git` of its own.
    let sub = outer.join("clientes").join("zheus");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("basemind.toml"), "\"$schema\" = \"v1\"\n").unwrap();
    fs::write(sub.join("inner.rs"), "fn inner() {}\n").unwrap();
    run(&outer, &["add", "."]);
    run(&outer, &["commit", "-qm", "sub"]);

    // Discovery is UNCHANGED: it still ascends to the enclosing repository.
    let inherited = Repo::discover(&sub).expect("discovery still ascends to the parent repo");
    assert_eq!(
        canonical(inherited.workdir()),
        outer,
        "discovery must keep ascending — every CLI verb run from a subdirectory depends on it"
    );
    assert!(
        !inherited.origin_is_workdir(),
        "the subdirectory is not the repository's workdir"
    );

    // ... but the git-history build refuses that scope.
    let sub_dir = index_dir(&scratch, "sub");
    let sub_index = GitHistoryIndex::open(&sub_dir).expect("open index");
    let error = builder::sync(&sub_index, &inherited, &sub_dir).expect_err("sync must refuse");
    assert!(
        matches!(error, GitHistoryError::ForeignHistory { .. }),
        "expected ForeignHistory, got {error:?}"
    );
    assert!(
        sub_index.last_indexed_head_hex().is_none(),
        "a refused sync must leave no indexed head behind"
    );
    assert_eq!(
        sub_index.commit_count(),
        0,
        "no ancestor commit may reach a subdirectory workspace's index"
    );

    // The repository root itself is untouched by the gate.
    let owned = Repo::discover(&outer).expect("discover repo root");
    assert!(owned.origin_is_workdir());
    let root_dir = index_dir(&scratch, "root");
    let root_index = GitHistoryIndex::open(&root_dir).expect("open index");
    assert!(
        matches!(
            builder::sync(&root_index, &owned, &root_dir).expect("root sync"),
            RebuildOutcome::FullRebuild { commits: 2, .. }
        ),
        "the repository's own root still indexes its history"
    );

    // The escape hatch restores the pre-#62 behaviour.
    unsafe { std::env::set_var(HATCH, "1") };
    let hatch_dir = index_dir(&scratch, "hatch");
    let hatch_index = GitHistoryIndex::open(&hatch_dir).expect("open index");
    let outcome = builder::sync(&hatch_index, &inherited, &hatch_dir).expect("hatch sync");
    unsafe { std::env::remove_var(HATCH) };
    assert!(
        matches!(outcome, RebuildOutcome::FullRebuild { commits: 2, .. }),
        "{HATCH}=1 opts back into indexing the ancestor repository, got {outcome:?}"
    );
}
