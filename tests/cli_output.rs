//! End-to-end checks of the CLI's output contract: human output never shortens a body, the timing
//! footer stays off stdout, and `--cursor` / `--max-tokens` reach the MCP tool.

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
fn code_read_prints_the_whole_body_and_keeps_timing_off_stdout() {
    let dir = build_repo();
    let out = run(dir.path(), &["code", "read", "a.rs", "--symbol", "alpha"]);
    let text = stdout(&out);
    assert!(text.contains(&"z".repeat(400)), "the body was shortened:\n{text}");
    assert!(!text.contains('…'), "no ellipsis in a raw body:\n{text}");
    assert!(!text.contains("startup"), "timing footer leaked onto stdout:\n{text}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("startup"), "timing footer must be on stderr:\n{err}");
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

/// Human output writes a row a line, under its file and symbol for `code grep` and with each
/// commit's details once for `git blame`: a joined row list or a JSON blob would pass a substring
/// check, so each answer is checked line by line. Shas vary per run and are masked as `<sha>`.
#[test]
fn human_output_writes_a_row_a_line() {
    let dir = build_repo();
    let root = dir.path();
    let lines = |args: &[&str]| -> Vec<String> {
        stdout(&run(root, args))
            .lines()
            .map(|line| {
                line.split(' ')
                    .map(|word| {
                        let hex = word.len() == 12 && word.bytes().all(|b| b.is_ascii_hexdigit());
                        if hex { "<sha>" } else { word }
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    };

    assert_eq!(
        lines(&["code", "symbols", "beta"]),
        ["results:", "  b.rs:1: pub fn beta()"],
    );
    assert_eq!(
        lines(&["code", "grep", "let _"]),
        ["hits:", "  a.rs", "    alpha", "      3:     let _ = s;"],
    );
    assert_eq!(
        lines(&["git", "log"]),
        [
            "commits:",
            "  <sha> init",
            "files:",
            "  <sha>",
            "    a.rs  added",
            "    b.rs  added",
            "    c.rs  added",
        ],
    );
    let blame = lines(&["git", "blame", "a.rs"]);
    assert_eq!(blame[..3], ["hunks:", "  1-4 <sha>", "commits:"], "{blame:?}");
    assert_eq!(
        blame[3].split_whitespace().collect::<Vec<_>>(),
        ["author", "time", "summary"],
        "{blame:?}"
    );
    assert!(
        blame[4].starts_with("  <sha>  t  ") && blame[4].ends_with("  init"),
        "{blame:?}"
    );
    assert_eq!(blame.len(), 5, "{blame:?}");
}
