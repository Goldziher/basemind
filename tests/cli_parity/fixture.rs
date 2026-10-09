//! A small scanned git repo shared by the probing and behavioural layers. Uses the process-wide
//! isolated `BASEMIND_DATA_HOME` (`init_isolated_cache`), which the spawned CLI inherits, so neither
//! side can touch the developer's real index.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

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

/// Build the repo, commit it, and index it with the built binary (`basemind scan`).
pub fn scanned_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/alpha.rs"),
        "use std::collections::HashMap;\n\n/// Adds one.\npub fn alpha(n: i32) -> i32 {\n    n + 1\n}\n\npub struct Beta {\n    pub x: i32,\n}\n\n\
         impl Beta {\n    pub fn total(&self, m: &HashMap<i32, i32>) -> i32 {\n        alpha(self.x) + m.len() as i32\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/gamma.rs"),
        "use crate::alpha::alpha;\n\npub fn caller() -> i32 {\n    alpha(1) + alpha(2)\n}\n\npub fn other() -> i32 {\n    caller()\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("README.md"), "# fixture\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);

    let status = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["--root", root.to_str().unwrap(), "scan", "--quiet"])
        .status()
        .expect("run basemind scan");
    assert!(status.success(), "basemind scan failed");
    dir
}
