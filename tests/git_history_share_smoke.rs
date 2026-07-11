//! Gap B regression: the git-history index must be **shared** across worktrees of one clone rather
//! than rebuilt per worktree. It is derived entirely from the shared `.git` object database, so a
//! linked worktree resolves the index to the MAIN worktree's workspace cache dir (keyed on the main
//! worktree root, in the now machine-global XDG store). This turns a per-worktree history rebuild
//! into a one-time cost.

use std::path::Path;
use std::process::Command;

use basemind::git_history::shared_history_basemind_dir;
use basemind::store::workspace_cache_dir;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn git_history_index_is_shared_from_linked_worktree() {
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    std::fs::create_dir(&main).unwrap();

    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "Test"]);
    std::fs::write(main.join("f.txt"), b"hello\n").unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-qm", "init"]);

    let wt = tmp.path().join("wt");
    git(&main, &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()]);

    let main_bm = shared_history_basemind_dir(&main);
    let wt_bm = shared_history_basemind_dir(&wt);

    // Both the main and the linked worktree must resolve the git-history index to the SAME
    // workspace cache dir — the one keyed on the main worktree root.
    assert_eq!(
        wt_bm, main_bm,
        "a linked worktree must resolve the git-history index to the MAIN worktree's workspace cache"
    );
    assert_eq!(
        main_bm,
        workspace_cache_dir(&main),
        "the shared index lives under the main worktree's workspace cache dir"
    );
    assert_ne!(
        wt_bm,
        workspace_cache_dir(&wt),
        "a linked worktree must NOT build its own per-worktree git-history index (distinct key)"
    );
}
