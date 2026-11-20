//! Change scopes over fixture repositories built with the git CLI, and the `changed` grammar.

use std::path::Path;
use std::process::Command;

use super::*;

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn write(root: &Path, rel: &str, body: &str) {
    let file = root.join(rel);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(file, body).expect("write");
}

fn commit(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", message]);
}

/// Three commits, the first tagged `v1` (annotated):
/// 1. `keep.rs`, `edit.rs`, `gone.rs`, `old.rs`
/// 2. `edit.rs` changed, `gone.rs` deleted
/// 3. `old.rs` moved to `new.rs`, `added.rs` created
fn fixture() -> (tempfile::TempDir, Repo) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["config", "tag.gpgsign", "false"]);
    write(root, "keep.rs", "keep\n");
    write(root, "edit.rs", "one\n");
    write(root, "gone.rs", "gone\n");
    write(root, "old.rs", "fn moved() {}\nfn stays() {}\nfn the_same() {}\n");
    commit(root, "first");
    git(root, &["tag", "-a", "v1", "-m", "v1"]);
    write(root, "edit.rs", "two\n");
    std::fs::remove_file(root.join("gone.rs")).expect("rm");
    commit(root, "second");
    git(root, &["mv", "old.rs", "new.rs"]);
    write(root, "added.rs", "added\n");
    commit(root, "third");
    let repo = Repo::discover(root).expect("repo");
    (dir, repo)
}

fn sorted(scope: &ChangeScope) -> Vec<String> {
    let mut out: Vec<String> = scope.paths().iter().map(|p| p.to_str_lossy().into_owned()).collect();
    out.sort();
    out
}

fn scope_of(repo: &Repo, value: &str) -> Result<ChangeScope, EngineError> {
    change_scope(repo, &ChangeSpec::parse(value)?.base)
}

#[test]
fn a_clean_working_tree_is_an_empty_scope_not_an_error() {
    let (_dir, repo) = fixture();
    let scope = scope_of(&repo, "").expect("scope");
    assert!(scope.is_empty());
    assert_eq!(scope.len(), 0);
}

#[test]
fn the_working_tree_scope_lists_staged_unstaged_and_each_untracked_file() {
    let (dir, repo) = fixture();
    let root = dir.path();
    write(root, "keep.rs", "kept no more\n");
    write(root, "staged.rs", "staged\n");
    git(root, &["add", "staged.rs"]);
    write(root, "fresh/a.rs", "a\n");
    write(root, "fresh/b.rs", "b\n");
    let scope = scope_of(&repo, "HEAD").expect("scope");
    assert_eq!(sorted(&scope), ["fresh/a.rs", "fresh/b.rs", "keep.rs", "staged.rs"]);
    assert!(scope.contains(&RelPath::from("keep.rs")));
    assert!(!scope.contains(&RelPath::from("edit.rs")));
}

#[test]
fn a_revision_scope_joins_its_commits_with_the_working_tree() {
    let (dir, repo) = fixture();
    write(dir.path(), "keep.rs", "dirty\n");
    let scope = scope_of(&repo, "HEAD~1").expect("scope");
    assert_eq!(sorted(&scope), ["added.rs", "keep.rs", "new.rs", "old.rs"]);
}

#[test]
fn a_file_restored_on_disk_to_the_revision_is_out_of_its_scope_only() {
    let (dir, repo) = fixture();
    // `edit.rs` is "two" at HEAD and was "one" at HEAD~2.
    write(dir.path(), "edit.rs", "one\n");
    let since_first = scope_of(&repo, "HEAD~2").expect("scope");
    assert!(!since_first.contains(&RelPath::from("edit.rs")));
    assert!(since_first.contains(&RelPath::from("gone.rs")));
    let since_head = scope_of(&repo, "").expect("scope");
    assert_eq!(sorted(&since_head), ["edit.rs"]);
}

