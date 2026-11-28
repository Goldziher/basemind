//! `code_migration_check` over a scanned Rust crate: every pattern kind, include / exclude, the
//! presence assertion, mixed files, read ranges and the progress history across runs.

use std::sync::Arc;

use super::*;

const APP: &str = "use old_log::Logger;
use tracing::span;

pub fn run() {
    old_log::info(\"a\");
    helper(\"b\");
}

pub fn more() {
    old_log::info(\"c\"); old_log::info(\"d\");
    tracing::info(\"e\");
}
";

const RULES: &str = r#"description = "old_log to tracing"

[[rule]]
name = "calls"
guidance = "Call tracing::info instead"
include = ["src/**"]
exclude = ["src/legacy/**"]
match = [{ call = "old_log::info" }, { text = "old_log::info(" }]
expect = [{ call = "tracing::info" }]

[[rule]]
name = "imports"
match = [{ import = "old_log" }]
expect = [{ import = "tracing" }]

[[rule]]
name = "presence"
match = [{ text = "never_present_anywhere" }]
expect = [{ identifier = "does_not_exist" }]

[[rule]]
name = "nowhere"
include = ["nowhere/**"]
match = [{ text = "old_log", regex = true }]
"#;

fn write(root: &std::path::Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, body).expect("write");
}

fn scanned_workspace(root: &std::path::Path) -> Workspace {
    crate::store::init_isolated_cache();
    let store = crate::store::Store::open(root, crate::store::VIEW_WORKING).expect("store");
    let ws = Workspace::new(
        store,
        root.to_path_buf(),
        Arc::new(crate::config::Config::with_defaults()),
        None,
        Arc::new(crate::git_cache::GitCache::open(root, 1, false).expect("git cache")),
        None,
        crate::engine::workspace::OpenOptions {
            background: false,
            read_only: false,
            forwards_writes: false,
            lazy_cache: false,
            history_sync: None,
            #[cfg(feature = "memory")]
            memory_owner: |_| "owner".to_string(),
        },
    );
    ws.rescan(None, crate::scanner::EmbedMode::Deferred, false)
        .expect("scan");
    ws
}

fn fixture() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write(root, "src/app.rs", APP);
    write(
        root,
        "src/legacy/keep.rs",
        "pub fn x() {\n    old_log::info(\"x\");\n}\n",
    );
    write(root, "src/new.rs", "pub fn y() {\n    tracing::info(\"y\");\n}\n");
    write(root, ".basemind/migrations/logging.toml", RULES);
    write(root, "basemind.toml", "");
    let ws = scanned_workspace(root);
    (dir, ws)
}

fn check(ws: &Workspace, ranges: bool) -> MigrationReport {
    let mut response = ws
        .code_migration_check(MigrationCheckParams {
            name: Some("logging".to_string()),
            ranges: Some(ranges),
        })
        .expect("check");
    assert_eq!(response.migrations.len(), 1);
    response.migrations.remove(0)
}

fn rule<'r>(report: &'r MigrationReport, name: &str) -> &'r RuleReport {
    report.rules.iter().find(|r| r.name == name).expect("rule reported")
}

fn paths(files: &[FileUses]) -> Vec<String> {
    files.iter().map(|f| f.path.to_string()).collect()
}

#[test]
fn call_and_text_patterns_count_each_line_once_within_the_selected_files() {
    let (_dir, ws) = fixture();
    let report = check(&ws, true);
    let calls = rule(&report, "calls");
    assert_eq!(
        calls.files_checked, 2,
        "src/app.rs and src/new.rs, not src/legacy/keep.rs"
    );
    assert_eq!(paths(&calls.files), ["src/app.rs"]);
    assert_eq!(calls.files[0].lines, [5, 10], "two calls on line 10 count once");
    assert_eq!(calls.remaining, 2);
    assert_eq!(calls.remaining_files, 1);
    assert_eq!(calls.expected, Some(2), "src/app.rs:11 and src/new.rs:2");
    assert_eq!(calls.expected_files, Some(2));
    assert_eq!(calls.migrated_share, Some(0.5));
    assert!(!calls.failed);
    assert_eq!(
        calls.mixed_files.iter().map(ToString::to_string).collect::<Vec<_>>(),
        ["src/app.rs"]
    );
    assert_eq!(calls.files[0].ranges.as_deref(), Some("1-12"));
    assert_eq!(calls.guidance.as_deref(), Some("Call tracing::info instead"));
}

#[test]
fn import_patterns_read_the_outline() {
    let (_dir, ws) = fixture();
    let report = check(&ws, false);
    let imports = rule(&report, "imports");
    assert_eq!(paths(&imports.files), ["src/app.rs"]);
    assert_eq!(imports.files[0].lines, [1]);
    assert_eq!(imports.expected, Some(1));
    assert!(imports.files[0].ranges.is_none(), "no ranges unless asked");
}

#[test]
fn a_missing_expect_fails_the_rule_and_an_unselecting_rule_checks_nothing() {
    let (_dir, ws) = fixture();
    let report = check(&ws, false);
    let presence = rule(&report, "presence");
    assert!(presence.failed);
    assert_eq!(presence.remaining, 0);
    assert_eq!(presence.expected, Some(0));
    let nowhere = rule(&report, "nowhere");
    assert_eq!(nowhere.files_checked, 0);
    assert_eq!(nowhere.expected, None);
    assert!(!nowhere.failed);
    assert_eq!(report.failed_rules, 1);
    assert_eq!(report.remaining, 3, "two call lines and one import line");
    assert_eq!(report.description.as_deref(), Some("old_log to tracing"));
}

#[test]
fn each_run_is_recorded_and_reported_against_the_ones_before() {
    let (dir, ws) = fixture();
    let first = check(&ws, false);
    assert_eq!(first.progress.runs, 1);
    assert!(first.progress.first.is_none() && first.progress.previous.is_none());
    let history = first.progress.history_file.clone().expect("history written");
    assert!(
        !std::path::Path::new(&history).starts_with(dir.path()),
        "the history lives in the cache, not the repository: {history}"
    );

    write(
        dir.path(),
        "src/app.rs",
        &APP.replace("old_log::info(\"a\")", "tracing::info(\"a\")"),
    );
    ws.rescan(None, crate::scanner::EmbedMode::Deferred, false)
        .expect("rescan");
    let second = check(&ws, false);
    assert_eq!(second.progress.runs, 2);
    assert_eq!(second.progress.first.as_ref().map(|run| run.remaining), Some(3));
    assert_eq!(second.progress.previous.as_ref().map(|run| run.remaining), Some(3));
    assert_eq!(second.remaining, 2, "one call line migrated");
    assert_eq!(
        second
            .progress
            .recent
            .iter()
            .map(|run| run.remaining)
            .collect::<Vec<_>>(),
        [3, 2]
    );
}

#[test]
fn every_rule_file_is_checked_without_a_name() {
    let (dir, ws) = fixture();
    write(
        dir.path(),
        ".basemind/migrations/another.toml",
        "[[rule]]\nname = \"r\"\nmatch = [{ identifier = \"run\" }]\n",
    );
    let response = ws
        .code_migration_check(MigrationCheckParams {
            name: None,
            ranges: None,
        })
        .expect("check");
    let names: Vec<&str> = response.migrations.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["another", "logging"]);
    assert_eq!(
        response.migrations[0].rules[0].files[0].lines,
        [4],
        "the definition of `run`"
    );
}
