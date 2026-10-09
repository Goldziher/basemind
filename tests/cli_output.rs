//! End-to-end checks of the CLI's output contract: `--format toon` renders, human output never
//! shortens a body, the timing footer stays off stdout, and `--cursor` / `--max-tokens` reach the
//! MCP tool.

use std::path::Path;
use std::process::{Command, Output};

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

/// A scanned repo with three files; `a.rs` holds one function whose body is far past 200 chars.
fn build_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    let long = "z".repeat(400);
    std::fs::write(
        root.join("a.rs"),
        format!("pub fn alpha() {{\n    let s = \"{long}\";\n    let _ = s;\n}}\n"),
    )
    .unwrap();
    std::fs::write(root.join("b.rs"), b"pub fn beta() {}\n").unwrap();
    std::fs::write(root.join("c.rs"), b"pub fn gamma() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    let scan = Command::new(bin())
        .args(["--root", root.to_str().unwrap(), "scan"])
        .output()
        .expect("scan");
    assert!(
        scan.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );
    dir
}

fn run(root: &Path, args: &[&str]) -> Output {
    let out = Command::new(bin())
        .args(["--root", root.to_str().unwrap()])
        .args(args)
        .output()
        .expect("run basemind");
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn code_expand_prints_the_whole_body_and_keeps_timing_off_stdout() {
    let dir = build_repo();
    let out = run(dir.path(), &["code", "expand", "a.rs", "alpha"]);
    let text = stdout(&out);
    assert!(text.contains(&"z".repeat(400)), "the body was shortened:\n{text}");
    assert!(!text.contains('…'), "no ellipsis in a raw body:\n{text}");
    assert!(!text.contains("startup"), "timing footer leaked onto stdout:\n{text}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("startup"), "timing footer must be on stderr:\n{err}");
}

#[test]
fn format_toon_renders_and_json_wins_over_it() {
    let dir = build_repo();
    let toon = stdout(&run(dir.path(), &["code", "symbols", "a", "--format", "toon"]));
    assert!(
        toon.contains("results[") && toon.contains("]{"),
        "expected a TOON table:\n{toon}"
    );
    let json = stdout(&run(
        dir.path(),
        &["--json", "code", "symbols", "a", "--format", "toon"],
    ));
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("--json prints JSON even with toon");
    assert!(parsed["results"].is_array(), "{json}");
}

#[test]
fn cursor_resumes_a_paged_listing() {
    let dir = build_repo();
    let first = stdout(&run(dir.path(), &["--json", "code", "files", "--limit", "1"]));
    let first: serde_json::Value = serde_json::from_str(&first).unwrap();
    let cursor = first["next_cursor"]
        .as_str()
        .expect("a 3-file listing at limit 1 has a next page");
    let second = stdout(&run(
        dir.path(),
        &["--json", "code", "files", "--limit", "1", "--cursor", cursor],
    ));
    let second: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_ne!(
        first["files"][0], second["files"][0],
        "the cursor must advance past the first page"
    );
}

#[test]
fn max_tokens_reaches_the_tool_and_flags_the_budget() {
    let dir = build_repo();
    let out = stdout(&run(
        dir.path(),
        &["--json", "code", "symbols", "a", "--max-tokens", "1"],
    ));
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        parsed["budgeted"], true,
        "a 1-token budget must trim and flag the list:\n{out}"
    );
}