#[test]
fn a_range_scope_names_deletions_and_both_sides_of_a_move() {
    let (_dir, repo) = fixture();
    let scope = scope_of(&repo, "HEAD~2..HEAD").expect("scope");
    assert_eq!(sorted(&scope), ["added.rs", "edit.rs", "gone.rs", "new.rs", "old.rs"]);
    let open_ended = scope_of(&repo, "HEAD~1..").expect("scope");
    assert_eq!(sorted(&open_ended), ["added.rs", "new.rs", "old.rs"]);
    let reversed = scope_of(&repo, "HEAD..HEAD~1").expect("scope");
    assert_eq!(sorted(&reversed), sorted(&open_ended));
}

#[test]
fn an_annotated_tag_names_its_commit() {
    let (dir, repo) = fixture();
    let range = scope_of(&repo, "v1..HEAD").expect("range");
    assert_eq!(range, scope_of(&repo, "HEAD~2..HEAD").expect("same range"));
    write(dir.path(), "edit.rs", "one\n");
    let working = scope_of(&repo, "v1").expect("working tree");
    assert_eq!(sorted(&working), ["added.rs", "gone.rs", "new.rs", "old.rs"]);
}

#[test]
fn the_staged_scope_leaves_unstaged_and_untracked_files_out() {
    let (dir, repo) = fixture();
    let root = dir.path();
    write(root, "keep.rs", "unstaged\n");
    write(root, "loose.rs", "untracked\n");
    write(root, "edit.rs", "three\n");
    git(root, &["add", "edit.rs"]);
    git(root, &["rm", "-q", "added.rs"]);
    let scope = change_scope(&repo, &ChangeBase::Staged).expect("scope");
    assert_eq!(sorted(&scope), ["added.rs", "edit.rs"]);
}

#[test]
fn an_unknown_revision_is_an_invalid_argument_naming_it() {
    let (_dir, repo) = fixture();
    for value in ["no-such-branch", "HEAD..no-such-branch", "no-such-branch..HEAD"] {
        let error = scope_of(&repo, value).expect_err(value);
        assert!(matches!(error, EngineError::InvalidArgument(_)), "{value}: {error:?}");
        assert!(error.to_string().contains("no-such-branch"), "{value}: {error}");
    }
}

#[test]
fn the_changed_grammar() {
    let tree = |rev: Option<&str>| ChangeBase::WorkingTree(rev.map(str::to_string));
    let range = |from: &str, to: &str| ChangeBase::Range {
        from: from.to_string(),
        to: to.to_string(),
    };
    let merge_base = |from: &str, to: &str| ChangeBase::MergeBase {
        from: from.to_string(),
        to: to.to_string(),
    };
    let cases = [
        ("", tree(None), false),
        (" HEAD ", tree(None), false),
        ("main~2", tree(Some("main~2")), false),
        ("a..b", range("a", "b"), false),
        ("a..", range("a", "HEAD"), false),
        ("..b", range("HEAD", "b"), false),
        ("a...b", merge_base("a", "b"), false),
        ("a...", merge_base("a", "HEAD"), false),
        ("...b", merge_base("HEAD", "b"), false),
        ("main...HEAD+importers", merge_base("main", "HEAD"), true),
        ("+importers", tree(None), true),
        ("v1 +importers", tree(Some("v1")), true),
        ("a..b+importers", range("a", "b"), true),
    ];
    for (value, base, importers) in cases {
        assert_eq!(
            ChangeSpec::parse(value).expect(value),
            ChangeSpec { base, importers },
            "{value:?}"
        );
    }
    for bad in ["..", "..+importers", "...", "...+importers"] {
        let error = ChangeSpec::parse(bad).expect_err(bad);
        assert!(error.to_string().contains("`A..B` or `A...B`"), "{bad}: {error}");
    }
}

