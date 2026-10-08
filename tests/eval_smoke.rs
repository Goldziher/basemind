//! End-to-end tests for `basemind admin eval`: a tiny throwaway git repo with known structure is
//! scanned, a task file with hand-computed gold is run through the built binary, and the per-task
//! scores, the aggregate report, the savings crediting and the `--baseline` regression gate are
//! asserted against values worked out by hand.
//!
//! Fixture (1-based lines):
//! - `a.rs`: `alpha` fn (line 1), `Beta` struct (line 2)
//! - `c.rs`: `caller` fn (line 1) which calls `alpha` twice on that same line

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
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

fn fixture() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\npub struct Beta { x: i32 }\n").unwrap();
    std::fs::write(root.join("c.rs"), b"pub fn caller() { alpha(); alpha(); }\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    let status = Command::new(bin())
        .args(["--root", root.to_str().unwrap(), "scan", "--quiet"])
        .status()
        .expect("run basemind scan");
    assert!(status.success(), "basemind scan failed");
    dir
}

fn eval(root: &Path, tasks: &Path, extra: &[&str]) -> Output {
    Command::new(bin())
        .args([
            "--root",
            root.to_str().unwrap(),
            "admin",
            "eval",
            "--tasks",
            tasks.to_str().unwrap(),
        ])
        .args(extra)
        .output()
        .expect("run basemind admin eval")
}

fn write_tasks(dir: &Path, name: &str, lines: &[&str]) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, lines.join("\n")).unwrap();
    p
}

fn rows(out: &Output) -> Vec<Value> {
    assert!(
        out.status.success(),
        "eval failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not NDJSON `{l}`: {e}")))
        .collect()
}

fn f(v: &Value, path: &str) -> f64 {
    path.split('.')
        .fold(v, |acc, k| &acc[k])
        .as_f64()
        .unwrap_or_else(|| panic!("no number at {path} in {v}"))
}

const PERFECT: &str = r#"{"id":"perfect","mode":"symbols","args":{"name":"alpha"},"gold":["a.rs:1"]}"#;
// grep "alpha" matches a.rs:1 and c.rs:1; gold names three files, one of which does not exist.
const PARTIAL_RECALL: &str =
    r#"{"id":"recall","mode":"grep","args":{"pattern":"alpha"},"gold":["a.rs","c.rs","z.rs"]}"#;
// Same answer, but gold only wants a.rs: half of what came back is wrong.
const PARTIAL_PRECISION: &str = r#"{"id":"precision","mode":"grep","args":{"pattern":"alpha"},"gold":["a.rs"]}"#;
const EMPTY_OK: &str = r#"{"id":"empty_ok","mode":"symbols","args":{"name":"nosuchsymbolzzz"},"gold":[]}"#;
const EMPTY_MISS: &str = r#"{"id":"empty_miss","mode":"symbols","args":{"name":"nosuchsymbolzzz"},"gold":["a.rs:1"]}"#;
const RANKED_HIT: &str =
    r#"{"id":"ranked_hit","mode":"find","args":{"query":"c.rs"},"gold":["c.rs"],"scoring":"ranked","k":5}"#;
const RANKED_MISS: &str =
    r#"{"id":"ranked_miss","mode":"find","args":{"query":"c.rs"},"gold":["nope.rs"],"scoring":"ranked","k":5}"#;
const LINE_LEVEL: &str = r#"{"id":"lines","mode":"references","args":{"name":"alpha"},"gold":["c.rs:1"]}"#;

