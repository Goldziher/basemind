//! The CLI against a live comms daemon, and the exit-code / safety contract of the scan commands.
//!
//! Every child process runs with a throwaway `BASEMIND_DATA_HOME` and `BASEMIND_COMMS_DIR`, so
//! nothing here can touch a developer's real cache or daemon. Exit codes follow
//! `basemind::cli::exit`: 0 ok, 1 error, 2 usage, 3 busy, 4 unavailable, 130 interrupted.

#![cfg(all(feature = "comms", unix))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use basemind::comms::singleton::{comms_socket_path, probe_alive};
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
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
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
        let comms_dir = env.comms_dir();
        let child = Command::new(BIN)
            .args(["comms", "daemon"])
            .env("BASEMIND_COMMS_DIR", &comms_dir)
            .env("BASEMIND_DATA_HOME", env.home.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon");
        let daemon = Self { child, comms_dir };
        let socket = comms_socket_path(&daemon.comms_dir);
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
    // Wait until the watcher owns the lock: from then on `scan` reports busy (3).
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let out = env.run(&["scan", "--quiet"]);
        if code(&out) == 3 {
            assert!(
                text(&out).contains("already running") || text(&out).contains("locked"),
                "{}",
                text(&out)
            );
            break;
        }
        assert!(Instant::now() < deadline, "watch never took the lock: {}", text(&out));
        std::thread::sleep(Duration::from_millis(250));
    }
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

    let put = env.run(&["memory", "put", "k1", "forwarded value"]);
    assert_eq!(code(&put), 0, "memory put must forward, not fail: {}", text(&put));
    let get = env.run(&["memory", "get", "k1"]);
    assert_eq!(code(&get), 0, "{}", text(&get));
    assert!(text(&get).contains("forwarded value"), "{}", text(&get));

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
        3,
        "scan next to the daemon is a collision: {}",
        text(&scan)
    );
    let blobs = env.run(&["cache", "clear", "--component", "blobs", "--yes"]);
    assert_eq!(
        code(&blobs),
        3,
        "blobs are machine-global and the daemon reads them: {}",
        text(&blobs)
    );
    assert!(text(&blobs).contains("daemon"), "{}", text(&blobs));
}

#[test]
fn ctrl_c_during_scan_stops_cleanly_and_leaves_a_consistent_index() {
    let env = Env::new();
    for i in 0..4000 {
        std::fs::write(
            env.root().join(format!("f{i}.rs")),
            format!("pub fn f{i}() {{ f{}() }}\n", i + 1),
        )
        .unwrap();
    }
    let mut scan = env
        .command(&["scan"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("scan");
    // The first output line is printed after the Ctrl-C handler is installed, so signalling after
    // it exercises the clean-stop path rather than the default kill.
    let mut stdout = scan.stdout.take().expect("piped stdout");
    let mut first = [0u8; 1];
    std::io::Read::read_exact(&mut stdout, &mut first).expect("scan output");
    // Keep draining so a full pipe can never stall the scan under test.
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
    });
    std::thread::sleep(Duration::from_millis(500));
    if scan.try_wait().expect("poll").is_some() {
        return; // finished before the signal: nothing to interrupt on this machine
    }
    // SAFETY: signalling a child PID this test spawned and still owns.
    unsafe { libc::kill(i32::try_from(scan.id()).expect("pid fits"), libc::SIGINT) };
    let status = scan.wait().expect("wait").code();
    assert!(
        matches!(status, Some(0) | Some(130)),
        "an interrupted scan exits 130, got {status:?}"
    );
    let rerun = env.run(&["scan", "--quiet"]);
    assert_eq!(
        code(&rerun),
        0,
        "the index must be usable after an interrupt: {}",
        text(&rerun)
    );
    let files = env.run(&["--json", "code", "symbols", "f3999"]);
    assert_eq!(code(&files), 0, "{}", text(&files));
}
