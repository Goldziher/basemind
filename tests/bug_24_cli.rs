//! Regression test for bug #24 (exit-code half): a scan with per-file read failures
//! but a successful index update must exit 0 — the read failure is non-fatal because
//! the index WAS updated. Previously the scan exited 2 whenever `read_failed > 0`,
//! masking an otherwise-successful run.
//!
//! Unix-only: simulates a read failure via `chmod 000`, which has no Windows analogue.

#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

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

/// Repo with one readable file (updates the index) and one unreadable file (read fail).
fn build_repo() -> TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("good.rs"), b"pub fn good() {}\n").unwrap();
    let bad = root.join("bad.rs");
    std::fs::write(&bad, b"pub fn bad() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();
    dir
}

#[test]
fn should_exit_zero_when_read_failed_but_index_updated() {
    let dir = build_repo();
    let root = dir.path();
    let output = Command::new(bin())
        .args(["--root", root.to_str().unwrap(), "scan"])
        .output()
        .expect("run basemind scan");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    assert!(
        combined.contains("read failed") || combined.contains("failed 1"),
        "expected a read failure in the report; got:\n{combined}"
    );
    assert!(
        combined.contains("updated 1"),
        "expected the readable file to update the index; got:\n{combined}"
    );

    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(root.join("bad.rs"), std::fs::Permissions::from_mode(0o644));

    assert_eq!(
        output.status.code(),
        Some(0),
        "scan with a per-file read failure but a successful index update must exit 0; got {:?}\n{combined}",
        output.status.code()
    );
}