/// A branch `feature` off `trunk`, both moved on since, checked out with uncommitted work:
/// - base: `shared.rs`, `stable.rs`
/// - `trunk`: `upstream.rs` added, `shared.rs` changed
/// - `feature` (`HEAD`): `mine.rs` added, `stable.rs` changed
/// - working tree: `stable.rs` changed again, `loose.rs` untracked
fn diverged() -> (tempfile::TempDir, Repo) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    write(root, "shared.rs", "shared\n");
    write(root, "stable.rs", "stable\n");
    commit(root, "base");
    git(root, &["branch", "trunk"]);
    git(root, &["checkout", "-q", "-b", "feature"]);
    write(root, "mine.rs", "mine\n");
    write(root, "stable.rs", "stable on feature\n");
    commit(root, "feature work");
    git(root, &["checkout", "-q", "trunk"]);
    write(root, "upstream.rs", "upstream\n");
    write(root, "shared.rs", "shared on trunk\n");
    commit(root, "upstream work");
    git(root, &["checkout", "-q", "feature"]);
    write(root, "stable.rs", "stable, uncommitted\n");
    write(root, "loose.rs", "loose\n");
    let repo = Repo::discover(root).expect("repo");
    (dir, repo)
}

#[test]
fn a_bare_revision_is_the_branch_since_its_merge_base_with_head() {
    let (_dir, repo) = diverged();
    let scope = scope_of(&repo, "trunk").expect("scope");
    assert_eq!(sorted(&scope), ["loose.rs", "mine.rs", "stable.rs"]);
}

#[test]
fn a_two_dot_range_still_diffs_the_trees_directly() {
    let (_dir, repo) = diverged();
    let scope = scope_of(&repo, "trunk..HEAD").expect("scope");
    assert_eq!(sorted(&scope), ["mine.rs", "shared.rs", "stable.rs", "upstream.rs"]);
}

#[test]
fn a_three_dot_range_is_what_its_right_side_changed_since_the_merge_base() {
    let (_dir, repo) = diverged();
    for (value, expected) in [
        ("trunk...HEAD", ["mine.rs", "stable.rs"]),
        ("trunk...", ["mine.rs", "stable.rs"]),
        ("...trunk", ["shared.rs", "upstream.rs"]),
        ("feature...trunk", ["shared.rs", "upstream.rs"]),
    ] {
        assert_eq!(sorted(&scope_of(&repo, value).expect(value)), expected, "{value}");
    }
    assert!(scope_of(&repo, "HEAD...HEAD").expect("same commit").is_empty());
}

#[test]
fn a_file_restored_on_disk_to_the_merge_base_is_out_of_the_bare_revision_scope() {
    let (dir, repo) = diverged();
    write(dir.path(), "stable.rs", "stable\n");
    let scope = scope_of(&repo, "trunk").expect("scope");
    assert_eq!(sorted(&scope), ["loose.rs", "mine.rs"]);
}

#[test]
fn histories_sharing_no_commit_have_no_merge_base() {
    let (dir, repo) = diverged();
    let root = dir.path();
    git(root, &["checkout", "-q", "--orphan", "island"]);
    commit(root, "unrelated");
    git(root, &["checkout", "-q", "-f", "feature"]);
    drop(repo);
    let repo = Repo::discover(root).expect("repo");
    for value in ["island", "island...HEAD", "HEAD...island"] {
        let error = scope_of(&repo, value).expect_err(value);
        assert!(matches!(error, EngineError::InvalidArgument(_)), "{value}: {error:?}");
        assert!(error.to_string().contains("share no commit"), "{value}: {error}");
    }
    let error = scope_of(&repo, "no-such-branch...HEAD").expect_err("unknown");
    assert!(error.to_string().contains("no-such-branch"), "{error}");
}

#[test]
fn an_expansion_adds_to_the_scope() {
    let mut scope = ChangeScope::default();
    scope.extend([RelPath::from("a.rs"), RelPath::from("a.rs"), RelPath::from("b.rs")]);
    assert_eq!(scope.len(), 2);
}
