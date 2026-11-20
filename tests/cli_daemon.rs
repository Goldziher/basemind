//! The CLI against a live comms daemon, and the exit-code / safety contract of the scan commands.
//!
//! Every child process runs with a throwaway `BASEMIND_DATA_HOME` and `BASEMIND_COMMS_DIR`, so
//! nothing here can touch a developer's real cache or daemon. Exit codes follow
//! `basemind::cli::exit`: 0 ok, 1 error, 2 usage, 3 busy, 4 unavailable, 130 interrupted.

#![cfg(all(feature = "comms", unix))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use basemind::comms::singleton::{comms_socket_path, generation_dir, probe_alive};
use basemind::scanner_hold::{TEST_HOLD_SCAN_ENV, held_marker};
use basemind::store::{CACHE_DIR, WORKSPACES_DIR, WriterProbe, probe_writer_lock, workspace_key};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// A throwaway machine: data home + comms dir + one git repo with two Rust files.
struct Env {
    home: TempDir,
    repo: TempDir,
}

impl Env {
    fn new() -> Self {
        let repo = tempfile::tempdir().expect("repo tempdir");
        let root = repo.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();
        std::fs::write(root.join("c.rs"), b"pub fn caller() { alpha(); }\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "init"]);
        Self {
            home: tempfile::tempdir().expect("home tempdir"),
            repo,
        }
    }

    fn root(&self) -> &Path {
        self.repo.path()
    }

    fn comms_dir(&self) -> PathBuf {
        self.home.path().to_path_buf()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(["--root", self.root().to_str().unwrap()])
            .args(args)
            .env("BASEMIND_DATA_HOME", self.home.path())
            .env("BASEMIND_COMMS_DIR", self.comms_dir())
            .env("BASEMIND_AGENT_ID", "cli-daemon-test")
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run basemind")
    }
}

fn git(repo: &Path, args: &[&str]) {
    let status = basemind::testing::git_command()
        .args(args)
        .current_dir(repo)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exited normally")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A real detached daemon on the env's comms dir; stopped and reaped on drop.
struct Daemon {
    child: Child,
    comms_dir: PathBuf,
}

impl Daemon {
    fn start(env: &Env) -> Self {
        Self::spawn(env, Stdio::null())
    }

    /// A daemon logging at `info` to `log`, which records each relay session it hosts.
    fn start_logged(env: &Env, log: &Path) -> Self {
        Self::spawn(env, std::fs::File::create(log).expect("daemon log").into())
    }

    fn spawn(env: &Env, stderr: Stdio) -> Self {
        let comms_dir = env.comms_dir();
        let child = Command::new(BIN)
            .args(["comms", "daemon"])
            .env("BASEMIND_COMMS_DIR", &comms_dir)
            .env("BASEMIND_DATA_HOME", env.home.path())
            .env("RUST_LOG", "basemind=info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("spawn daemon");
        let daemon = Self { child, comms_dir };
        let socket = comms_socket_path(&generation_dir(&daemon.comms_dir));
        let deadline = Instant::now() + Duration::from_secs(180);
        while Instant::now() < deadline {
            if probe_alive(&socket) {
                return daemon;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon did not become ready");
    }
}

/// Block until the daemon has loaded the workspace and caught up on the tree: until then a
/// forwarded answer carries a `warming_up` or `rescanning` notice the in-process call (which loads
/// the map before answering and runs no catch-up) never shows.
fn wait_until_warm(env: &Env) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let out = env.run(&["--json", "code", "outline", "a.rs"]);
        let settled = !answer(&out).contains("\"warming_up\"") && !answer(&out).contains("\"rescanning\"");
        if code(&out) == 0 && settled {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not finish warming: {}",
            answer(&out)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .args(["comms", "stop"])
            .env("BASEMIND_COMMS_DIR", &self.comms_dir)
            .output();
        std::thread::sleep(Duration::from_millis(200));
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[test]
fn rescan_rejects_invalid_paths_with_the_usage_code() {
    let env = Env::new();
    assert_eq!(code(&env.run(&["scan", "--quiet"])), 0);
    for bad in ["../escape.rs", "no_such_file.rs"] {
        let out = env.run(&["rescan", bad]);
        assert_eq!(code(&out), 2, "`rescan {bad}`: {}", text(&out));
        assert!(text(&out).contains("invalid rescan path"), "{}", text(&out));
    }
    let out = env.run(&["rescan", "no_such_file.rs"]);
    assert_eq!(code(&out), 2, "admin rescan shares the validation: {}", text(&out));
    let ok = env.run(&["rescan", "./a.rs", "--quiet"]);
    assert_eq!(code(&ok), 0, "{}", text(&ok));
}

#[test]
fn cache_clear_demands_yes_off_a_terminal_and_names_the_global_blob_store() {
    let env = Env::new();
    assert_eq!(code(&env.run(&["scan", "--quiet"])), 0);
    let refused = env.run(&["cache", "clear", "--component", "blobs"]);
    assert_eq!(code(&refused), 2, "{}", text(&refused));
    assert!(text(&refused).contains("MACHINE-GLOBAL"), "{}", text(&refused));
    assert!(text(&refused).contains("--yes"), "{}", text(&refused));

    let views = env.run(&["cache", "clear", "--component", "views"]);
    assert_eq!(code(&views), 2, "views needs confirmation too: {}", text(&views));

    let cleared = env.run(&["cache", "clear", "--component", "views", "--yes"]);
    assert_eq!(code(&cleared), 0, "{}", text(&cleared));

    // git-cache is cheap and regenerable: no confirmation.
    assert_eq!(code(&env.run(&["cache", "clear"])), 0);
}

#[test]
fn writer_lock_collision_exits_busy_for_scan_rescan_and_cache_clear() {
    let env = Env::new();
    assert_eq!(code(&env.run(&["scan", "--quiet"])), 0);
    let mut holder = env
        .command(&["watch"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("watch");
    // Wait until the watcher owns the lock, by probing it rather than by scanning: each probing scan
    // held the lock itself, and `watch` gives up after half a second of contention, so on a loaded
    // machine it exited busy and the loop waited out its deadline.
    let workspace = env
        .home
        .path()
        .join(CACHE_DIR)
        .join(WORKSPACES_DIR)
        .join(workspace_key(env.root()));
    let deadline = Instant::now() + Duration::from_secs(60);
    while !matches!(probe_writer_lock(&workspace), WriterProbe::Held { .. }) {
        if let Some(status) = holder.try_wait().expect("poll watch") {
            panic!("watch exited ({status}) before it took the lock");
        }
        assert!(Instant::now() < deadline, "watch never took the lock");
        std::thread::sleep(Duration::from_millis(50));
    }
    let scan = env.run(&["scan", "--quiet"]);
    assert_eq!(code(&scan), 3, "scan must not run next to a writer: {}", text(&scan));
    assert!(
        text(&scan).contains("already running") || text(&scan).contains("locked"),
        "{}",
        text(&scan)
    );
    let rescan = env.run(&["rescan", "--quiet"]);
    assert_eq!(
        code(&rescan),
        3,
        "rescan must not run next to a writer: {}",
        text(&rescan)
    );
    let clear = env.run(&["cache", "clear", "--component", "views", "--yes"]);
    assert_eq!(
        code(&clear),
        3,
        "cache clear must refuse under a live writer: {}",
        text(&clear)
    );
    let _ = holder.kill();
    let _ = holder.wait();
}

#[test]
fn live_daemon_serves_rescan_memory_and_blocks_destructive_commands() {
    let env = Env::new();
    let _daemon = Daemon::start(&env);

    let rescan = env.run(&["rescan"]);
    assert_eq!(code(&rescan), 0, "{}", text(&rescan));
    assert!(text(&rescan).contains("via daemon"), "{}", text(&rescan));

    // Memory commands work only in a binary built with the `memory` feature.
    if cfg!(feature = "memory") {
        let put = env.run(&["memory", "put", "k1", "forwarded value"]);
        assert_eq!(code(&put), 0, "memory put must forward, not fail: {}", text(&put));
        let get = env.run(&["memory", "get", "k1"]);
        assert_eq!(code(&get), 0, "{}", text(&get));
        assert!(text(&get).contains("forwarded value"), "{}", text(&get));
    }

    let callers = env.run(&["--json", "code", "callers", "a.rs", "alpha"]);
    assert_eq!(code(&callers), 0, "{}", text(&callers));
    assert!(
        text(&callers).contains("c.rs"),
        "callers must see the cross-file ref: {}",
        text(&callers)
    );

    let scan = env.run(&["scan", "--quiet"]);
    assert_eq!(
        code(&scan),
        0,
        "scan next to the daemon is forwarded to it: {}",
        text(&scan)
    );
    assert!(text(&scan).contains("scan via daemon"), "{}", text(&scan));
    let blobs = env.run(&["cache", "clear", "--component", "blobs", "--yes"]);
    assert_eq!(
        code(&blobs),
        3,
        "blobs are machine-global and the daemon reads them: {}",
        text(&blobs)
    );
    assert!(text(&blobs).contains("daemon"), "{}", text(&blobs));
}

/// Wait until a scan holding on `gate` reports it is held, failing if `child` exits first.
fn wait_until_held(gate: &Path, child: &mut Child) {
    let marker = held_marker(gate);
    let deadline = Instant::now() + Duration::from_secs(120);
    while !marker.exists() {
        if let Some(status) = child.try_wait().expect("poll") {
            panic!("the scan exited ({status}) before it was held");
        }
        assert!(
            Instant::now() < deadline,
            "the scan was never held on {}",
            gate.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn ctrl_c_during_scan_stops_cleanly_and_leaves_a_consistent_index() {
    let env = Env::new();
    for i in 0..200 {
        std::fs::write(
            env.root().join(format!("f{i}.rs")),
            format!("pub fn f{i}() {{ f{}() }}\n", i + 1),
        )
        .unwrap();
    }
    let hold = tempfile::tempdir().expect("hold tempdir");
    let gate = hold.path().join("gate");
    std::fs::write(&gate, b"").unwrap();
    let mut scan = env
        .command(&["scan"])
        .env(TEST_HOLD_SCAN_ENV, &gate)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("scan");
    // Held mid-pass, after the Ctrl-C handler is installed: the signal lands on a running scan and
    // exercises the clean-stop path rather than the default kill.
    wait_until_held(&gate, &mut scan);
    // SAFETY: signalling a child PID this test spawned and still owns.
    unsafe { libc::kill(i32::try_from(scan.id()).expect("pid fits"), libc::SIGINT) };
    let status = scan.wait().expect("wait").code();
    assert_eq!(status, Some(130), "an interrupted scan exits 130");
    let rerun = env.run(&["scan", "--quiet"]);
    assert_eq!(
        code(&rerun),
        0,
        "the index must be usable after an interrupt: {}",
        text(&rerun)
    );
    let files = env.run(&["--json", "code", "symbols", "f199"]);
    assert_eq!(code(&files), 0, "{}", text(&files));
    assert!(
        String::from_utf8_lossy(&files.stdout).contains("f199.rs"),
        "{}",
        text(&files)
    );
}

/// `stdout` with the timings taken out: a call's own `elapsed_us` and the process's `startup_us`
/// differ from run to run, and nothing else may.
fn answer(out: &Output) -> String {
    fn strip(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                map.remove("elapsed_us");
                map.remove("startup_us");
                map.values_mut().for_each(strip);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    match serde_json::from_str::<serde_json::Value>(&stdout) {
        Ok(mut value) => {
            strip(&mut value);
            value.to_string()
        }
        Err(_) => stdout.into_owned(),
    }
}

/// `stderr` without tracing lines: the in-process call logs opening the store, which a forwarded
/// one does not; what the command itself prints stays.
fn diagnostics(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|line| !line.starts_with("\u{1b}[2m"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// With the daemon up, a CLI command on the working view is forwarded whole to it over one relay
/// session, and prints byte for byte what the same command prints in process: the answers, the
/// refusals and their exit codes. The daemon's log proves each call was forwarded.
#[test]
fn a_forwarded_call_prints_exactly_what_the_in_process_call_prints() {
    let env = Env::new();
    let scan = env.run(&["scan", "--quiet"]);
    assert_eq!(code(&scan), 0, "{}", text(&scan));
    let calls: &[&[&str]] = &[
        &["--json", "code", "outline", "a.rs"],
        &["--json", "code", "symbols", "alpha"],
        &["--json", "code", "read", "a.rs", "--symbol", "alpha"],
        &["--json", "git", "log"],
        &["--json", "code", "find", "a"],
        &["--json", "code", "files"],
        &["--json", "git", "churn"],
        &["code", "outline", "missing.rs"],
        &["code", "read", "a.rs", "--symbol", "nope"],
        &["code", "outline", "a.rs", "--limit", "0"],
    ];
    let in_process: Vec<(i32, String, String)> = calls
        .iter()
        .map(|args| {
            let out = env.run(args);
            (code(&out), answer(&out), diagnostics(&out))
        })
        .collect();
    assert_eq!(in_process[0].0, 0, "{}", in_process[0].2);
    assert_eq!(
        in_process[7].0, 2,
        "a path that is not indexed is a usage error: {}",
        in_process[7].2
    );
    assert!(in_process[7].2.contains("missing.rs"), "{}", in_process[7].2);
    assert_eq!(in_process[9].0, 2, "{}", in_process[9].2);

    let log = env.home.path().join("daemon-stderr.log");
    let _daemon = Daemon::start_logged(&env, &log);
    wait_until_warm(&env);
    let hosted_count = || {
        std::fs::read_to_string(&log)
            .expect("daemon log")
            .matches("relay: hosting rmcp session")
            .count()
    };
    let warm_up_calls = hosted_count();
    for (args, expected) in calls.iter().zip(&in_process) {
        let out = env.run(args);
        let forwarded = (code(&out), answer(&out), diagnostics(&out));
        assert_eq!(
            &forwarded, expected,
            "basemind {args:?} answered differently through the daemon"
        );
    }
    assert_eq!(
        hosted_count() - warm_up_calls,
        calls.len(),
        "every call is one forwarded relay session"
    );
}

/// A view the daemon does not host runs in process even with the daemon up.
#[test]
fn a_call_on_another_view_runs_in_process_next_to_the_daemon() {
    let env = Env::new();
    let log = env.home.path().join("daemon-stderr.log");
    let _daemon = Daemon::start_logged(&env, &log);
    let out = env.run(&["--view", "other", "--json", "code", "files"]);
    let hosted = std::fs::read_to_string(&log)
        .expect("daemon log")
        .matches("relay: hosting rmcp session")
        .count();
    assert_eq!(hosted, 0, "{}", text(&out));
}

/// A git call that reads only the repository answers the same on every view, so with the daemon up
/// it is forwarded whatever `--view` names: byte for byte what it prints in process on the working
/// view and on a `rev-*` view. A symbol's history reads the view's outline, so it stays in process.
#[test]
fn a_view_independent_git_call_on_another_view_is_forwarded_and_answers_alike() {
    let env = Env::new();
    std::fs::write(env.root().join("a.rs"), b"pub fn alpha() -> u32 { 1 }\n").unwrap();
    git(env.root(), &["commit", "-qam", "widen alpha"]);
    for args in [&["scan", "--quiet"][..], &["scan", "--quiet", "--rev", "HEAD"][..]] {
        let scan = env.run(args);
        assert_eq!(code(&scan), 0, "{}", text(&scan));
    }
    let head = basemind::testing::git_command()
        .args(["rev-parse", "--short=7", "HEAD"])
        .current_dir(env.root())
        .output()
        .expect("git rev-parse");
    let rev_view = format!("rev-{}", String::from_utf8_lossy(&head.stdout).trim());
    let calls: &[&[&str]] = &[
        &["git", "log"],
        &["git", "log", "--limit", "1"],
        &["git", "log", "--path", "a.rs"],
        &["git", "blame", "a.rs"],
        &["git", "blame", "a.rs", "--symbol", "alpha"],
        &["git", "diff"],
        &["git", "diff", "a.rs"],
        &["git", "churn"],
        &["git", "by-path", "a"],
        &["git", "log", "--path", "missing.rs"],
    ];
    let on = |view: &str, call: &[&str]| -> (i32, String, String) {
        let args: Vec<&str> = ["--view", view, "--json"].iter().chain(call).copied().collect();
        let out = env.run(&args);
        (code(&out), answer(&out), diagnostics(&out))
    };
    let in_process: Vec<_> = calls.iter().map(|call| on(&rev_view, call)).collect();
    for (call, rev) in calls.iter().zip(&in_process) {
        assert_eq!(
            &on("working", call),
            rev,
            "basemind {call:?} answers differently on {rev_view}"
        );
    }
    assert_eq!(in_process[0].0, 0, "{}", in_process[0].2);
    assert_eq!(in_process[9].0, 2, "{}", in_process[9].2);

    let log = env.home.path().join("daemon-stderr.log");
    let _daemon = Daemon::start_logged(&env, &log);
    for (call, expected) in calls.iter().zip(&in_process) {
        assert_eq!(
            &on(&rev_view, call),
            expected,
            "basemind {call:?} on {rev_view} differs through the daemon"
        );
    }
    let symbol_history = on(&rev_view, &["git", "log", "--path", "a.rs", "--symbol", "alpha"]);
    assert_eq!(symbol_history.0, 0, "{}", symbol_history.2);
    let hosted = std::fs::read_to_string(&log)
        .expect("daemon log")
        .matches("relay: hosting rmcp session")
        .count();
    assert_eq!(
        hosted,
        calls.len(),
        "each repository-only call is forwarded; the symbol history is not"
    );
}
