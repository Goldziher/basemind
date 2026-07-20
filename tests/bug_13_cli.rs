//! Regression test for bug #13: `-q` / `--quiet` must suppress subsystem INFO/WARN
//! logs during a scan, while an explicit `RUST_LOG` still wins.
//!
//! Drives the built binary over a throwaway git repo and inspects stderr (where all
//! diagnostics are routed). Independent + idempotent: own tempdir, cleaned on drop.

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

/// Tiny repo with one Rust file; deliberately no `.basemind/basemind.toml` and a
/// fresh index dir so the scan emits subsystem INFO ("Creating database", etc.).
fn build_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    dir
}

/// Run a fresh scan (wiping any prior index) and return captured stderr.
fn scan_stderr(root: &Path, extra: &[&str], rust_log: Option<&str>) -> String {
    let _ = std::fs::remove_dir_all(basemind::store::workspace_cache_dir(root));
    let mut args = vec!["--root", root.to_str().unwrap(), "scan"];
    args.extend_from_slice(extra);
    let mut cmd = Command::new(bin());
    cmd.args(&args).env_remove("RUST_LOG");
    if let Some(v) = rust_log {
        cmd.env("RUST_LOG", v);
    }
    let output = cmd.output().expect("run basemind scan");
    assert!(output.status.success(), "scan exited non-zero");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn info_line_count(stderr: &str) -> usize {
    stderr.lines().filter(|l| l.contains("INFO")).count()
}

#[test]
fn should_suppress_info_logs_when_quiet() {
    let dir = build_repo();
    let root = dir.path();
    let quiet = scan_stderr(root, &["-q"], None);
    assert_eq!(
        info_line_count(&quiet),
        0,
        "`-q` should suppress all INFO logs; got stderr:\n{quiet}"
    );
}

#[test]
fn should_emit_info_logs_by_default() {
    let dir = build_repo();
    let root = dir.path();
    let default = scan_stderr(root, &[], None);
    assert!(
        info_line_count(&default) > 0,
        "default verbosity should emit subsystem INFO logs; got stderr:\n{default}"
    );
}

#[test]
fn should_honor_explicit_rust_log_over_quiet() {
    let dir = build_repo();
    let root = dir.path();
    let forced = scan_stderr(root, &["-q"], Some("info"));
    assert!(
        info_line_count(&forced) > 0,
        "explicit RUST_LOG=info must win over `-q`; got stderr:\n{forced}"
    );
}
