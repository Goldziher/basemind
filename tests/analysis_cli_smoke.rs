//! `basemind code impact` and `basemind code cycles` end to end: the built binary over a scanned Rust
//! crate, in text and `--json`, the `--fail-on-cycle` exit codes, and a malformed request's usage
//! exit.
#![cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

fn git(root: &Path, args: &[&str]) {
    let status = basemind::testing::git_command()
        .args(args)
        .current_dir(root)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn write(root: &Path, rel: &str, body: &str) {
    let file = root.join(rel);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(file, body).expect("write");
}

/// An empty git repository.
fn repo() -> tempfile::TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    dir
}

/// Commit everything and scan.
fn commit_and_scan(root: &Path) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    let scan = basemind(root, &["scan", "--quiet"]);
    assert!(scan.status.success(), "{}", String::from_utf8_lossy(&scan.stderr));
}

/// `a` and `b` use each other, `c` uses `a`, `d` uses `c`; committed, then scanned.
fn crate_with_a_cycle() -> tempfile::TempDir {
    let dir = repo();
    let root = dir.path();
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
    commit_and_scan(root);
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

/// The exit code of `basemind code cycles` with `args`, and its stderr.
fn exit_of(root: &Path, args: &[&str]) -> (Option<i32>, String) {
    let mut full = vec!["code", "cycles"];
    full.extend_from_slice(args);
    let output = basemind(root, &full);
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn fail_on_cycle_passes_a_rust_module_cycle_unless_strict() {
    let dir = crate_with_a_cycle();
    let root = dir.path();

    let (code, stderr) = exit_of(root, &["--fail-on-cycle"]);
    assert_eq!(
        code,
        Some(0),
        "a cycle among Rust modules of one crate is legal: {stderr}"
    );
    let (code, stderr) = exit_of(root, &["--fail-on-cycle", "--strict"]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("cycles: 1 import cycle(s) listed"), "{stderr}");
    let (code, stderr) = exit_of(root, &["--fail-on-cycle", "--strict", "--path", "src/d.rs"]);
    assert_eq!(code, Some(0), "no cycle holds `src/d.rs`: {stderr}");
    let (code, stderr) = exit_of(root, &["--fail-on-cycle", "--strict", "--max-size", "1"]);
    assert_eq!(code, Some(0), "the 2-file cycle is over `--max-size`: {stderr}");

    let (code, _) = exit_of(root, &["--fail-on-cycle", "--check", "src/a.rs", "src/d.rs"]);
    assert_eq!(code, Some(2), "`--fail-on-cycle` gates a list, not a check");
    let (code, _) = exit_of(root, &["--strict"]);
    assert_eq!(code, Some(2), "`--strict` needs `--fail-on-cycle`");
}

/// `a.ts` imports `b.ts`, which holds `b_src`, and `c.ts` imports `a.ts`; committed, then scanned.
#[cfg(feature = "code-intel-js")]
fn typescript_repo(b_src: &str) -> tempfile::TempDir {
    let dir = repo();
    let root = dir.path();
    write(
        root,
        "a.ts",
        "import { bee } from './b';\nexport function ay() {\n  return bee();\n}\n",
    );
    write(root, "b.ts", b_src);
    write(
        root,
        "c.ts",
        "import { ay } from './a';\nexport const sea = () => ay();\n",
    );
    commit_and_scan(root);
    dir
}

#[cfg(feature = "code-intel-js")]
#[test]
fn fail_on_cycle_fails_a_typescript_cycle_and_passes_an_acyclic_graph() {
    let cyclic = typescript_repo(
        "import { ay } from './a';\nexport function bee() {\n  return 1;\n}\nexport const x = () => ay();\n",
    );
    let (code, stderr) = exit_of(cyclic.path(), &["--fail-on-cycle"]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("cycles: 1 import cycle(s) listed"), "{stderr}");
    let (code, stderr) = exit_of(cyclic.path(), &["--fail-on-cycle", "--path", "c.ts"]);
    assert_eq!(code, Some(0), "no cycle holds `c.ts`: {stderr}");

    let acyclic = typescript_repo("export function bee() {\n  return 1;\n}\n");
    let (code, stderr) = exit_of(acyclic.path(), &["--fail-on-cycle"]);
    assert_eq!(code, Some(0), "{stderr}");
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
fn impact_buckets_orders_and_gates_on_the_impacted_file_count() {
    let dir = crate_with_a_cycle();
    let root = dir.path();
    let impact = json(root, &["code", "impact", "src/c.rs"]);
    assert_eq!(impact["impacted_files"], 1, "{impact}");
    assert_eq!(impact["buckets"]["prod"], 1, "{impact}");
    assert_eq!(impact["buckets"]["test"], 0, "{impact}");
    assert_eq!(impact["order"][0]["path"], "src/c.rs", "{impact}");
    assert_eq!(impact["order"][1]["path"], "src/d.rs", "{impact}");
    assert_eq!(impact["order"][1]["bucket"], "prod", "{impact}");

    let within = basemind(root, &["code", "impact", "src/c.rs", "--fail-above", "1"]);
    assert_eq!(
        within.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&within.stderr)
    );
    let above = basemind(root, &["code", "impact", "src/c.rs", "--fail-above", "0"]);
    assert_eq!(
        above.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&above.stderr)
    );
    assert!(
        String::from_utf8_lossy(&above.stderr).contains("1 impacted file(s) exceed --fail-above 0"),
        "{}",
        String::from_utf8_lossy(&above.stderr)
    );
}

#[cfg(feature = "code-intel-js")]
#[test]
fn impact_grades_the_callers_and_lists_the_calls_of_a_typescript_symbol() {
    let dir = typescript_repo("export function bee() {\n  return 1;\n}\n");
    let impact = json(dir.path(), &["code", "impact", "--symbol", "ay"]);
    assert_eq!(impact["callers"].as_array().map(Vec::len), Some(1), "{impact}");
    let caller = &impact["callers"][0];
    assert_eq!(
        (&caller["path"], &caller["line"], &caller["depth"], &caller["calls"]),
        (
            &Value::from("c.ts"),
            &Value::from(2),
            &Value::from(1),
            &Value::from("ay")
        ),
        "{impact}"
    );
    assert_eq!(
        (&caller["resolution"], &caller["proven"]),
        (&Value::from("imported"), &Value::from(true))
    );
    assert_eq!(impact["calls"][0]["name"], "bee", "{impact}");
    assert_eq!(impact["calls"][0]["path"], "b.ts", "{impact}");
    assert_eq!(impact["calls"][0]["resolution"], "imported", "{impact}");
    assert_eq!(impact["dependencies"][0]["path"], "b.ts", "{impact}");
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
