//! Unit tests for the [`Registry`](super::Registry). Included from `mod.rs` via
//! `#[cfg(test)] #[path = "tests.rs"] mod tests;`, so `super` resolves to the `registry` module.
//! Fixtures are built with real `git` plumbing (`git init` / `git worktree add`) to exercise the
//! enumeration path end to end. Each test uses its own registry directory for isolation.

use std::path::Path;
use std::process::Command;

use super::*;

/// Run a git command in `cwd`, asserting success.
fn git(args: &[&str], cwd: &Path) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run a git command in `cwd` and return trimmed stdout.
fn git_capture(args: &[&str], cwd: &Path) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A committed git repo on branch `main` with one source file. Returns the main worktree path.
fn init_repo(main: &Path) {
    std::fs::create_dir_all(main).expect("mkdir main");
    git(&["init", "-q", "-b", "main"], main);
    git(&["config", "user.email", "t@example.com"], main);
    git(&["config", "user.name", "Test"], main);
    std::fs::write(main.join("a.rs"), b"pub fn alpha() {}\n").expect("write a.rs");
    git(&["add", "."], main);
    git(&["commit", "-qm", "init"], main);
}

#[test]
fn registers_git_repo_with_worktrees_and_branches() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    init_repo(&main);
    let wt = tmp.path().join("wt");
    git(
        &["worktree", "add", "-q", "-b", "feature", wt.to_str().expect("utf8")],
        &main,
    );

    let main_sha = git_capture(&["rev-parse", "refs/heads/main"], &main);
    let feature_sha = git_capture(&["rev-parse", "refs/heads/feature"], &main);

    let mut registry = Registry::open(&tmp.path().join("registry")).expect("open registry");
    let key = registry.register_workspace(&main).expect("register");

    let workspaces = registry.workspaces();
    assert_eq!(workspaces.len(), 1, "one workspace registered");
    assert_eq!(workspaces[0].key, key);
    assert_eq!(workspaces[0].kind, WorkspaceKind::Git);
    let repo_id = workspaces[0].repo_id.clone().expect("git workspace has a repo id");

    let worktrees = registry.worktrees(&repo_id);
    assert_eq!(worktrees.len(), 2, "main + one linked worktree");
    let names: Vec<&str> = worktrees.iter().map(|w| w.name.as_str()).collect();
    assert!(names.contains(&"(main)"), "main worktree present: {names:?}");
    assert!(names.contains(&"wt"), "linked worktree present: {names:?}");

    let branches = registry.branches(&repo_id);
    assert_eq!(branches.len(), 2, "main + feature branches");
    let main_branch = branches.iter().find(|b| b.name == "main").expect("main branch");
    let feature_branch = branches.iter().find(|b| b.name == "feature").expect("feature branch");
    assert_eq!(main_branch.head_sha, main_sha, "main head sha matches git");
    assert_eq!(feature_branch.head_sha, feature_sha, "feature head sha matches git");
}

#[test]
fn plain_dir_registers_without_repo() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let plain = tmp.path().join("media");
    std::fs::create_dir_all(&plain).expect("mkdir media");

    let mut registry = Registry::open(&tmp.path().join("registry")).expect("open registry");
    let key = registry.register_workspace(&plain).expect("register");

    let record = registry.get_workspace(&key).expect("workspace row");
    assert_eq!(record.kind, WorkspaceKind::Plain);
    assert_eq!(record.repo_id, None, "a plain dir has no repo id");
    assert_eq!(record.main_worktree, None);
    assert!(registry.repos().is_empty(), "no repo rows for a plain workspace");
}

#[test]
fn claim_is_exclusive_and_release_frees_it() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    init_repo(&main);

    let mut registry = Registry::open(&tmp.path().join("registry")).expect("open registry");
    let key = registry.register_workspace(&main).expect("register");
    let repo_id = registry.get_workspace(&key).expect("row").repo_id.expect("repo id");

    assert!(
        registry.claim_worktree(&repo_id, "(main)", "agent-a").expect("claim a"),
        "first claim by agent-a succeeds"
    );
    assert!(
        !registry.claim_worktree(&repo_id, "(main)", "agent-b").expect("claim b"),
        "agent-b cannot claim a worktree agent-a holds"
    );
    assert!(
        registry
            .claim_worktree(&repo_id, "(main)", "agent-a")
            .expect("reclaim a"),
        "agent-a re-claiming its own worktree is idempotent"
    );
    assert!(
        !registry
            .release_worktree(&repo_id, "(main)", "agent-b")
            .expect("release b"),
        "agent-b cannot release agent-a's claim"
    );
    assert!(
        registry
            .release_worktree(&repo_id, "(main)", "agent-a")
            .expect("release a"),
        "agent-a releases its own claim"
    );
    assert!(
        registry
            .claim_worktree(&repo_id, "(main)", "agent-b")
            .expect("claim b after release"),
        "agent-b can claim once freed"
    );
}

