//! End-to-end tests for the value-gate task kinds of `basemind admin eval`: implementations,
//! read, the git kinds, paging through `next_cursor`, the baseline scored against the same gold,
//! and the per-kind ADR-0022 verdict in the report. Gold is worked out by hand from a two-commit
//! fixture, and every kind is also run with a wrong gold to show the score drops.
//!
//! Fixture (1-based lines), commit 1 then commit 2:
//! - `a.rs` v1: `pub trait Show {}` (1), `pub struct A;` (2), `impl Show for A {}` (3),
//!   `pub fn alpha() -> u8 {` (4) `    1` (5) `}` (6)
//! - `a.rs` v2: line 5 becomes `    2`, and `pub fn gamma() {}` is appended (7)
//! - `c.rs` (commit 2): `pub fn caller() {` (1) `    alpha();` (2) `    alpha();` (3) `}` (4)

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = basemind::testing::git_command()
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git in PATH");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Fixture {
    dir: TempDir,
    c1: String,
    c2: String,
}

fn fixture() -> Fixture {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    let v1 = "pub trait Show {}\npub struct A;\nimpl Show for A {}\npub fn alpha() -> u8 {\n    1\n}\n";
    std::fs::write(root.join("a.rs"), v1).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "first"]);
    let c1 = git(root, &["rev-parse", "HEAD"]);
    let v2 =
        "pub trait Show {}\npub struct A;\nimpl Show for A {}\npub fn alpha() -> u8 {\n    2\n}\npub fn gamma() {}\n";
    std::fs::write(root.join("a.rs"), v2).unwrap();
    std::fs::write(root.join("c.rs"), "pub fn caller() {\n    alpha();\n    alpha();\n}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "second"]);
    let c2 = git(root, &["rev-parse", "HEAD"]);
    let status = Command::new(bin())
        .args(["--root", root.to_str().unwrap(), "scan", "--quiet"])
        .status()
        .expect("run basemind scan");
    assert!(status.success(), "basemind scan failed");
    Fixture { dir, c1, c2 }
}

