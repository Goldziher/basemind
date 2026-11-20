//! `basemind::testing::git_command` keeps the host's git configuration out of the git a test runs:
//! a global config that requires signing, points `core.hooksPath` elsewhere and turns on
//! `core.autocrlf`, and config passed down through `GIT_CONFIG_PARAMETERS`, reach a plain `git` but
//! not the helper's.

use std::process::Command;

use basemind::testing::git_command;

/// `key`'s value as `command` reads it in `cwd`, or `None` when it is unset.
fn config_value(mut command: Command, cwd: &std::path::Path, key: &str) -> Option<String> {
    let out = command
        .args(["config", "--get", key])
        .current_dir(cwd)
        .output()
        .expect("git in PATH");
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Deliberately the ONLY test function in this binary: it stands in for a developer's machine by
/// setting the git variables of this process, and `std::env::set_var` is unsound while another
/// thread reads the environment.
#[test]
fn git_command_ignores_a_hostile_host_configuration() {
    let host = tempfile::tempdir().expect("host tempdir");
    let hostile = host.path().join("gitconfig");
    std::fs::write(
        &hostile,
        "[commit]\n\tgpgsign = true\n[core]\n\thooksPath = /nonexistent\n\tautocrlf = true\n",
    )
    .expect("write the hostile global config");
    // SAFETY: the only test fn in this binary, so no other thread reads the environment.
    unsafe {
        std::env::set_var("GIT_CONFIG_GLOBAL", &hostile);
        std::env::set_var("GIT_CONFIG_PARAMETERS", "'init.defaultbranch=hostile'");
    }
    let repo = tempfile::tempdir().expect("repo tempdir");
    let root = repo.path();

    assert_eq!(
        config_value(Command::new("git"), root, "commit.gpgsign"),
        Some("true".to_string()),
        "the hostile global config is live for a plain git"
    );
    assert_eq!(
        config_value(Command::new("git"), root, "init.defaultbranch"),
        Some("hostile".to_string()),
        "the inherited config parameters are live for a plain git"
    );
    for key in [
        "commit.gpgsign",
        "core.hookspath",
        "core.autocrlf",
        "init.defaultbranch",
    ] {
        assert_eq!(
            config_value(git_command(), root, key),
            None,
            "{key} reached git_command"
        );
    }

    std::fs::write(root.join("a.txt"), "line\n").expect("write a.txt");
    for args in [&["init", "-q"][..], &["add", "a.txt"], &["commit", "-qm", "init"]] {
        let status = git_command()
            .args(args)
            .current_dir(root)
            .status()
            .expect("git in PATH");
        assert!(status.success(), "git {args:?} failed under a hostile host config");
    }
}
