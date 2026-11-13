use super::*;

#[test]
fn selected_base_reports_committed_staged_and_untracked_changes() {
    fn git(root: &Path, args: &[&str]) {
        assert!(
            std::process::Command::new("git")
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.test")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.test")
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("old.rs"), "fn old() {}").unwrap();
    git(root, &["add", "old.rs"]);
    git(root, &["-c", "commit.gpgsign=false", "commit", "-qm", "base"]);
    std::fs::rename(root.join("old.rs"), root.join("renamed.rs")).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["-c", "commit.gpgsign=false", "commit", "-qm", "rename"]);
    std::fs::write(root.join("staged.rs"), "fn staged() {}").unwrap();
    git(root, &["add", "staged.rs"]);
    std::fs::write(root.join("staged.rs"), "fn changed() {}").unwrap();
    std::fs::create_dir(root.join("new")).unwrap();
    std::fs::write(root.join("new/untracked.rs"), "fn untracked() {}").unwrap();

    let repo = Repo::discover(root).unwrap();
    assert_eq!(repo.changed_tree_files("HEAD", "HEAD").unwrap(), 0);
    assert!(repo.changed_tree_files("HEAD~1", "HEAD").unwrap() > 0);
    let mut status = repo.status_against("HEAD~1").unwrap();
    status.staged_added.sort();
    assert_eq!(
        status.staged_added,
        ["renamed.rs", "staged.rs"].map(crate::path::RelPath::from)
    );
    assert_eq!(status.staged_deleted, [crate::path::RelPath::from("old.rs")]);
    assert_eq!(status.modified, [crate::path::RelPath::from("staged.rs")]);
    assert_eq!(status.untracked, [crate::path::RelPath::from("new/untracked.rs")]);
    assert_eq!(
        status.overlay_candidates().into_iter().collect::<Vec<_>>(),
        ["new/untracked.rs", "old.rs", "renamed.rs", "staged.rs"].map(crate::path::RelPath::from),
    );
    assert!(repo.status_against("missing-branch").is_err());
}
