//! `basemind code deadcode` end to end: the built binary over a scanned Rust crate holding one of
//! each finding, in text and `--json`, the `--tier`/`--kind` filters, the `--fail-on-dead-code`
//! exit codes (`high` by default, the given tier otherwise, and a refusal to pass a report that
//! judged nothing), `[analysis] entry_points`, a malformed request's usage exit, what a build
//! without an import resolver still reports, and each language's export and entry rules over a
//! repository of Go, TypeScript, Python, Java and Rust.

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

fn fixture() -> Fixture {
    basemind::store::init_isolated_cache();
    let fixture = Fixture {
        repo: tempfile::tempdir().expect("tempdir"),
        comms: tempfile::tempdir().expect("comms dir"),
    };
    let root = fixture.repo.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    fixture
}

/// Commit everything and scan.
fn commit_and_scan(fixture: &Fixture) {
    let root = fixture.repo.path();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    let scan = basemind(fixture, &["scan", "--quiet"]);
    assert!(scan.status.success(), "{}", String::from_utf8_lossy(&scan.stderr));
}

fn crate_with_dead_code() -> Fixture {
    let fixture = fixture();
    let root = fixture.repo.path();
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
    commit_and_scan(&fixture);
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
    let stderr = String::from_utf8_lossy(&failing.stderr);
    assert!(stderr.contains("2 finding(s) at or above tier `high`"), "{stderr}");
    let only_low = basemind(
        &fixture,
        &["code", "deadcode", "--kind", "imports", "--fail-on-dead-code"],
    );
    assert_eq!(
        only_low.status.code(),
        Some(0),
        "a bare gate ignores `low` (the Rust unresolved import): {}",
        String::from_utf8_lossy(&only_low.stderr)
    );
    let at_low = basemind(
        &fixture,
        &[
            "code",
            "deadcode",
            "--kind",
            "imports",
            "--tier",
            "low",
            "--fail-on-dead-code",
        ],
    );
    assert_eq!(
        at_low.status.code(),
        Some(1),
        "`--tier low` gates on `low`: {}",
        String::from_utf8_lossy(&at_low.stderr)
    );
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

/// Without a resolver the graph has no edges, so files and imports are not judged; symbols still are,
/// and a gate over files or imports refuses to pass on a report that judged nothing.
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
    assert!(
        report["incomplete"][0]
            .as_str()
            .is_some_and(|why| why.starts_with("files and imports: no indexed language has an import resolver")),
        "{report}"
    );

    let files = basemind(
        &fixture,
        &["code", "deadcode", "--kind", "files", "--fail-on-dead-code"],
    );
    let stderr = String::from_utf8_lossy(&files.stderr);
    assert_eq!(files.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("the report is incomplete, so the gate judged nothing") && stderr.contains("--kind symbols"),
        "{stderr}"
    );
    let symbols = basemind(
        &fixture,
        &["code", "deadcode", "--kind", "symbols", "--fail-on-dead-code"],
    );
    let stderr = String::from_utf8_lossy(&symbols.stderr);
    assert_eq!(symbols.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("1 finding(s)"), "symbols alone are judged: {stderr}");
}

#[test]
fn a_malformed_request_exits_with_a_usage_error() {
    let fixture = crate_with_dead_code();
    // The engine refuses a `limit` out of range (clap takes any `u32`).
    for limit in ["0", "10001"] {
        let refused = basemind(&fixture, &["code", "deadcode", "--limit", limit]);
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert_eq!(refused.status.code(), Some(2), "{stderr}");
        assert!(
            stderr.contains("Error: `basemind code deadcode`: `--limit` is 1 to 10000\n"),
            "the engine's refusal, in the command's spelling: {stderr}"
        );
    }
    // clap refuses an unknown tier before the engine sees it.
    let unknown = basemind(&fixture, &["code", "deadcode", "--tier", "certain"]);
    assert_eq!(unknown.status.code(), Some(2));
}