fn eval(root: &Path, lines: &[String], report: &Path) -> Vec<Value> {
    let tasks = root.join("tasks.jsonl");
    std::fs::write(&tasks, lines.join("\n")).unwrap();
    let out: Output = Command::new(bin())
        .args(["--root", root.to_str().unwrap(), "admin", "eval", "--tasks"])
        .arg(&tasks)
        .arg("--report")
        .arg(report)
        .output()
        .expect("run basemind admin eval");
    assert!(
        out.status.success(),
        "eval failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn task(id: &str, mode: &str, args: Value, gold: &[String], baseline: Value) -> String {
    serde_json::json!({"id": id, "mode": mode, "args": args, "gold": gold, "baseline": baseline}).to_string()
}

fn recall(rows: &[Value], id: &str, key: &str) -> f64 {
    let row = rows
        .iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no row {id}"));
    assert_eq!(row["ok"], true, "{row}");
    row[key]["recall"]
        .as_f64()
        .unwrap_or_else(|| panic!("no {key}.recall in {row}"))
}

/// `(id, mode, args, gold, baseline)` of one task per kind, gold worked out by hand.
fn kinds(f: &Fixture) -> Vec<(&'static str, &'static str, Value, Vec<String>, Value)> {
    use serde_json::json;
    let (c1, c2) = (f.c1.clone(), f.c2.clone());
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    vec![
        (
            "impls",
            "implementations",
            json!({"name": "Show", "relation": "implementations"}),
            s(&["a.rs:3"]),
            json!({"grep": "git grep -n -E 'impl.*Show.* for ' -- '*.rs'", "items": "grep_lines"}),
        ),
        (
            "read",
            "read",
            json!({"path": "a.rs", "symbol": "alpha"}),
            s(&["a.rs:4", "a.rs:5", "a.rs:6"]),
            json!({"grep": "git grep -n 'fn alpha' -- a.rs", "read": ["a.rs"], "items": "read"}),
        ),
        (
            "log",
            "git_path",
            json!({"path": "a.rs"}),
            vec![c2.clone(), c1.clone()],
            json!({"grep": "git log --oneline -- a.rs", "items": "shas"}),
        ),
        (
            "history",
            "git_symbol",
            json!({"path": "a.rs", "symbol": "alpha"}),
            vec![c2.clone(), c1.clone()],
            json!({"grep": "git log -s --format=%h -L '/fn alpha/,/^}/:a.rs'", "items": "shas"}),
        ),
        (
            "blame",
            "blame",
            json!({"path": "a.rs", "line": 4, "end_line": 5}),
            vec![format!("{c1}:4"), format!("{c2}:5")],
            json!({"grep": "git blame -L 4,5 -- a.rs", "items": "blame"}),
        ),
        (
            "blame_sym",
            "blame_symbol",
            json!({"path": "a.rs", "symbol": "alpha"}),
            vec![format!("{c1}:4"), format!("{c2}:5"), format!("{c1}:6")],
            json!({"grep": "git blame -L '/fn alpha/,/^}/' -- a.rs", "items": "blame"}),
        ),
        (
            "diff",
            "diff",
            json!({"path": "a.rs", "from": c1, "to": c2}),
            s(&["-a.rs:5", "+a.rs:5", "+a.rs:7"]),
            json!({"grep": format!("git diff {c1} {c2} -- a.rs"), "items": "diff"}),
        ),
        (
            "outline_diff",
            "diff_structural",
            json!({"path": "a.rs", "from": c1, "structural": true}),
            s(&["+gamma"]),
            json!({"grep": format!("git diff {c1} -- a.rs"), "items": "diff_symbols"}),
        ),
        (
            "around",
            "read_around",
            json!({"pattern": "alpha();", "path": "c.rs", "context": 1}),
            s(&["c.rs:1", "c.rs:2", "c.rs:3", "c.rs:4"]),
            json!({"grep": "git grep -n -C1 -F -e 'alpha();' -- c.rs", "items": "grep_context"}),
        ),
        (
            "paged",
            "references",
            json!({"name": "alpha", "limit": 1}),
            s(&["c.rs:2", "c.rs:3"]),
            json!({"grep": "git grep -n -w alpha -- c.rs", "items": "grep_lines"}),
        ),
    ]
}

#[test]
fn every_kind_and_its_baseline_score_full_recall_on_hand_worked_gold() {
    let f = fixture();
    let lines: Vec<String> = kinds(&f)
        .into_iter()
        .map(|(id, mode, args, gold, base)| task(id, mode, args, &gold, base))
        .collect();
    let report = f.dir.path().join("report.json");
    let rows = eval(f.dir.path(), &lines, &report);
    for (id, ..) in kinds(&f) {
        assert_eq!(recall(&rows, id, "score"), 1.0, "tool recall for {id}: {rows:?}");
        assert_eq!(recall(&rows, id, "baseline_score"), 1.0, "baseline recall for {id}");
    }
    let paged = rows.iter().find(|r| r["id"] == "paged").unwrap();
    assert!(
        paged["pages"].as_u64().unwrap() >= 2,
        "limit 1 over two call sites needs a second page: {paged}"
    );

    let rep: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(
        rep["gate"]["read_around"]["tool"], "code_grep",
        "read_around is one `code_grep` call with `context`"
    );
    for mode in [
        "implementations",
        "read",
        "git_path",
        "git_symbol",
        "blame",
        "diff",
        "read_around",
    ] {
        let gate = &rep["gate"][mode];
        assert_eq!(gate["tasks"], 1, "{mode}: {gate}");
        assert_eq!(gate["credited"], 1, "{mode}: {gate}");
        if rep["tokenizer"] == "o200k" {
            assert!(gate["pass"].is_boolean(), "{mode}: {gate}");
        } else {
            assert!(gate["pass"].is_null(), "bytes/4 counts carry no verdict: {gate}");
            assert!(gate["reason"].as_str().unwrap().contains("bytes/4"), "{gate}");
        }
    }
}

#[test]
fn a_wrong_gold_scores_both_arms_down_and_is_not_credited() {
    let f = fixture();
    let wrong_sha = "0".repeat(40);
    let lines: Vec<String> = kinds(&f)
        .into_iter()
        .map(|(id, mode, args, gold, base)| {
            // Same answer, gold moved: shifted lines, a sha that is not in the repo, a name never
            // defined.
            let wrong: Vec<String> = gold
                .iter()
                .map(|g| match g.rsplit_once(':') {
                    Some((p, l)) if l.parse::<u32>().is_ok() && p.len() == 40 => format!("{wrong_sha}:{l}"),
                    Some((p, l)) if l.parse::<u32>().is_ok() => format!("{p}:{}", l.parse::<u32>().unwrap() + 100),
                    _ if g.len() == 40 => wrong_sha.clone(),
                    _ => format!("{g}_never"),
                })
                .collect();
            task(id, mode, args, &wrong, base)
        })
        .collect();
    let report = f.dir.path().join("report.json");
    let rows = eval(f.dir.path(), &lines, &report);
    for (id, ..) in kinds(&f) {
        assert_eq!(recall(&rows, id, "score"), 0.0, "tool recall for {id}");
        assert_eq!(recall(&rows, id, "baseline_score"), 0.0, "baseline recall for {id}");
        let row = rows.iter().find(|r| r["id"] == id).unwrap();
        assert_eq!(row["savings"]["credited"], false, "{row}");
    }
    let rep: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(rep["gate"]["diff"]["credited"], 0);
    if rep["tokenizer"] == "o200k" {
        assert_eq!(rep["gate"]["diff"]["pass"], false);
    }
}

#[test]
fn a_failed_baseline_scores_recall_0_and_a_tool_error_still_runs_its_baseline() {
    use serde_json::json;
    let f = fixture();
    let refs = json!({"name": "alpha", "limit": 10});
    let gold = ["c.rs:2".to_string(), "c.rs:3".to_string()];
    let lines = vec![
        task(
            "answered",
            "references",
            refs.clone(),
            &gold,
            json!({"grep": "git grep -n -w alpha -- c.rs", "items": "grep_lines"}),
        ),
        // `true` prints nothing; `exit 3` fails. Both are baseline failures, not cheap answers.
        task(
            "silent",
            "references",
            refs.clone(),
            &gold,
            json!({"grep": "true", "items": "grep_lines"}),
        ),
        task(
            "broken",
            "references",
            refs,
            &gold,
            json!({"grep": "exit 3", "items": "grep_lines"}),
        ),
        task(
            "tool_error",
            "read",
            json!({"path": "a.rs", "symbol": "no_such_symbol"}),
            &["a.rs:4".to_string()],
            json!({"grep": "git grep -n 'fn alpha' -- a.rs", "items": "grep_lines"}),
        ),
        task(
            "explain",
            "explain",
            json!({"target": "alpha"}),
            &["a.rs:4".to_string(), "c.rs:2".to_string(), "c.rs:3".to_string()],
            json!({"grep": ["git grep -n -E 'fn [a-z]' -- a.rs", "git grep -n -w -F -e alpha -- '*.rs'",
                            "sed -n '4,6p' a.rs"], "items": "explain"}),
        ),
    ];
    let report = f.dir.path().join("report.json");
    let rows = eval(f.dir.path(), &lines, &report);
    let row = |id: &str| {
        rows.iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("no row {id}"))
    };

    assert_eq!(row("answered").get("baseline_failed"), None, "{}", row("answered"));
    for id in ["silent", "broken"] {
        let r = row(id);
        assert_eq!(r["baseline_failed"], true, "{r}");
        assert_eq!(
            r["baseline_score"]["recall"], 0.0,
            "a failed baseline scores recall 0: {r}"
        );
        assert_eq!(r["score"]["recall"], 1.0, "{r}");
    }
    assert_eq!(row("silent")["savings"]["baseline_tokens"], 0);
    assert!(row("broken")["baseline_error"].is_string(), "{}", row("broken"));

    let err = row("tool_error");
    assert_eq!(err["ok"], false, "{err}");
    assert_eq!(
        err["baseline_score"]["recall"], 1.0,
        "the baseline ran although the tool failed: {err}"
    );
    assert_eq!(err["savings"]["credited"], false, "{err}");

    assert_eq!(
        row("explain")["baseline_score"]["recall"],
        1.0,
        "definition and call sites read from the rows: {}",
        row("explain")
    );

    let rep: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    let refs = &rep["gate"]["references"];
    assert_eq!(
        (refs["tool"].as_str(), refs["job"].as_str()),
        (Some("code_refs"), Some("locating")),
        "{refs}"
    );
    assert_eq!(refs["baseline_failed"], 2, "{refs}");
    assert_eq!(refs["credited"], 3, "{refs}");
    let mean = refs["baseline_recall"].as_f64().unwrap();
    assert!((mean - 1.0 / 3.0).abs() < 1e-9, "two of three baselines failed: {refs}");
    assert_eq!(rep["gate"]["explain"]["job"], "reducing");
    assert!(rep["build"]["build_id"].is_string(), "{}", rep["build"]);
    let features: Vec<&str> = rep["build"]["features"]
        .as_array()
        .unwrap_or_else(|| panic!("{}", rep["build"]))
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let code_intel = cfg!(all(feature = "code-intel-js", feature = "code-intel-stack"));
    assert_eq!(
        features.contains(&"code-intel"),
        code_intel,
        "a `code-intel` build names the feature it was built with: {features:?}"
    );
    if code_intel {
        assert!(
            !features.iter().any(|f| f.starts_with("code-intel-")),
            "the umbrella stands for its parts: {features:?}"
        );
    }
}
