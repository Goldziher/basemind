//! `basemind code explain` end to end: the built binary over a scanned, committed Rust crate, on a
//! symbol and a file, in text and `--json`, with `--limit`, `--sections`, `--no-code` and `--depth`,
//! and a malformed request's usage exit.
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

/// `ay` in `a.rs` calls `bee` and is called from `b.rs` and `c.rs`; committed, then scanned.
fn crate_with_callers() -> tempfile::TempDir {
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
    write(root, "src/lib.rs", "mod a;\nmod b;\nmod c;\n");
    write(
        root,
        "src/a.rs",
        "use crate::b::bee;\n/// Calls bee.\npub fn ay() {\n    bee();\n}\n",
    );
    write(
        root,
        "src/b.rs",
        "use crate::a::ay;\npub fn bee() {}\npub fn _x() { ay(); }\n",
    );
    write(root, "src/c.rs", "use crate::a::ay;\npub fn sea() { ay(); }\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "add ay"]);
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

#[test]
fn a_symbol_briefing_names_its_definition_callers_callees_importers_and_history() {
    let dir = crate_with_callers();
    let root = dir.path();

    let ay = json(root, &["code", "explain", "ay"]);
    assert_eq!(ay["target"], "symbol", "{ay}");
    assert_eq!(ay["definition"]["path"], "src/a.rs", "{ay}");
    assert_eq!(
        (ay["definition"]["line"].as_u64(), ay["definition"]["end_line"].as_u64()),
        (Some(3), Some(5))
    );
    assert_eq!(ay["definition"]["doc"], "/// Calls bee.", "{ay}");
    assert_eq!(ay["body"], "pub fn ay() {\n    bee();\n}", "{ay}");
    assert_eq!(ay["callers"]["src/b.rs"][0]["line"], 3, "{ay}");
    assert_eq!(ay["callers"]["src/c.rs"][0]["line"], 2, "{ay}");
    assert_eq!(ay["callers_total"], 2, "{ay}");
    assert_eq!(ay["callees"], serde_json::json!(["bee"]), "{ay}");
    assert_eq!(ay["importers"], serde_json::json!(["src/b.rs", "src/c.rs"]), "{ay}");
    assert_eq!(ay["history"][0]["summary"], "add ay", "{ay}");
    assert!(ay.get("truncated_sections").is_none(), "{ay}");

    let cut = json(root, &["code", "explain", "src/a.rs:4", "--limit", "1", "--no-code"]);
    assert_eq!(cut["definition"]["name"], "ay", "{cut}");
    assert!(cut.get("body").is_none(), "{cut}");
    let sections: Vec<&str> = cut["truncated_sections"]
        .as_array()
        .expect("truncated_sections")
        .iter()
        .map(|section| section["section"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(sections, ["callers", "importers"], "{cut}");
    assert_eq!(cut["truncated_sections"][0]["total"], 2, "{cut}");
    assert!(
        cut["truncated_sections"][0]["next"]
            .as_str()
            .is_some_and(|next| next.starts_with("code_refs {")),
        "{cut}"
    );

    let text = basemind(root, &["code", "explain", "ay", "--sections", "callees"]);
    assert!(text.status.success(), "{}", String::from_utf8_lossy(&text.stderr));
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains("bee") && !text.contains("src/c.rs"), "{text}");
}

#[test]
fn a_file_briefing_names_its_outline_imports_importers_and_owners() {
    let dir = crate_with_callers();
    let root = dir.path();
    let file = json(root, &["code", "explain", "src/a.rs"]);
    assert_eq!(file["target"], "file", "{file}");
    assert_eq!(file["outline"][0]["name"], "ay", "{file}");
    assert_eq!(file["imports"], serde_json::json!(["src/b.rs"]), "{file}");
    assert_eq!(file["importers"], serde_json::json!(["src/b.rs", "src/c.rs"]), "{file}");
    assert_eq!(file["owners"][0]["author"], "t", "{file}");
    assert_eq!(file["owners"][0]["commits"], 1, "{file}");
    assert_eq!(file["owners_commits"], 1, "{file}");
    assert!(file["hotspot"]["signals"].is_array(), "{file}");
    assert_eq!(file["calls_in"][0]["path"], "src/b.rs", "{file}");
    assert_eq!(file["calls_in"][1]["path"], "src/c.rs", "{file}");
    assert_eq!(file["calls_out"][0]["path"], "src/b.rs", "{file}");
    assert_eq!(file["calls_out"][0]["names"], serde_json::json!(["bee"]), "{file}");
}

#[test]
fn depth_is_a_flag_of_symbol_briefings_only() {
    let dir = crate_with_callers();
    let root = dir.path();
    let deep = json(
        root,
        &["code", "explain", "ay", "--depth", "2", "--sections", "callers"],
    );
    assert!(deep["indirect_callers"].is_array(), "{deep}");
    let on_file = basemind(root, &["code", "explain", "src/a.rs", "--depth", "2"]);
    assert_eq!(
        on_file.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&on_file.stderr)
    );
}

#[test]
fn a_malformed_request_exits_with_a_usage_error() {
    let dir = crate_with_callers();
    let root = dir.path();
    let unknown = basemind(root, &["code", "explain", "nowhere"]);
    assert_eq!(
        unknown.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&unknown.stderr)
    );
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("--fuzzy"));
    let bad_section = basemind(root, &["code", "explain", "src/a.rs", "--sections", "callers"]);
    assert_eq!(bad_section.status.code(), Some(2));
}

/// The `explain` eval kind runs the op and scores its definition, call sites and callees: a right
/// gold scores 1 and a wrong one 0.
#[test]
fn the_explain_eval_kind_scores_the_briefing() {
    let dir = crate_with_callers();
    let root = dir.path();
    let tasks = root.join("tasks.jsonl");
    std::fs::write(
        &tasks,
        concat!(
            r#"{"id":"right","mode":"explain","args":{"target":"ay"},"#,
            r#""gold":["src/a.rs:3","src/b.rs:3","src/c.rs:2","->bee"]}"#,
            "\n",
            r#"{"id":"wrong","mode":"explain","args":{"target":"ay"},"gold":["src/z.rs:1"]}"#,
            "\n",
        ),
    )
    .expect("write tasks");
    let output = basemind(root, &["admin", "eval", "--tasks", tasks.to_str().expect("utf-8")]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let rows: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("`{line}`: {error}")))
        .collect();
    let row = |id: &str| {
        rows.iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("no {id}"))
    };
    for id in ["right", "wrong"] {
        assert_eq!(row(id)["ok"], true, "{}", row(id));
    }
    let score = |id: &str, key: &str| row(id)["score"][key].as_f64().unwrap_or(-1.0);
    assert_eq!((score("right", "precision"), score("right", "recall")), (1.0, 1.0));
    assert_eq!(score("wrong", "recall"), 0.0);
}
