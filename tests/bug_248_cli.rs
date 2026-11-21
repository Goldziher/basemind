//! Regression test for bug #248: `basemind hook install --force` renamed a foreign pre-commit hook to
//! `pre-commit.bak` every time, so a second forced install over a different foreign hook replaced the
//! first backup and lost that hook. Every forced install must keep the hook it replaces.

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn force_install(root: &Path) {
    let output = Command::new(bin())
        .args(["--root", root.to_str().unwrap(), "hook", "install", "--force"])
        .output()
        .expect("run basemind hook install --force");
    assert!(
        output.status.success(),
        "hook install --force failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn should_keep_both_foreign_hooks_across_two_forced_installs() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    let hooks = root.join(".git").join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();

    std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\necho first\n").unwrap();
    force_install(root);
    std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\necho second\n").unwrap();
    force_install(root);

    let first = std::fs::read_to_string(hooks.join("pre-commit.bak")).expect("first backup kept");
    let second = std::fs::read_to_string(hooks.join("pre-commit.bak.1")).expect("second backup kept");
    assert!(first.contains("first"), "{first}");
    assert!(second.contains("second"), "{second}");
}