#[test]
fn known_gold_yields_known_scores() {
    let repo = fixture();
    let tasks = write_tasks(
        repo.path(),
        "t.jsonl",
        &[
            PERFECT,
            PARTIAL_RECALL,
            PARTIAL_PRECISION,
            EMPTY_OK,
            EMPTY_MISS,
            RANKED_HIT,
            RANKED_MISS,
            LINE_LEVEL,
        ],
    );
    let rows = rows(&eval(repo.path(), &tasks, &[]));
    let by_id = |id: &str| {
        rows.iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("no row {id}"))
    };
    for r in &rows {
        assert_eq!(r["ok"], true, "task failed: {r}");
    }

    let p = by_id("perfect");
    assert_eq!(
        (f(p, "score.precision"), f(p, "score.recall"), f(p, "score.f1")),
        (1.0, 1.0, 1.0)
    );
    assert!(f(p, "tokens") > 0.0 && f(p, "elapsed_us") > 0.0);

    let r = by_id("recall");
    assert_eq!(f(r, "score.precision"), 1.0);
    assert!((f(r, "score.recall") - 2.0 / 3.0).abs() < 1e-9);

    let q = by_id("precision");
    assert_eq!(f(q, "score.precision"), 0.5);
    assert_eq!(f(q, "score.recall"), 1.0);
    assert!((f(q, "score.f1") - 2.0 / 3.0).abs() < 1e-9);

    assert_eq!(
        f(by_id("empty_ok"), "score.f1"),
        1.0,
        "right to return nothing when gold is empty"
    );
    let miss = by_id("empty_miss");
    assert_eq!((f(miss, "score.recall"), f(miss, "score.f1")), (0.0, 0.0));
    assert_eq!(f(miss, "score.returned"), 0.0);

    let hit = by_id("ranked_hit");
    assert_eq!(
        (f(hit, "score.hit_at_1"), f(hit, "score.mrr"), f(hit, "score.ndcg")),
        (1.0, 1.0, 1.0)
    );
    let none = by_id("ranked_miss");
    assert_eq!(
        (f(none, "score.hit_at_5"), f(none, "score.mrr"), f(none, "score.ndcg")),
        (0.0, 0.0, 0.0)
    );
    assert!(
        by_id("perfect")["score"].get("mrr").is_none(),
        "set tasks carry no rank metrics"
    );

    // Three call-site hits collapse by (path, line): both calls sit on c.rs:1, so one item.
    let lines = by_id("lines");
    assert_eq!(
        (f(lines, "score.precision"), f(lines, "score.recall")),
        (1.0, 1.0),
        "{lines}"
    );
}

#[test]
fn report_aggregates_per_mode_and_markdown_is_written() {
    let repo = fixture();
    let tasks = write_tasks(repo.path(), "t.jsonl", &[PERFECT, EMPTY_MISS, RANKED_HIT, RANKED_MISS]);
    let report = repo.path().join("report.json");
    let md = repo.path().join("report.md");
    let out = eval(
        repo.path(),
        &tasks,
        &["--report", report.to_str().unwrap(), "--markdown", md.to_str().unwrap()],
    );
    rows(&out);
    let rep: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    let sym = &rep["modes"]["symbols"];
    assert_eq!(sym["tasks"], 2);
    assert_eq!(f(sym, "mean_f1"), 0.5, "one perfect + one miss");
    assert!(sym["mrr"].is_null(), "symbols tasks are set-scored");
    let find = &rep["modes"]["find"];
    assert_eq!(find["ranked_tasks"], 2);
    assert_eq!(f(find, "mrr"), 0.5);
    assert_eq!(f(find, "hit_at_1"), 0.5);
    assert!(f(find, "latency_p95_us") >= f(find, "latency_p50_us"));
    assert_eq!(rep["overall"]["tasks"], 4);
    let text = std::fs::read_to_string(&md).unwrap();
    assert!(
        text.contains("| symbols | 2 |") && text.contains("| find | 2 |"),
        "{text}"
    );
}