#[test]
fn prune_missing_drops_a_deleted_worktree() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    init_repo(&main);
    let wt = tmp.path().join("wt");
    git(
        &["worktree", "add", "-q", "-b", "feature", wt.to_str().expect("utf8")],
        &main,
    );

    let mut registry = Registry::open(&tmp.path().join("registry")).expect("open registry");
    let key = registry.register_workspace(&main).expect("register");
    let repo_id = registry.get_workspace(&key).expect("row").repo_id.expect("repo id");
    assert_eq!(registry.worktrees(&repo_id).len(), 2, "both worktrees registered");

    std::fs::remove_dir_all(&wt).expect("delete linked worktree checkout");
    let removed = registry.prune_missing().expect("prune");
    assert_eq!(removed, 1, "exactly the vanished linked worktree is pruned");

    let remaining = registry.worktrees(&repo_id);
    assert_eq!(remaining.len(), 1, "only the main worktree survives");
    assert_eq!(remaining[0].name, "(main)");
}

#[test]
fn refresh_from_root_adds_new_and_drops_stale_rows() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    init_repo(&main);

    let mut registry = Registry::open(&tmp.path().join("registry")).expect("open registry");
    let key = registry.register_workspace(&main).expect("register");
    let repo_id = registry.get_workspace(&key).expect("row").repo_id.expect("repo id");
    assert_eq!(registry.branches(&repo_id).len(), 1, "only main branch at first");
    assert_eq!(registry.worktrees(&repo_id).len(), 1, "only main worktree at first");

    // Add a branch and a linked worktree with real git plumbing, then refresh from the root.
    git(&["branch", "feature"], &main);
    let wt = tmp.path().join("wt");
    git(&["worktree", "add", "-q", wt.to_str().expect("utf8"), "feature"], &main);
    registry.refresh_from_root(&main).expect("refresh from root");

    let branches = registry.branches(&repo_id);
    let branch_names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
    assert!(branch_names.contains(&"main"), "main branch present: {branch_names:?}");
    assert!(
        branch_names.contains(&"feature"),
        "new feature branch surfaced after refresh: {branch_names:?}"
    );
    let worktree_names: Vec<String> = registry.worktrees(&repo_id).iter().map(|w| w.name.clone()).collect();
    assert!(
        worktree_names.contains(&"wt".to_string()),
        "new linked worktree surfaced after refresh: {worktree_names:?}"
    );

    // Delete the feature branch and drop the worktree; refresh must reconcile the stale rows away.
    git(&["worktree", "remove", "--force", wt.to_str().expect("utf8")], &main);
    git(&["branch", "-D", "feature"], &main);
    registry.refresh_from_root(&main).expect("refresh after deletions");

    let branch_names: Vec<String> = registry.branches(&repo_id).iter().map(|b| b.name.clone()).collect();
    assert!(
        !branch_names.contains(&"feature".to_string()),
        "deleted branch dropped after refresh: {branch_names:?}"
    );
    let worktree_names: Vec<String> = registry.worktrees(&repo_id).iter().map(|w| w.name.clone()).collect();
    assert!(
        !worktree_names.contains(&"wt".to_string()),
        "removed worktree dropped after refresh: {worktree_names:?}"
    );
    assert_eq!(registry.worktrees(&repo_id).len(), 1, "only the main worktree survives");
}

#[test]
fn refresh_repo_by_id_reconciles_from_recorded_main_root() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    // Register the canonical path so the repo id computed at register time equals the one
    // `refresh_repo` re-derives from the recorded (canonical) main root — otherwise a symlinked
    // tempdir (`/var` vs `/private/var` on macOS) yields two ids and the refresh writes a new row.
    let main = tmp.path().canonicalize().expect("canonicalize tmp").join("main");
    init_repo(&main);

    let mut registry = Registry::open(&tmp.path().join("registry")).expect("open registry");
    let key = registry.register_workspace(&main).expect("register");
    let repo_id = registry.get_workspace(&key).expect("row").repo_id.expect("repo id");
    assert_eq!(registry.branches(&repo_id).len(), 1, "only main branch at first");

    // Create a new branch, then refresh by id (not root): the recorded main root is re-enumerated.
    git(&["branch", "topic"], &main);
    registry.refresh_repo(&repo_id).expect("refresh by id");
    let branch_names: Vec<String> = registry.branches(&repo_id).iter().map(|b| b.name.clone()).collect();
    assert!(
        branch_names.contains(&"topic".to_string()),
        "refresh_repo surfaced the new branch: {branch_names:?}"
    );

    // An unknown id is a no-op (must not error).
    registry
        .refresh_repo(&"path:/definitely/not/a/repo".to_string())
        .expect("refresh of an unknown id is a no-op");
}

#[test]
fn reopen_sees_persisted_rows() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    init_repo(&main);
    let registry_dir = tmp.path().join("registry");

    let repo_id = {
        let mut registry = Registry::open(&registry_dir).expect("open registry");
        let key = registry.register_workspace(&main).expect("register");
        registry.get_workspace(&key).expect("row").repo_id.expect("repo id")
    };

    let reopened = Registry::open(&registry_dir).expect("reopen registry");
    assert_eq!(reopened.workspaces().len(), 1, "workspace persisted across reopen");
    assert_eq!(reopened.worktrees(&repo_id).len(), 1, "main worktree persisted");
    assert!(reopened.get_repo(&repo_id).is_some(), "repo row persisted");
}
