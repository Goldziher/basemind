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

/// The guard used to be a purely syntactic check on whatever path it was handed, and `std::path`
/// normalizes neither `..` nor symlinks. Every spelling below names `/` while looking like an
/// ordinary directory, and the env hatch is set throughout — the module doc promises nothing
/// overrides the filesystem-root refusal, so this is that promise under test end to end.
#[test]
fn paths_that_resolve_to_the_filesystem_root_are_refused_even_under_the_env_hatch() {
    basemind::store::init_isolated_cache();
    let hatch = [("BASEMIND_ALLOW_ANY_ROOT", "1")];

    for spelling in ["/..", "/usr/..", "/tmp/../.."] {
        let (ok, output) = run(&["--root", spelling, "scan", "--quiet"], &hatch);
        assert!(!ok, "`basemind scan --root {spelling}` must fail: {output}");
        assert!(
            output.contains("filesystem/volume root"),
            "{spelling} resolves to the filesystem root: {output}"
        );
    }

    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let link = dir.path().join("slash");
        std::os::unix::fs::symlink("/", &link).expect("symlink to /");
        let (ok, output) = run(
            &["--root", link.to_str().expect("utf-8 link"), "scan", "--quiet"],
            &hatch,
        );
        assert!(!ok, "a symlink to / must fail: {output}");
        assert!(output.contains("filesystem/volume root"), "{output}");
    }

    assert!(
        !working_index(Path::new("/")).exists(),
        "no spelling of the filesystem root may be indexed"
    );
}

/// `basemind admin rescan` is a full working-tree walk that reaches `crate::scanner::scan`
/// in-process, but the tool-subcommand dispatcher never calls the CLI's root guard — so
/// `cd / && basemind admin rescan` reproduced issue #62 on a verb the guard missed. The guard now
/// sits at `mcp::helpers::scan_and_refresh`, which is also what the `admin` MCP tool funnels through.
#[test]
fn admin_rescan_refuses_a_non_project_root() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").expect("write source");
    let root_str = root.to_str().expect("utf-8 root");

    let (ok, output) = run(&["--root", root_str, "admin", "rescan"], &[]);
    assert!(!ok, "`basemind admin rescan` on a plain directory must fail: {output}");
    assert!(
        output.contains("refusing to use") && output.contains("basemind init"),
        "the operator gets the same guidance as from `scan`: {output}"
    );
    assert!(
        !working_index(&root).exists(),
        "a refused `admin rescan` must not have walked the tree"
    );
}

/// The escape hatch is per-invocation. Before the admission record it was a permanent whitelist: the
/// scan it authorized wrote `views/working/index.msgpack`, and the grandfather clause read that file
/// as proof of consent — so one `BASEMIND_ALLOW_ANY_ROOT=1 basemind scan $HOME` admitted `$HOME`
/// forever, for every consumer, with no env var set.
#[test]
fn using_the_env_hatch_once_does_not_whitelist_the_root_forever() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").expect("write source");
    let root_str = root.to_str().expect("utf-8 root");

    let (ok, output) = run(
        &["--root", root_str, "scan", "--quiet"],
        &[("BASEMIND_ALLOW_ANY_ROOT", "1")],
    );
    assert!(ok, "the hatch lets the scan through: {output}");
    assert!(working_index(&root).exists(), "the hatched scan really did index");

    let (ok, output) = run(&["--root", root_str, "scan", "--quiet"], &[]);
    assert!(
        !ok,
        "unsetting the hatch must re-refuse a root it only ever admitted: {output}"
    );
    assert!(
        output.contains("unset it and this root is refused again"),
        "the refusal states the hatch's scope in plain words: {output}"
    );
}