/// One repository per language rule: a Go module with `internal/`, a Vite TypeScript package with
/// `exports`, a script and root-relative and query-suffixed imports, a Python package with
/// `__all__` and a console script, a JUnit 5 test class, and a tokio-style crate that declares a
/// module inside a macro and `include!`s a file.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
fn polyglot() -> Fixture {
    let fixture = fixture();
    let root = fixture.repo.path();
    write(root, "go.mod", "module example.com/m\n\ngo 1.22\n");
    write(
        root,
        "cmd/tool/main.go",
        "package main\n\nimport \"example.com/m/internal/used\"\n\nfunc main() { used.Used() }\n",
    );
    write(
        root,
        "internal/used/u.go",
        "package used\n\nfunc Used() {}\n\nfunc stale() {}\n",
    );
    write(root, "internal/dead/d.go", "package dead\n\nfunc Gone() {}\n");

    write(
        root,
        "web/package.json",
        r#"{ "name": "web", "exports": { ".": "./src/index.ts" }, "scripts": { "gen": "node tools/gen.js" } }"#,
    );
    write(
        root,
        "web/index.html",
        "<html><body><script type=\"module\" src=\"/src/main.tsx\"></script></body></html>\n",
    );
    write(root, "web/public/vite.svg", "<svg/>\n");
    write(root, "web/src/icon.svg", "<svg/>\n");
    write(root, "web/src/index.ts", "export function api() { return 1; }\n");
    write(
        root,
        "web/src/main.tsx",
        "import viteLogo from '/vite.svg';\nimport Icon from './icon.svg?react';\nimport { shown } from './extra';\n\
         const url = `https://x/${shown()}`;\nconsole.log(viteLogo, Icon, url);\n",
    );
    write(
        root,
        "web/src/extra.ts",
        "export function shown() { return 'a'; }\nexport function spare() { return 'b'; }\n",
    );
    write(root, "web/tools/gen.js", "console.log('gen');\n");

    write(
        root,
        "pyproject.toml",
        "[project]\nname = \"pkg\"\nversion = \"0.1.0\"\n\n[project.scripts]\ntool = \"pkg.cli:run\"\n",
    );
    write(root, "pkg/__init__.py", "from .api import load\n");
    write(root, "pkg/cli.py", "def run():\n    print('run')\n");
    write(
        root,
        "pkg/api.py",
        "__all__ = [\"load\"]\n\n\ndef load():\n    return 1\n\n\ndef _unused():\n    return 2\n",
    );

    write(
        root,
        "src/test/java/com/acme/OrderServiceTest.java",
        "package com.acme;\n\nimport org.junit.jupiter.api.Test;\n\nclass OrderServiceTest {\n    @Test\n    void placesAnOrder() {}\n}\n",
    );

    write(root, "Cargo.toml", "[package]\nname = \"rt\"\nversion = \"0.1.0\"\n");
    write(
        root,
        "src/lib.rs",
        "macro_rules! cfg_rt {\n    ($($item:item)*) => { $($item)* }\n}\n\ncfg_rt! {\n    pub mod runtime;\n}\n\n\
         include!(\"gen/table.rs\");\n",
    );
    write(root, "src/runtime.rs", "pub fn spawn() {}\n");
    write(root, "src/gen/table.rs", "pub const TABLE: [u8; 1] = [0];\n");
    commit_and_scan(&fixture);
    fixture
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
#[test]
fn each_language_rule_holds_on_a_polyglot_repository() {
    let fixture = polyglot();
    let report = json(&fixture, &["code", "deadcode", "--limit", "10000"]);
    let listed = |kind: &str, key: &str| -> Vec<(String, String)> {
        report[kind]
            .as_array()
            .unwrap_or_else(|| panic!("{kind}: {report}"))
            .iter()
            .map(|f| {
                (
                    f[key].as_str().unwrap_or_default().to_string(),
                    f["tier"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    };
    let files = listed("files", "path");
    let symbols = listed("symbols", "name");
    let imports = listed("imports", "specifier");
    let tier_of =
        |found: &[(String, String)], key: &str| found.iter().find(|(k, _)| k == key).map(|(_, tier)| tier.clone());

    // The check fired: what is dead in each language is found.
    assert_eq!(
        tier_of(&files, "internal/dead/d.go").as_deref(),
        Some("high"),
        "{report}"
    );
    assert_eq!(tier_of(&symbols, "stale").as_deref(), Some("high"), "{report}");
    assert_eq!(tier_of(&symbols, "_unused").as_deref(), Some("high"), "{report}");
    assert_eq!(
        tier_of(&symbols, "spare").as_deref(),
        Some("medium"),
        "an export no package publishes: {report}"
    );

    // Live code in each language is never `high`.
    for (path, tier) in &files {
        assert!(
            path == "internal/dead/d.go" || tier != "high",
            "{path} is reached or declared out of the resolver's sight: {report}"
        );
    }
    assert_ne!(
        tier_of(&files, "src/runtime.rs").as_deref(),
        Some("high"),
        "declared by `mod` inside a macro: {report}"
    );
    assert_eq!(
        tier_of(&files, "src/gen/table.rs").as_deref(),
        Some("low"),
        "`include!`d, which the resolver does not follow: {report}"
    );
    for live in ["web/tools/gen.js", "web/src/main.tsx", "web/src/extra.ts", "pkg/cli.py"] {
        assert_eq!(tier_of(&files, live), None, "{live} is an entry or reached: {report}");
    }
    for live in [
        "Used",
        "api",
        "shown",
        "run",
        "load",
        "OrderServiceTest",
        "placesAnOrder",
        "spawn",
    ] {
        assert_eq!(
            tier_of(&symbols, live),
            None,
            "{live} is used, exported or an entry: {report}"
        );
    }
    for live in ["/vite.svg", "./icon.svg?react"] {
        assert_eq!(tier_of(&imports, live), None, "{live} names a file on disk: {report}");
    }
    assert!(report["incomplete"].is_null(), "{report}");

    // `web/` holds only a `medium` finding: a bare gate passes, `--tier medium` fails.
    let web = basemind(&fixture, &["code", "deadcode", "--path", "web", "--fail-on-dead-code"]);
    assert_eq!(web.status.code(), Some(0), "{}", String::from_utf8_lossy(&web.stderr));
    let medium = basemind(
        &fixture,
        &[
            "code",
            "deadcode",
            "--path",
            "web",
            "--tier",
            "medium",
            "--fail-on-dead-code",
        ],
    );
    assert_eq!(
        medium.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&medium.stderr)
    );
}
