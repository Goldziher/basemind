//! The memory scope of a repository without `origin` is the same from every worktree, so a note
//! saved in one is visible in the others (the notes store is one database per repository group).

use std::path::Path;

use basemind::git::{Repo, scope_key};

fn git(dir: &Path, args: &[&str]) {
    let status = basemind::testing::git_command()
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn worktrees_of_a_repository_without_origin_share_a_memory_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "Test"]);
    std::fs::write(main.join("f.txt"), b"hello\n").unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-qm", "init"]);
    let linked = tmp.path().join("linked");
    git(&main, &["worktree", "add", "-q", "--detach", linked.to_str().unwrap()]);

    let from_main = scope_key(&Repo::discover(&main).unwrap());
    let from_linked = scope_key(&Repo::discover(&linked).unwrap());
    assert_eq!(from_main, from_linked, "one scope for every worktree of the group");
}
