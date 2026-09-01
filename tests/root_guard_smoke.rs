//! End-to-end guards for the workspace-root allow-list (issue #62).
//!
//! Root discovery still falls back to the start directory, so an MCP host launched at `/` — or at
//! any directory that is not a project — resolves to a root basemind would otherwise open
//! read-write and walk in full. The allow-list is applied where a root is *consumed*; these tests
//! drive the real binary at those consumption points and assert the refusal is a hard failure that
//! leaves nothing behind.

use std::path::Path;
use std::process::Command;

/// Path to the freshly built `basemind` binary (cargo sets `CARGO_BIN_EXE_<name>`).
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
}

/// Run the binary with the isolated cache this process already installed (the child inherits
/// `BASEMIND_DATA_HOME`), returning `(success, stdout + stderr)`.
fn run(args: &[&str], extra_env: &[(&str, &str)]) -> (bool, String) {
    let mut cmd = Command::new(bin());
    // Null stdin so a regression that lets `serve` through blocks nothing: it would reach the relay
    // pump, see EOF, and exit — a failing assertion rather than a hung test binary.
    cmd.stdin(std::process::Stdio::null());
    cmd.args(args);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("run basemind");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// The working-view index for `root`, the artifact whose absence proves nothing was indexed.
fn working_index(root: &Path) -> std::path::PathBuf {
    basemind::store::workspace_cache_dir(root)
        .join(basemind::store::VIEWS_DIR)
        .join(basemind::store::VIEW_WORKING)
        .join(basemind::store::INDEX_FILE)
}

#[test]
fn scan_refuses_the_filesystem_root_and_names_no_override() {
    basemind::store::init_isolated_cache();
    let (ok, output) = run(&["--root", "/", "scan"], &[]);
    assert!(!ok, "`basemind scan --root /` must fail: {output}");
    assert!(
        output.contains("filesystem/volume root"),
        "the refusal must name what was refused: {output}"
    );
    assert!(
        output.contains("no override"),
        "the filesystem-root refusal must say it cannot be overridden: {output}"
    );
    assert!(
        !working_index(Path::new("/")).exists(),
        "a refused root must not be indexed"
    );
}

#[test]
fn the_env_hatch_never_unlocks_the_filesystem_root() {
    basemind::store::init_isolated_cache();
    let (ok, output) = run(&["--root", "/", "scan"], &[("BASEMIND_ALLOW_ANY_ROOT", "1")]);
    assert!(!ok, "the env hatch must not unlock `/`: {output}");
    assert!(output.contains("filesystem/volume root"), "{output}");
}

#[test]
fn scan_refuses_a_plain_directory_and_the_env_hatch_lets_it_through() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").expect("write source");
    let root_str = root.to_str().expect("utf-8 root");

    let (ok, output) = run(&["--root", root_str, "scan", "--quiet"], &[]);
    assert!(!ok, "a plain non-git directory must be refused: {output}");
    assert!(output.contains("basemind init"), "escape hatch #1 named: {output}");
    assert!(
        output.contains("BASEMIND_ALLOW_ANY_ROOT"),
        "escape hatch #2 named: {output}"
    );
    assert!(!working_index(&root).exists(), "a refused root must not be indexed");

    let (ok, output) = run(
        &["--root", root_str, "scan", "--quiet"],
        &[("BASEMIND_ALLOW_ANY_ROOT", "1")],
    );
    assert!(ok, "the env hatch must let a plain directory through: {output}");
}

/// The severe half of #62: `serve` must refuse before it relays, and — critically — the refusal
/// must NOT degrade into an in-process scan. `cmd_serve` has no in-process fallback
/// (`try_serve_relay` is the only path and `open_relay_connection` bails on a declined welcome), so
/// a refused root exits non-zero with the guidance and writes no index. If a fallback is ever
/// reintroduced, the 43 GiB scan simply relocates from the daemon into the client and this goes red.
#[test]
fn serve_refuses_a_non_project_root_instead_of_scanning_in_process() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").expect("write source");

    let (ok, output) = run(&["--root", root.to_str().expect("utf-8 root"), "serve"], &[]);
    assert!(!ok, "`basemind serve` on a non-project root must fail: {output}");
    assert!(
        output.contains("refusing to use") && output.contains("workspace root"),
        "the refusal reaches the client's own stderr: {output}"
    );
    assert!(
        !working_index(&root).exists(),
        "a refused `serve` must not fall back to an in-process scan"
    );
}
