//! Gap B regression: the git-history index must be **shared** across worktrees of one clone rather
//! than rebuilt per worktree. It is derived entirely from the shared `.git` object database, so a
//! linked worktree resolves the index to the MAIN worktree's `.basemind` (mirroring the blob-store
//! sharing). This turns a per-worktree history rebuild into a one-time cost.

use std::path::Path;
use std::process::Command;

use basemind::git_history::shared_history_basemind_dir;

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

    let root_of = |bm: &Path| std::fs::canonicalize(bm.parent().unwrap()).expect("canonicalize");

    assert_eq!(
        root_of(&wt_bm),
        root_of(&main_bm),
        "a linked worktree must resolve the git-history index to the MAIN worktree's .basemind"
    );
    assert_ne!(
        root_of(&wt_bm),
        std::fs::canonicalize(&wt).unwrap(),
        "a linked worktree must NOT build its own per-worktree git-history index"
    );
    assert_eq!(root_of(&main_bm), std::fs::canonicalize(&main).unwrap());
}
