//! `basemind code impact` and `basemind code cycles` end to end: the built binary over a scanned Rust
//! crate, in text and `--json`, and a malformed request's usage exit.
#![cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

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

/// `a` and `b` use each other, `c` uses `a`, `d` uses `c`; committed, then scanned.
fn crate_with_a_cycle() -> tempfile::TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    );
    write(root, "src/lib.rs", "mod a;\nmod b;\nmod c;\nmod d;\n");
    write(root, "src/a.rs", "use crate::b::bee;\npub fn ay() { bee(); }\n");
    write(
        root,
        "src/b.rs",
        "use crate::a::ay;\npub fn bee() {}\npub fn _x() { ay(); }\n",
    );
    write(root, "src/c.rs", "use crate::a::ay;\npub fn sea() { ay(); }\n");
    write(root, "src/d.rs", "use crate::c::sea;\npub fn dee() { sea(); }\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    let scan = basemind(root, &["scan", "--quiet"]);
    assert!(scan.status.success(), "{}", String::from_utf8_lossy(&scan.stderr));
    dir
}

fn basemind(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_basemind"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("run basemind")
}

fn json(root: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let output = basemind(root, &full);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!("{args:?} is not JSON: {error}"))
}

fn text(root: &Path, args: &[&str]) -> String {
    let output = basemind(root, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn cycles_lists_the_cycle_and_checks_a_proposed_import() {
    let dir = crate_with_a_cycle();
    let root = dir.path();

    let report = json(root, &["code", "cycles"]);
    assert_eq!(report["cycle_count"], 1, "{report}");
    assert_eq!(
        report["cycles"][0]["chain"], "src/a.rs:1 -> src/b.rs:1 -> src/a.rs",
        "{report}"
    );
    assert!(report["coverage"]["languages"].is_array(), "{report}");
    assert!(text(root, &["code", "cycles"]).contains("src/a.rs:1 -> src/b.rs:1 -> src/a.rs"));

    let check = json(root, &["code", "cycles", "--check", "src/a.rs", "src/d.rs"]);
    assert_eq!(check["would_close_cycle"], true, "{check}");
    assert_eq!(
        check["closing_chain"], "src/d.rs:1 -> src/c.rs:1 -> src/a.rs",
        "{check}"
    );
    let open = json(root, &["code", "cycles", "--check", "src/d.rs", "src/a.rs"]);
    assert_eq!(open["would_close_cycle"], false, "{open}");
}

#[test]
fn impact_walks_from_a_file_a_symbol_and_the_uncommitted_change() {
    let dir = crate_with_a_cycle();
    let root = dir.path();

    let by_file = json(root, &["code", "impact", "src/c.rs"]);
    assert_eq!(by_file["target"], "file", "{by_file}");
    assert_eq!(by_file["affected"][0]["path"], "src/d.rs", "{by_file}");
    assert_eq!(by_file["affected_files"], 1, "{by_file}");

    let by_symbol = json(root, &["code", "impact", "ay", "--depth", "2"]);
    assert_eq!(by_symbol["target"], "symbol", "{by_symbol}");
    assert_eq!(by_symbol["affected_files"], 3, "{by_symbol}");
    assert!(text(root, &["code", "impact", "ay", "--depth", "2"]).contains("src/d.rs"));

    write(root, "src/d.rs", "use crate::c::sea;\npub fn dee() { sea(); sea(); }\n");
    let nothing_imports_d = json(root, &["code", "impact", "--changed"]);
    assert_eq!(nothing_imports_d["scope_files"], 1, "{nothing_imports_d}");
    assert_eq!(nothing_imports_d["affected_files"], 0, "{nothing_imports_d}");
}

#[test]
fn a_malformed_request_exits_with_a_usage_error() {
    let dir = crate_with_a_cycle();
    let root = dir.path();
    let no_target = basemind(root, &["code", "impact"]);
    assert_eq!(
        no_target.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&no_target.stderr)
    );
    assert!(String::from_utf8_lossy(&no_target.stderr).contains("exactly one target"));
    let too_deep = basemind(root, &["code", "impact", "src/c.rs", "--depth", "9"]);
    assert_eq!(too_deep.status.code(), Some(2));
}
