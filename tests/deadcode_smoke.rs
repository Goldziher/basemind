//! `basemind code deadcode` end to end: the built binary over a scanned Rust crate holding one of
//! each finding, in text and `--json`, the `--tier`/`--kind` filters, the `--fail-on-dead-code`
//! exit codes, `[analysis] entry_points`, a malformed request's usage exit, and what a build without
//! an import resolver still reports.

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

/// A library crate: `orphan.rs` is declared by no `mod`, `used.rs` holds a private function nothing
/// names, and `lib.rs` imports a crate the manifest does not declare. Committed, then scanned.
struct Fixture {
    repo: tempfile::TempDir,
    comms: tempfile::TempDir,
}

fn crate_with_dead_code() -> Fixture {
    basemind::store::init_isolated_cache();
    let fixture = Fixture {
        repo: tempfile::tempdir().expect("tempdir"),
        comms: tempfile::tempdir().expect("comms dir"),
    };
    let root = fixture.repo.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    );
    write(
        root,
        "src/lib.rs",
        "mod used;\npub use used::api;\nuse missing_crate::gone;\n",
    );
    write(
        root,
        "src/used.rs",
        "pub fn api() { live(); }\nfn live() {}\nfn never_named() {}\n",
    );
    write(root, "src/orphan.rs", "pub fn lost() {}\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    let scan = basemind(&fixture, &["scan", "--quiet"]);
    assert!(scan.status.success(), "{}", String::from_utf8_lossy(&scan.stderr));
    fixture
}

fn basemind(fixture: &Fixture, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_basemind"))
        .arg("--root")
        .arg(fixture.repo.path())
        .args(args)
        .env("BASEMIND_COMMS_DIR", fixture.comms.path())
        .output()
        .expect("run basemind")
}

fn json(fixture: &Fixture, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let output = basemind(fixture, &full);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!("{args:?} is not JSON: {error}"))
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
#[test]
fn deadcode_reports_each_kind_with_its_tier_and_gates_ci() {
    let fixture = crate_with_dead_code();

    let report = json(&fixture, &["code", "deadcode"]);
    assert_eq!(report["files"][0]["path"], "src/orphan.rs", "{report}");
    assert_eq!(report["files"][0]["tier"], "high", "{report}");
    let symbols = report["symbols"].as_array().expect("symbols");
    assert!(
        symbols
            .iter()
            .any(|s| s["name"] == "never_named" && s["tier"] == "high" && s["line"] == 3),
        "{report}"
    );
    assert!(
        !symbols.iter().any(|s| s["name"] == "live" || s["name"] == "api"),
        "{report}"
    );
    assert_eq!(report["imports"][0]["specifier"], "missing_crate::gone", "{report}");
    assert!(report["coverage"].as_array().is_some_and(|c| !c.is_empty()), "{report}");

    let text = basemind(&fixture, &["code", "deadcode"]);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("never_named"));

    let only = json(&fixture, &["code", "deadcode", "--kind", "files", "--tier", "high"]);
    assert!(only["symbols"].is_null() && only["imports"].is_null(), "{only}");
    assert_eq!(only["counts"]["files"]["high"], 1, "{only}");

    let failing = basemind(&fixture, &["code", "deadcode", "--fail-on-dead-code"]);
    assert_eq!(
        failing.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&failing.stderr)
    );
    assert!(String::from_utf8_lossy(&failing.stderr).contains("finding(s)"));
    let clean = basemind(
        &fixture,
        &[
            "code",
            "deadcode",
            "--kind",
            "imports",
            "--tier",
            "high",
            "--fail-on-dead-code",
        ],
    );
    assert_eq!(
        clean.status.code(),
        Some(0),
        "a Rust unresolved import is `low`: {}",
        String::from_utf8_lossy(&clean.stderr)
    );

    write(
        fixture.repo.path(),
        "basemind.toml",
        "\"$schema\" = \"v1\"\n[analysis]\nentry_points = [\"src/orphan.rs\"]\n",
    );
    let configured = json(&fixture, &["code", "deadcode", "--kind", "files"]);
    assert_eq!(
        configured["files"].as_array().map(Vec::len),
        Some(0),
        "a configured entry point is reachable: {configured}"
    );
}

/// Without a resolver the graph has no edges, so files and imports are not judged; symbols still are.
#[cfg(not(any(feature = "code-intel-js", feature = "code-intel-stack")))]
#[test]
fn without_a_resolver_only_symbols_are_judged() {
    let fixture = crate_with_dead_code();
    let report = json(&fixture, &["code", "deadcode"]);
    assert_eq!(report["files"].as_array().map(Vec::len), Some(0), "{report}");
    let symbols = report["symbols"].as_array().expect("symbols");
    assert!(
        symbols
            .iter()
            .any(|s| s["name"] == "never_named" && s["tier"] == "high"),
        "{report}"
    );
    let rust = report["coverage"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["language"] == "rust"))
        .expect("rust coverage");
    assert_eq!(rust["analysed"], serde_json::json!(["symbols"]), "{report}");
    assert!(
        rust["note"]
            .as_str()
            .is_some_and(|note| note.contains("no import resolver")),
        "{report}"
    );
}

#[test]
fn a_malformed_request_exits_with_a_usage_error() {
    let fixture = crate_with_dead_code();
    let zero = basemind(&fixture, &["code", "deadcode", "--limit", "0"]);
    assert_eq!(zero.status.code(), Some(2), "{}", String::from_utf8_lossy(&zero.stderr));
    let unknown = basemind(&fixture, &["code", "deadcode", "--tier", "certain"]);
    assert_eq!(unknown.status.code(), Some(2));
}
