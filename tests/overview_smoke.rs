//! `basemind overview` (#131) end to end: a Cargo workspace beside an npm package, scanned with the
//! built binary, then reported section by section. Every assertion names the exact rows, so an
//! empty section never passes. With a code-intel build the import graph also feeds
//! `most_imported`, on the overview and on `index_status`.

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

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

const FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.dependencies]\nserde = \"1\"\n",
    ),
    (
        "crates/core/Cargo.toml",
        "[package]\nname = \"core_lib\"\nversion = \"0.2.0\"\n\n[dependencies]\nserde = \"1\"\nregex = \"1\"\n\n[dev-dependencies]\ntempfile = \"3\"\n",
    ),
    (
        "crates/core/src/lib.rs",
        "mod util;\nmod other;\npub use crate::util::parse_value;\n\npub fn run_core() {}\npub struct CoreState;\n",
    ),
    (
        "crates/core/src/util.rs",
        "pub fn parse_value() {}\npub fn format_value() {}\npub const MAX_VALUE: u32 = 1;\n",
    ),
    (
        "crates/core/src/other.rs",
        "use crate::util::format_value;\n\npub fn call_other() { format_value(); }\npub struct OtherThing;\npub struct ThirdThing;\n",
    ),
    ("crates/core/tests/it.rs", "#[test]\nfn works_fine() {}\n"),
    (
        "crates/cli/Cargo.toml",
        "[package]\nname = \"cli\"\nversion = \"0.2.0\"\n",
    ),
    ("crates/cli/src/main.rs", "fn main() {}\n"),
    (
        "web/package.json",
        "{\"name\":\"web\",\"version\":\"1.0.0\",\"main\":\"src/index.ts\",\"scripts\":{\"build\":\"tsc\"},\"dependencies\":{\"zod\":\"^3\"}}",
    ),
    (
        "web/src/index.ts",
        "export function startApp() {}\nexport function stopApp() {}\nexport class AppShell {}\n",
    ),
];

/// A committed repository holding [`FILES`], scanned by the built binary.
fn scanned() -> tempfile::TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    for (rel, body) in FILES {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "initial layout"]);
    let status = Command::new(bin())
        .args(["--root", root.to_str().unwrap(), "scan", "--quiet"])
        .status()
        .expect("run basemind scan");
    assert!(status.success(), "basemind scan failed");
    dir
}

