//! End-to-end smoke tests for `basemind init` — the re-runnable onboarding flow.
//!
//! These shell the built binary (`CARGO_BIN_EXE_basemind`) against a tempdir and assert the
//! observable filesystem effects: the `basemind.toml` scaffold, the `.gitignore` entry, and the
//! idempotent delimited rules block injected into CLAUDE.md / AGENTS.md / an ai-rulez rule file.

use std::path::Path;
use std::process::Command;

const BEGIN_MARKER: &str = "<!-- BEGIN basemind (managed by `basemind init`) -->";
const END_MARKER: &str = "<!-- END basemind -->";

fn tmpdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create tempdir")
}

/// Run `basemind --root <root> init <extra args...>` and assert success.
fn run_init(root: &Path, extra: &[&str]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_basemind"));
    cmd.arg("--root").arg(root).arg("init");
    for a in extra {
        cmd.arg(a);
    }
    let output = cmd.output().expect("spawn basemind init");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "init failed: status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    format!("{stdout}{stderr}")
}

fn count_markers(haystack: &str) -> usize {
    haystack.matches(BEGIN_MARKER).count()
}

#[test]
fn fresh_dir_writes_config_gitignore_and_claude_block() {
    let dir = tmpdir();
    let root = dir.path();
    run_init(root, &["--yes"]);

    let config = root.join("basemind.toml");
    assert!(config.exists(), "basemind.toml should be written");
    let config_text = std::fs::read_to_string(&config).expect("read config");
    assert!(config_text.contains("[scan]"), "scaffold content present");

    let gitignore = std::fs::read_to_string(root.join(".gitignore")).expect("read .gitignore");
    assert!(
        gitignore.lines().any(|l| l.trim().trim_matches('/') == ".basemind"),
        ".basemind should be gitignored, got:\n{gitignore}"
    );

    let claude = std::fs::read_to_string(root.join("CLAUDE.md")).expect("CLAUDE.md created");
    assert_eq!(count_markers(&claude), 1, "exactly one managed block");
    assert!(claude.contains(END_MARKER), "END marker present");
    assert!(claude.contains("basemind"), "block advertises basemind usage");
}

#[test]
fn init_is_idempotent_single_block_and_print_shows_no_change() {
    let dir = tmpdir();
    let root = dir.path();
    run_init(root, &["--yes"]);
    run_init(root, &["--yes"]);

    let claude = std::fs::read_to_string(root.join("CLAUDE.md")).expect("read CLAUDE.md");
    assert_eq!(
        count_markers(&claude),
        1,
        "second run must not duplicate the block:\n{claude}"
    );

    // ~keep A --print dry-run after convergence must report no pending changes.
    let out = run_init(root, &["--yes", "--print"]);
    let lower = out.to_lowercase();
    assert!(
        lower.contains("no change") || lower.contains("up to date") || lower.contains("up-to-date"),
        "--print should report no pending changes, got:\n{out}"
    );
}

#[test]
fn existing_claude_content_is_preserved_verbatim() {
    let dir = tmpdir();
    let root = dir.path();
    let handwritten = "# My Project\n\nSome hand-written guidance.\n\nDo not delete me.\n";
    std::fs::write(root.join("CLAUDE.md"), handwritten).expect("seed CLAUDE.md");

    run_init(root, &["--yes"]);

    let claude = std::fs::read_to_string(root.join("CLAUDE.md")).expect("read CLAUDE.md");
    assert!(
        claude.contains(handwritten.trim_end()),
        "pre-existing content must survive verbatim:\n{claude}"
    );
    assert_eq!(count_markers(&claude), 1, "block appended once");
    // ~keep The managed block must come AFTER the user content (appended at EOF).
    let user_idx = claude.find("Do not delete me.").expect("user content present");
    let block_idx = claude.find(BEGIN_MARKER).expect("block present");
    assert!(block_idx > user_idx, "block appended after user content");
}

#[test]
fn ai_rulez_present_writes_rule_file_and_leaves_claude_untouched() {
    let dir = tmpdir();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".ai-rulez")).expect("mkdir .ai-rulez");
    std::fs::write(root.join(".ai-rulez/config.toml"), "version = \"4.0\"\n").expect("seed ai-rulez config");
    // ~keep A CLAUDE.md that is a generated artifact must NOT be edited when ai-rulez owns the rules.
    std::fs::write(root.join("CLAUDE.md"), "# generated\n").expect("seed CLAUDE.md");

    run_init(root, &["--yes"]);

    let rule = root.join(".ai-rulez/rules/basemind-usage.md");
    assert!(rule.exists(), "ai-rulez rule file should be written");
    let rule_text = std::fs::read_to_string(&rule).expect("read rule");
    assert!(rule_text.contains("basemind"), "rule advertises basemind");

    let claude = std::fs::read_to_string(root.join("CLAUDE.md")).expect("read CLAUDE.md");
    assert!(
        !claude.contains(BEGIN_MARKER),
        "ai-rulez path must NOT inject into CLAUDE.md:\n{claude}"
    );
}

#[test]
fn rules_target_none_touches_no_rules_file() {
    let dir = tmpdir();
    let root = dir.path();
    run_init(root, &["--yes", "--rules-target", "none"]);

    assert!(root.join("basemind.toml").exists(), "config still written");
    assert!(!root.join("CLAUDE.md").exists(), "no CLAUDE.md created");
    assert!(!root.join("AGENTS.md").exists(), "no AGENTS.md created");
    assert!(
        !root.join(".ai-rulez/rules/basemind-usage.md").exists(),
        "no ai-rulez rule created"
    );
}

#[test]
fn no_rules_flag_touches_no_rules_file() {
    let dir = tmpdir();
    let root = dir.path();
    run_init(root, &["--yes", "--no-rules"]);

    assert!(root.join("basemind.toml").exists(), "config still written");
    assert!(!root.join("CLAUDE.md").exists(), "no CLAUDE.md created");
}

#[test]
fn init_refuses_to_corrupt_a_file_with_a_broken_marker() {
    let dir = tmpdir();
    let root = dir.path();
    // ~keep A CLAUDE.md with a BEGIN marker but no END (e.g. a hand-edit or bad merge dropped the END line).
    // ~keep init must bail rather than append a second block and later collapse the intervening user content.
    let broken = format!("# My Project\n\nkeep me\n\n{BEGIN_MARKER}\nstale rules\n\ntrailing user content\n");
    std::fs::write(root.join("CLAUDE.md"), &broken).expect("seed broken CLAUDE.md");

    let output = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .arg("--root")
        .arg(root)
        .arg("init")
        .arg("--yes")
        .output()
        .expect("spawn basemind init");
    assert!(!output.status.success(), "init must fail on a malformed marker");

    let after = std::fs::read_to_string(root.join("CLAUDE.md")).expect("read CLAUDE.md");
    assert_eq!(after, broken, "the file must be left byte-for-byte untouched on bail");
}

#[test]
fn existing_config_is_kept_not_clobbered() {
    let dir = tmpdir();
    let root = dir.path();
    let sentinel = "# my custom config\n\"$schema\" = \"v1\"\n";
    std::fs::write(root.join("basemind.toml"), sentinel).expect("seed config");

    run_init(root, &["--yes", "--no-rules"]);

    let config = std::fs::read_to_string(root.join("basemind.toml")).expect("read config");
    assert_eq!(config, sentinel, "existing config must be kept verbatim");
}