#[test]
fn savings_are_credited_only_for_correct_answers() {
    let repo = fixture();
    // Word-like text, so both the o200k tokenizer and its offline word-count fallback see many tokens.
    let big = "lorem ipsum dolor ".repeat(4000);
    std::fs::write(repo.path().join("big.txt"), big).unwrap();
    let good =
        r#"{"id":"good","mode":"symbols","args":{"name":"alpha"},"gold":["a.rs:1"],"baseline":{"read":["big.txt"]}}"#;
    let wrong = r#"{"id":"wrong","mode":"symbols","args":{"name":"alpha"},"gold":["zzz.rs:9"],"baseline":{"read":["big.txt"]}}"#;
    let shell = r#"{"id":"shell","mode":"grep","args":{"pattern":"alpha"},"gold":["a.rs","c.rs"],"baseline":{"grep":"git grep -n alpha"}}"#;
    let tasks = write_tasks(repo.path(), "t.jsonl", &[good, wrong, shell]);
    let report = repo.path().join("report.json");
    let rows = rows(&eval(repo.path(), &tasks, &["--report", report.to_str().unwrap()]));

    let by_id = |id: &str| rows.iter().find(|r| r["id"] == id).unwrap();
    assert_eq!(by_id("good")["savings"]["credited"], true);
    assert!(f(by_id("good"), "savings.baseline_tokens") > f(by_id("good"), "tokens"));
    assert_eq!(
        by_id("wrong")["savings"]["credited"],
        false,
        "recall 0 must withhold savings"
    );
    assert!(
        f(by_id("shell"), "savings.baseline_tokens") > 0.0,
        "git grep output is counted"
    );

    let rep: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    let sv = &rep["modes"]["symbols"]["savings"];
    assert_eq!((sv["credited"].as_u64(), sv["uncredited"].as_u64()), (Some(1), Some(1)));
    assert_eq!(f(sv, "model_ratio"), 25.0, "savings.rs models code:symbols at 25x");
    assert!(f(sv, "ratio.median") > 25.0 * 1.25 && sv["flagged"] == true, "{sv}");
}

#[test]
fn baseline_report_gates_regressions() {
    let repo = fixture();
    let good = write_tasks(repo.path(), "good.jsonl", &[PERFECT, RANKED_HIT]);
    let report = repo.path().join("base.json");
    rows(&eval(repo.path(), &good, &["--report", report.to_str().unwrap()]));

    // Identical tasks: no quality change. Cost tolerance is widened so timing noise cannot fail it.
    let same = eval(
        repo.path(),
        &good,
        &["--baseline", report.to_str().unwrap(), "--cost-tolerance", "1000000"],
    );
    assert!(same.status.success(), "{}", String::from_utf8_lossy(&same.stderr));

    // Same ids and modes but the gold is now wrong: F1 and MRR collapse.
    let bad = write_tasks(
        repo.path(),
        "bad.jsonl",
        &[
            r#"{"id":"perfect","mode":"symbols","args":{"name":"alpha"},"gold":["b.rs:1"]}"#,
            r#"{"id":"ranked_hit","mode":"find","args":{"query":"c.rs"},"gold":["nope.rs"],"scoring":"ranked"}"#,
        ],
    );
    let regressed = eval(
        repo.path(),
        &bad,
        &["--baseline", report.to_str().unwrap(), "--cost-tolerance", "1000000"],
    );
    assert!(!regressed.status.success(), "a regression must exit non-zero");
    let err = String::from_utf8_lossy(&regressed.stderr);
    assert!(err.contains("REGRESSION symbols mean_f1"), "{err}");
    assert!(err.contains("REGRESSION find mrr"), "{err}");

    // A loose enough tolerance accepts the same drop.
    let tolerated = eval(
        repo.path(),
        &bad,
        &[
            "--baseline",
            report.to_str().unwrap(),
            "--tolerance",
            "1.0",
            "--cost-tolerance",
            "1000000",
        ],
    );
    assert!(
        tolerated.status.success(),
        "{}",
        String::from_utf8_lossy(&tolerated.stderr)
    );
}

#[test]
fn bad_task_files_fail_loudly() {
    let repo = fixture();
    let dup = write_tasks(repo.path(), "dup.jsonl", &[PERFECT, PERFECT]);
    let out = eval(repo.path(), &dup, &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("duplicate task id"));

    let unknown = write_tasks(repo.path(), "u.jsonl", &[r#"{"id":"x","mode":"teleport"}"#]);
    assert!(!eval(repo.path(), &unknown, &[]).status.success());

    // A task whose args the tool rejects is reported as a failed task, not silently scored as a pass.
    let badargs = write_tasks(
        repo.path(),
        "b.jsonl",
        &[r#"{"id":"x","mode":"symbols","args":{"bogus_field":1},"gold":[]}"#],
    );
    let out = eval(repo.path(), &badargs, &[]);
    let row = &rows(&out)[0];
    assert_eq!(row["ok"], false, "{row}");
}