/// `basemind --root <root> --json <args>`: the parsed answer, or the exit code and stderr.
fn run(root: &Path, args: &[&str]) -> Result<Value, (i32, String)> {
    let output = Command::new(bin())
        .args(["--root", root.to_str().unwrap(), "--json"])
        .args(args)
        .output()
        .expect("run basemind");
    if !output.status.success() {
        return Err((
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{args:?} is not JSON: {e}\n{stdout}")))
}

fn indexed(body: &Value, rel: &str) -> bool {
    body["largest"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(|r| r["path"] == rel))
}

#[test]
fn overview_reports_each_section_from_the_index_and_the_manifests() {
    let dir = scanned();
    let root = dir.path();
    let body = run(root, &["overview", "--limit", "20"]).expect("overview");
    if !indexed(&body, "web/src/index.ts") {
        basemind::testing::unavailable(
            "BASEMIND_TEST_ALLOW_NO_GRAMMAR",
            "the typescript grammar is unavailable",
        );
        return;
    }

    assert_eq!(body["files"], 6, "{body}");
    let languages: Vec<&str> = body["languages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["language"].as_str().unwrap())
        .collect();
    assert_eq!(languages, ["rust", "typescript"], "{body}");
    assert_eq!(
        body["layout"],
        json!([
            { "path": "crates", "files": 5, "lines": 17, "language": "rust" },
            { "path": "web", "files": 1, "lines": 3, "language": "typescript" },
        ]),
        "{body}"
    );

    let manifests: Vec<(&str, &str)> = body["manifests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["path"].as_str().unwrap(), m["ecosystem"].as_str().unwrap()))
        .collect();
    assert_eq!(
        manifests,
        [
            ("Cargo.toml", "cargo"),
            ("crates/cli/Cargo.toml", "cargo"),
            ("crates/core/Cargo.toml", "cargo"),
            ("web/package.json", "npm"),
        ]
    );
    let core = &body["manifests"][2];
    assert_eq!(core["name"], "core_lib");
    assert_eq!(core["version"], "0.2.0");
    assert_eq!(core["dependencies"], json!({ "runtime": 2, "dev": 1 }));
    assert_eq!(core["dependency_names"], json!(["regex", "serde"]));
    assert!(
        core.get("targets").is_none(),
        "targets are reported under entry_points: {core}"
    );
    assert_eq!(body["manifests"][0]["members"], json!(["crates/*"]));

    assert_eq!(
        body["entry_points"],
        json!([
            { "manifest": "crates/cli/Cargo.toml", "kind": "bin", "name": "cli", "path": "crates/cli/src/main.rs" },
            { "manifest": "crates/core/Cargo.toml", "kind": "lib", "name": "core_lib", "path": "crates/core/src/lib.rs" },
            { "manifest": "web/package.json", "kind": "main", "path": "web/src/index.ts" },
            { "manifest": "web/package.json", "kind": "script", "name": "build" },
        ]),
        "{body}"
    );
    assert_eq!(body["entry_points_counted"], json!({ "test": 1 }), "{body}");

    assert_eq!(body["largest"][0]["path"], "crates/core/src/lib.rs", "{body}");
    assert_eq!(body["largest"][0]["lines"], 6, "{body}");
    assert_eq!(body["routes"], json!({ "count": 0, "files": 0 }), "{body}");

    let rust = body["naming"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["language"] == "rust")
        .unwrap_or_else(|| panic!("no rust naming row: {body}"));
    assert_eq!(rust["symbols"]["function"]["case"], "snake_case", "{body}");
    assert_eq!(rust["symbols"]["struct"]["case"], "PascalCase", "{body}");

    assert_eq!(body["recent"][0]["summary"], "initial layout", "{body}");
    assert_eq!(body["recent"].as_array().unwrap().len(), 1, "{body}");
}

#[test]
fn overview_takes_sections_and_a_limit_and_refuses_what_it_does_not_know() {
    let dir = scanned();
    let root = dir.path();

    let body = run(root, &["overview", "--sections", "layout,largest", "--limit", "1"]).expect("overview");
    let keys: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
    for absent in [
        "languages",
        "manifests",
        "entry_points",
        "most_imported",
        "naming",
        "recent",
    ] {
        assert!(!keys.contains(&absent), "`{absent}` was not asked for: {body}");
    }
    assert_eq!(body["layout"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body["largest"].as_array().unwrap().len(), 1, "{body}");
    assert!(body["omitted"]["largest"].as_u64().unwrap() >= 5, "{body}");

    let (code, stderr) = run(root, &["overview", "--sections", "layout,bogus"]).expect_err("an unknown section");
    assert_eq!(code, 2, "a bad section is a usage error: {stderr}");
    assert!(stderr.contains("unknown section `bogus`"), "{stderr}");
    let (code, stderr) = run(root, &["overview", "--limit", "0"]).expect_err("a zero limit");
    assert_eq!(code, 2, "{stderr}");
}

/// `most_imported` counts runtime importers over the import graph, here the Rust resolver's: `mod`
/// declarations are containment and are not counted, `use crate::…` is.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
#[test]
fn most_imported_ranks_files_by_runtime_importers_on_overview_and_status() {
    let dir = scanned();
    let root = dir.path();
    let body = run(root, &["overview", "--sections", "most_imported"]).expect("overview");
    assert_eq!(
        body["most_imported"],
        json!([{ "path": "crates/core/src/util.rs", "importers": 2 }]),
        "{body}"
    );
    assert_eq!(body["import_graph"]["complete"], true, "{body}");

    let status = run(root, &["status"]).expect("status");
    assert_eq!(
        status["most_imported"],
        json!([{ "path": "crates/core/src/util.rs", "importers": 2 }]),
        "{status}"
    );
}
