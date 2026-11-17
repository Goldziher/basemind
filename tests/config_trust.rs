//! The repo trust gate must be applied by every loader, not just unit-tested in isolation: deleting
//! the `sanitize_repo_config` call from `load` must fail here.

use std::path::Path;
use std::sync::Mutex;

use basemind::config;

static ENV_LOCK: Mutex<()> = Mutex::new(());

const GRANTS: &[&str] = &["BASEMIND_ALLOW_FOLLOW_SYMLINKS"];

const HOSTILE: &str = "\"$schema\" = \"v1\"\n[scan]\nfollow_symlinks = true\n";

fn hostile_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("basemind.toml"), HOSTILE).expect("write config");
    dir
}

fn clear_grants() {
    for name in GRANTS {
        // SAFETY: every test in this binary holds ENV_LOCK.
        unsafe { std::env::remove_var(name) };
    }
}

fn assert_stripped(cfg: &config::Config, via: &str) {
    assert!(!cfg.scan.follow_symlinks, "{via}: follow_symlinks");
}

#[test]
fn every_loader_strips_what_the_repo_was_not_granted() {
    let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_grants();
    let repo = hostile_repo();
    let root: &Path = repo.path();

    assert_stripped(&config::load(root).expect("load"), "load");
    assert_stripped(&config::daemon::load_daemon(root).expect("load_daemon"), "load_daemon");
    let loaded = config::overrides::load_with_overrides(root, None, None).expect("load_with_overrides");
    assert_stripped(&loaded.config, "load_with_overrides");
}

#[test]
fn operator_grants_restore_the_repo_values() {
    let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_grants();
    // SAFETY: ENV_LOCK is held; cleared again below.
    unsafe { std::env::set_var("BASEMIND_ALLOW_FOLLOW_SYMLINKS", "1") };
    let repo = hostile_repo();
    let cfg = config::load(repo.path()).expect("load");
    clear_grants();
    assert!(cfg.scan.follow_symlinks);
}

/// A repository cannot turn agent launching on: an `[agents]` table in its `basemind.toml` is
/// dropped (the operator's own user-level file is the only source), and the rest of the file
/// still loads instead of failing validation on the unknown table.
#[test]
fn an_agents_table_in_the_repo_config_is_dropped_and_the_rest_loads() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("basemind.toml"),
        "\"$schema\" = \"v1\"\n[comms]\nagent_id = \"repo-lead\"\n\n[agents.launch]\nenabled = true\n\
         worktrees_root = \"/tmp\"\n[agents.launch.harness.claude]\nexecutable = \"/tmp/claude\"\n",
    )
    .expect("write config");
    let cfg = config::load(dir.path()).expect("a repo [agents] table must not break the file");
    assert_eq!(cfg.comms.agent_id.as_deref(), Some("repo-lead"));
    let serialized = serde_json::to_value(&cfg).expect("serialize");
    assert!(serialized.get("agents").is_none(), "{serialized}");
}
