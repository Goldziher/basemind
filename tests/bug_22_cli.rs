//! Regression test for bug #22: `basemind cache clear --component views:<name>` clears a
//! single view, leaving the other views (and shared blobs) intact — previously the only
//! option, `--component views`, removed every view at once.

#![cfg(unix)]

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
}

fn git(repo: &Path, args: &[&str]) {
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

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("run basemind")
}

#[test]
fn cache_clear_single_view_leaves_others_intact() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("a.rs"), b"pub fn a() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);

    assert!(run(root, &["scan"]).status.success(), "working scan");
    assert!(run(root, &["scan", "--rev", "HEAD"]).status.success(), "rev scan");

    let views_dir = basemind::store::workspace_cache_dir(root).join("views");
    let rev_view = std::fs::read_dir(&views_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("rev-"))
        .expect("a rev-* view exists after `scan --rev`");

    let out = run(root, &["cache", "clear", "--component", &format!("views:{rev_view}")]);
    assert!(
        out.status.success(),
        "single-view clear failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !views_dir.join(&rev_view).exists(),
        "the named rev view must be removed"
    );
    assert!(
        views_dir.join("working").exists(),
        "the working view must survive a single-view clear"
    );
}
