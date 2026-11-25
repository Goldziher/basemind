//! End-to-end checks of the CLI's output contract: human output never shortens a body, the timing
//! footer stays off stdout, `--cursor` / `--max-tokens` reach the MCP tool, a usage error is spelled
//! as the command line, and `scan` / `rescan` answer `--json`.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
}

fn git(repo: &Path, args: &[&str]) {
    let status = basemind::testing::git_command()
        .args(args)
        .current_dir(repo)
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
                        let hex = matches!(word.len(), 8 | 12) && word.bytes().all(|b| b.is_ascii_hexdigit());
                        let day = word.len() == 10
                            && word
                                .bytes()
                                .enumerate()
                                .all(|(i, b)| matches!(i, 4 | 7) == (b == b'-') && (b == b'-' || b.is_ascii_digit()));
                        if hex {
                            "<sha>"
                        } else if day {
                            "<date>"
                        } else {
                            word
                        }
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
            "",
            "commits (1 items):",
            "  sha           author  date        subject",
            "  <sha>  t       <date>  init",
        ],
    );
    assert_eq!(
        lines(&["git", "log", "--files"]),
        [
            "files:",
            "  <sha>",
            "    a.rs  added",
            "    b.rs  added",
            "    c.rs  added",
            "",
            "commits (1 items):",
            "  sha           author  date        subject",
            "  <sha>  t       <date>  init",
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

/// Run `basemind --root <root> <args>` expecting exit `code`; return its stderr.
fn run_failing(root: &Path, args: &[&str], code: i32) -> String {
    let out = Command::new(bin())
        .args(["--root", root.to_str().unwrap()])
        .args(args)
        .output()
        .expect("run basemind");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(code), "{args:?}: {stderr}");
    assert!(out.stdout.is_empty(), "{args:?} printed on stdout: {}", stdout(&out));
    stderr
}

/// A tool's refusal names the command that was typed and its flags, not the MCP tool and its fields:
/// the tool says `` `code_grep`: `limit` ``, the CLI `` `basemind code grep`: `--limit` ``.
#[test]
fn a_usage_error_is_spelled_as_the_command_line() {
    let dir = build_repo();
    let refusal = "`--limit` must be at least 1; omit it for the default page size.\n";
    for (args, command) in [
        (&["code", "grep", "alpha", "--limit", "0"][..], "basemind code grep"),
        (
            &["docs", "search", "releases", "--limit", "0"][..],
            "basemind docs search",
        ),
        (
            &["memory", "documents", "releases", "--limit", "0"][..],
            "basemind memory documents",
        ),
    ] {
        let stderr = run_failing(dir.path(), args, 2);
        let expected = format!("Error: `{command}`: {refusal}");
        assert!(
            stderr.ends_with(&expected),
            "{args:?}: expected {expected:?} in:\n{stderr}"
        );
    }
}

/// `docs search` is `docs_search`, as `memory documents` was: both run the one body, so they exit
/// alike, and a build without the document tier refuses both in the same words.
#[test]
fn docs_search_and_its_old_name_give_the_same_answer() {
    let dir = build_repo();
    let answer = |args: &[&str]| {
        let out = Command::new(bin())
            .args(["--root", dir.path().to_str().unwrap()])
            .args(args)
            .output()
            .expect("run basemind");
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let error: Vec<String> = stderr
            .lines()
            .skip_while(|line| !line.starts_with("Error: "))
            .map(str::to_string)
            .collect();
        (out.status.code(), error)
    };
    let new = answer(&["docs", "search", "releases"]);
    let old = answer(&["memory", "documents", "releases"]);
    assert_eq!(new, old);
    if !cfg!(feature = "documents") {
        assert_eq!(new.0, Some(1), "{new:?}");
        assert!(
            new.1
                .first()
                .is_some_and(|line| line.contains("`docs_search` needs the `documents` Cargo feature")),
            "{new:?}"
        );
    }
}

/// `scan --json` and `rescan --json` print the `index_rescan` response and nothing else on stdout.
#[test]
fn scan_and_rescan_print_the_index_rescan_response_as_json() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();
    std::fs::write(root.join("b.rs"), b"pub fn beta() {}\n").unwrap();
    let canonical = root.canonicalize().expect("canonical root").display().to_string();
    let report = |args: &[&str]| -> serde_json::Value {
        let mut value: serde_json::Value =
            serde_json::from_str(&stdout(&run(root, args))).unwrap_or_else(|e| panic!("{args:?} is not JSON: {e}"));
        let elapsed = value
            .as_object_mut()
            .and_then(|map| map.remove("elapsed_ms"))
            .unwrap_or_else(|| panic!("{args:?} has no elapsed_ms"));
        assert!(elapsed.is_u64(), "{args:?}: elapsed_ms {elapsed}");
        value
    };
    let counts = |scanned: u64, updated: u64, skipped_unchanged: u64| {
        serde_json::json!({
            "scanned": scanned,
            "updated": updated,
            "docs_indexed": 0,
            "docs_degraded": 0,
            "docs_text_fallback": 0,
            "doc_timeouts": 0,
            "removed": 0,
            "skipped_unchanged": skipped_unchanged,
            "skipped_no_lang": 0,
            "extract_failed": 0,
            "root": canonical,
        })
    };

    assert_eq!(report(&["--json", "scan", "--no-git-history"]), counts(2, 2, 0));
    assert_eq!(report(&["--json", "scan", "--no-git-history"]), counts(2, 0, 2));
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\npub fn gamma() {}\n").unwrap();
    assert_eq!(
        report(&["--json", "rescan", "a.rs", "--no-git-history"]),
        counts(1, 1, 0)
    );
}
