//! A session that never writes leaves no Fjall store behind.
//!
//! fjall preallocates every journal at 64 MiB (`set_len`, no setting to lower it): sparse on APFS
//! and ext4, real bytes where the filesystem has no sparse `set_len`. A store a read created would
//! cost that per database per workspace, and its directory is never reaped. So each store is created
//! by its first write, and a read of a missing one answers empty: this binary drives the one-shot CLI
//! through read-only commands against a fresh data home and counts what is on disk afterwards.

use std::path::{Path, PathBuf};
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

/// A one-commit repository.
fn repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("a.rs"), b"pub fn alpha() {}\n").expect("write a.rs");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "init"]);
    dir
}

/// A data home and comms directory of their own, so no daemon or cache of this machine is reached.
struct Home {
    dir: TempDir,
}

impl Home {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("data home"),
        }
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    /// Run the one-shot CLI against `root`; returns whether it succeeded.
    fn run(&self, root: &Path, args: &[&str]) -> bool {
        let out = Command::new(env!("CARGO_BIN_EXE_basemind"))
            .arg("--root")
            .arg(root)
            .arg("--json")
            .args(args)
            .env("BASEMIND_DATA_HOME", self.data())
            .env("BASEMIND_COMMS_DIR", self.dir.path().join("comms"))
            .env("BASEMIND_NO_AUTOSPAWN", "1")
            .output()
            .expect("run basemind");
        out.status.success()
    }

    /// Every `*.fjall` directory and `*.jnl` file under the data home and the comms directory,
    /// relative to the home, sorted.
    fn stores(&self) -> (Vec<String>, usize) {
        let mut stores = Vec::new();
        let journals = walk(self.dir.path(), self.dir.path(), &mut stores);
        stores.sort();
        (stores, journals)
    }
}

/// Collect `*.fjall` directories under `dir` into `stores`; return the number of `*.jnl` files.
fn walk(base: &Path, dir: &Path, stores: &mut Vec<String>) -> usize {
    let mut journals = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if name.ends_with(".fjall") {
                let relative = path.strip_prefix(base).expect("under the home");
                stores.push(relative.to_string_lossy().replace('\\', "/"));
            }
            journals += walk(base, &path, stores);
        } else if name.ends_with(".jnl") {
            journals += 1;
        }
    }
    journals
}

/// The store names (`index.fjall`, …) in `stores`, sorted.
fn names(stores: &[String]) -> Vec<&str> {
    let mut names: Vec<&str> = stores.iter().filter_map(|path| path.rsplit('/').next()).collect();
    names.sort_unstable();
    names
}

/// Reads of a workspace that was never scanned: nothing to read, and nothing created.
#[test]
fn a_read_only_session_creates_no_store() {
    let repo = repo();
    let home = Home::new();

    assert!(home.run(repo.path(), &["status"]), "status");
    assert!(home.run(repo.path(), &["git", "log"]), "git log");
    // ~keep Unscanned, these fail (no index to read); what counts is what they leave on disk.
    home.run(repo.path(), &["memory", "get", "absent"]);
    home.run(repo.path(), &["memory", "list", "--prefix", "a"]);
    // ~keep With auto-spawn off and no daemon up the inbox read fails, likewise.
    home.run(repo.path(), &["agents", "inbox"]);

    assert_eq!(home.stores(), (Vec::new(), 0), "a read-only session created a store");
}

/// A scan writes the code index and the git history, one journal each. Reads after it, memory
/// included, add no store: the group memory store waits for the first note.
#[test]
fn reads_after_a_scan_add_no_store() {
    let repo = repo();
    let home = Home::new();
    assert!(home.run(repo.path(), &["scan"]), "scan");
    let (scanned, journals) = home.stores();
    assert_eq!(names(&scanned), ["git-history.fjall", "index.fjall"], "{scanned:?}");
    assert_eq!(journals, 2);

    assert!(home.run(repo.path(), &["status"]), "status");
    assert!(home.run(repo.path(), &["git", "log"]), "git log");
    // ~keep With no daemon a read-only CLI does not open the group index (ADR-0036 rule 3), so these
    // ~keep are refused; what counts is that they leave no notes store behind.
    home.run(repo.path(), &["memory", "get", "absent"]);
    home.run(repo.path(), &["memory", "list", "--prefix", "a"]);
    home.run(repo.path(), &["agents", "inbox"]);

    assert_eq!(home.stores(), (scanned, 2), "a read created a store");
}
