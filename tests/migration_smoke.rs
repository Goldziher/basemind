//! `basemind code migration check` (#130) through the real CLI, over `tests/fixtures/migration`:
//! the report, the exit codes of `--fail-on-remaining` (an old form left, a presence assertion
//! failed, all clear), the refusal of a bad rule file, and the progress history kept in the cache.
//!
//! Every child runs on the isolated data home and a private, empty `BASEMIND_COMMS_DIR`, so no
//! daemon answers and the in-process engine does.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/migration")
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            std::fs::create_dir_all(&target).expect("mkdir");
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// The fixture copied into a fresh directory (marked as a basemind root) and scanned.
struct Repo {
    dir: tempfile::TempDir,
    comms: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        basemind::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        copy_tree(&fixture_dir(), dir.path());
        std::fs::write(dir.path().join("basemind.toml"), "\"$schema\" = \"v1\"\n").expect("basemind.toml");
        let repo = Self {
            dir,
            comms: tempfile::tempdir().expect("comms dir"),
        };
        let scan = repo.run(&["scan"]);
        assert!(scan.status.success(), "scan: {}", String::from_utf8_lossy(&scan.stderr));
        repo
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_basemind"))
            .arg("--root")
            .arg(self.root())
            .args(args)
            .env("BASEMIND_COMMS_DIR", self.comms.path())
            .output()
            .expect("run basemind")
    }

    fn check(&self, extra: &[&str]) -> (Option<i32>, Value, String) {
        let mut args = vec!["--json", "code", "migration", "check"];
        args.extend_from_slice(extra);
        let out = self.run(&args);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let report = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        (out.status.code(), report, stderr)
    }

    fn write(&self, rel: &str, body: &str) {
        std::fs::write(self.root().join(rel), body).expect("write");
    }
}

fn rule<'r>(migration: &'r Value, name: &str) -> &'r Value {
    migration["rules"]
        .as_array()
        .and_then(|rules| rules.iter().find(|rule| rule["name"] == name))
        .unwrap_or_else(|| panic!("rule `{name}` in {migration}"))
}

#[test]
fn the_report_counts_old_and_new_forms_per_rule_and_file() {
    let repo = Repo::new();
    let (code, report, stderr) = repo.check(&["logging"]);
    assert_eq!(code, Some(0), "{stderr}");
    let migration = &report["migrations"][0];
    assert_eq!(migration["name"], "logging");

    let calls = rule(migration, "calls");
    assert_eq!(calls["files_checked"], 2, "src/legacy is excluded: {calls}");
    assert_eq!(calls["remaining"], 2, "lines 5 and 10 of src/app.rs: {calls}");
    assert_eq!(calls["files"][0]["path"], "src/app.rs");
    assert_eq!(calls["files"][0]["lines"], serde_json::json!([5, 10]));
    assert_eq!(calls["expected"], 2, "{calls}");
    assert_eq!(calls["mixed_files"], serde_json::json!(["src/app.rs"]));
    assert_eq!(calls["migrated_share"], 0.5);

    let imports = rule(migration, "imports");
    assert_eq!(imports["remaining"], 1, "{imports}");
    assert_eq!(imports["files"][0]["lines"], serde_json::json!([1]));

    assert_eq!(migration["remaining"], 3);
    assert_eq!(migration["failed_rules"], 0);
}

#[test]
fn fail_on_remaining_exits_1_with_the_report_printed() {
    let repo = Repo::new();
    let (code, report, stderr) = repo.check(&["logging", "--fail-on-remaining"]);
    assert_eq!(code, Some(1), "{stderr}");
    assert_eq!(report["migrations"][0]["remaining"], 3, "the report is still printed");
    assert!(stderr.contains("3 line(s) still hold an old form"), "{stderr}");
}

#[test]
fn a_presence_assertion_fails_the_gate_and_a_clean_migration_passes_it() {
    let repo = Repo::new();
    repo.write(
        ".basemind/migrations/present.toml",
        "[[rule]]\nname = \"r\"\nmatch = [{ text = \"never_present_anywhere\" }]\nexpect = [{ identifier = \"no_such_fn\" }]\n",
    );
    let (code, report, stderr) = repo.check(&["present", "--fail-on-remaining"]);
    assert_eq!(code, Some(1), "{stderr}");
    assert_eq!(report["migrations"][0]["remaining"], 0);
    assert_eq!(report["migrations"][0]["rules"][0]["failed"], true);

    repo.write(
        ".basemind/migrations/done.toml",
        "[[rule]]\nname = \"r\"\nmatch = [{ text = \"never_present_anywhere\" }]\nexpect = [{ identifier = \"run\" }]\n",
    );
    let (code, report, stderr) = repo.check(&["done", "--fail-on-remaining"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(report["migrations"][0]["rules"][0]["expected"], 1);
}

#[test]
fn a_bad_rule_file_is_a_usage_error() {
    let repo = Repo::new();
    repo.write(
        ".basemind/migrations/bad.toml",
        "[[rule]]\nname = \"r\"\ninclude = [\"../**\"]\nmatch = [{ call = \"x\" }]\n",
    );
    let (code, _, stderr) = repo.check(&["bad"]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("leaves the repository"), "{stderr}");
    let (code, _, stderr) = repo.check(&["../logging"]);
    assert_eq!(code, Some(2), "{stderr}");
}

#[test]
fn progress_is_recorded_in_the_cache_across_runs() {
    let repo = Repo::new();
    let (_, first, _) = repo.check(&["logging"]);
    let history = first["migrations"][0]["progress"]["history_file"]
        .as_str()
        .expect("history written")
        .to_string();
    assert!(
        !Path::new(&history).starts_with(repo.root()),
        "the history is kept out of the repository: {history}"
    );

    let app = std::fs::read_to_string(repo.root().join("src/app.rs")).expect("read");
    repo.write(
        "src/app.rs",
        &app.replace("old_log::info(\"a\")", "tracing::info(\"a\")"),
    );
    let rescan = repo.run(&["scan"]);
    assert!(rescan.status.success(), "{}", String::from_utf8_lossy(&rescan.stderr));

    let (code, second, stderr) = repo.check(&["logging", "--ranges"]);
    assert_eq!(code, Some(0), "{stderr}");
    let progress = &second["migrations"][0]["progress"];
    assert_eq!(progress["runs"], 2, "{progress}");
    assert_eq!(progress["first"]["remaining"], 3);
    assert_eq!(progress["previous"]["remaining"], 3);
    assert_eq!(second["migrations"][0]["remaining"], 2);
    assert_eq!(rule(&second["migrations"][0], "calls")["files"][0]["ranges"], "5-12");
    assert_eq!(std::fs::read_to_string(&history).expect("history").lines().count(), 2);
}
