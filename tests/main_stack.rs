//! The binary must run on Windows' 1 MiB main-thread stack. Linux and macOS give the main thread
//! 8 MiB, so an oversized frame on it (the debug-build clap parser alone needs about 1 MiB) passes
//! every Unix leg and aborts every Windows one with "thread 'main' has overflowed its stack". These
//! tests start the built binary with its stack limit lowered to 1 MiB, which sizes the main thread of
//! the exec'd process the way Windows does, so the overflow fails here first.
//! Hermetic via `init_isolated_cache`: the children inherit its data home and comms directory.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

/// Windows' default main-thread stack reservation, in KiB as `ulimit -s` takes it.
const WINDOWS_MAIN_STACK_KIB: &str = "1024";

/// The limit is set by a shell that then execs the program: on macOS, `setrlimit(RLIMIT_STACK)` in a
/// `pre_exec` hook of a child forked from a test thread is refused with EINVAL.
const LIMIT_THEN_EXEC: &str = "ulimit -s \"$BASEMIND_TEST_STACK_KIB\" && exec \"$@\"";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
}

/// A command running `program args…` with its main-thread stack limited to Windows' default.
fn small_stack(program: &str, args: &[&str]) -> Command {
    let mut command = Command::new("sh");
    command
        .args(["-c", LIMIT_THEN_EXEC, "sh", program])
        .args(args)
        .env("BASEMIND_TEST_STACK_KIB", WINDOWS_MAIN_STACK_KIB);
    command
}

fn git(repo: &Path, args: &[&str]) {
    let status = basemind::testing::git_command()
        .args(args)
        .current_dir(repo)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn repo() -> tempfile::TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\npub fn caller() { alpha(); }\n").expect("write a.rs");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    dir
}

#[test]
fn the_child_really_runs_on_a_one_mib_stack() {
    // Proves the limit reaches the exec'd program: a shell started the same way reports it.
    let out = small_stack("sh", &["-c", "ulimit -s"]).output().expect("run sh");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), WINDOWS_MAIN_STACK_KIB);
}

#[test]
fn scan_and_a_query_run_on_a_windows_sized_main_stack() {
    let dir = repo();
    let root = dir.path().to_str().expect("utf-8 temp path");
    for args in [
        vec!["--version"],
        vec!["--root", root, "scan"],
        vec!["--root", root, "code", "outline", "a.rs"],
    ] {
        let out = small_stack(bin(), &args).output().expect("run basemind");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "`basemind {}` failed on a 1 MiB main stack ({:?}):\n{stderr}",
            args.join(" "),
            out.status
        );
        assert!(!stderr.contains("overflowed its stack"), "{stderr}");
    }
}

#[cfg(feature = "comms")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_answers_initialize_on_a_windows_sized_main_stack() {
    use rmcp::ServiceExt;
    use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};

    let dir = repo();
    let root = dir.path().to_path_buf();
    // The daemon `serve` starts inherits the limit too.
    let cmd = tokio::process::Command::from(small_stack(bin(), &["serve"])).configure(move |c| {
        c.arg("--root").arg(&root);
        c.env("BASEMIND_HOST", "smoke");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let service = ().serve(transport).await.expect("initialize over a 1 MiB main stack");
    let tools = service.list_all_tools().await.expect("list tools");
    assert!(tools.iter().any(|t| t.name == "code_outline"), "code_outline is listed");
    service.cancel().await.expect("close the session");
}
