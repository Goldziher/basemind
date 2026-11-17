//! `basemind scan` next to a running daemon (ADR-0024 rule 5): the daemon is the workspace's
//! writer, so a working-tree scan is forwarded to it; with no daemon the CLI scans directly and
//! starts none; a flag the forwarded scan cannot honour is refused, never dropped.
//!
//! Every child process runs with a throwaway `BASEMIND_DATA_HOME` and `BASEMIND_COMMS_DIR`.

#![cfg(all(feature = "comms", unix))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use basemind::comms::singleton::{comms_socket_path, generation_dir, probe_alive};
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

    fn socket(&self) -> PathBuf {
        comms_socket_path(&generation_dir(&self.comms_dir()))
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(["--root", self.root().to_str().unwrap()])
            .args(args)
            .env("BASEMIND_DATA_HOME", self.home.path())
            .env("BASEMIND_COMMS_DIR", self.comms_dir())
            .env("BASEMIND_AGENT_ID", "scan-forwarding-test")
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
        let child = Command::new(BIN)
            .args(["comms", "daemon"])
            .env("BASEMIND_COMMS_DIR", env.comms_dir())
            .env("BASEMIND_DATA_HOME", env.home.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon");
        let daemon = Self {
            child,
            comms_dir: env.comms_dir(),
        };
        let deadline = Instant::now() + Duration::from_secs(180);
        while Instant::now() < deadline {
            if probe_alive(&env.socket()) {
                return daemon;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon did not become ready");
    }

    fn pid(&self) -> u32 {
        self.child.id()
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

/// Whether `name` is defined in the index, asked through whatever serves it.
fn defines(env: &Env, name: &str) -> bool {
    let out = env.run(&["--json", "code", "symbols", name]);
    assert_eq!(code(&out), 0, "{}", text(&out));
    String::from_utf8_lossy(&out.stdout).contains("fresh.rs")
}

#[test]
fn a_scan_next_to_a_hosting_daemon_is_forwarded_and_updates_its_index() {
    let env = Env::new();
    let daemon = Daemon::start(&env);
    // The daemon takes the workspace and keeps its writer lock.
    let rescan = env.run(&["rescan"]);
    assert_eq!(code(&rescan), 0, "{}", text(&rescan));

    std::fs::write(env.root().join("fresh.rs"), b"pub fn freshly_added() {}\n").unwrap();
    let scan = env.run(&["scan"]);
    assert_eq!(
        code(&scan),
        0,
        "a scan next to the daemon is forwarded: {}",
        text(&scan)
    );
    assert!(text(&scan).contains("scan via daemon"), "{}", text(&scan));
    assert!(
        text(&scan).contains("git-history index:"),
        "the history index is synced as a direct scan syncs it: {}",
        text(&scan)
    );
    assert!(defines(&env, "freshly_added"), "the daemon's index has the new file");
    assert!(
        probe_alive(&env.socket()),
        "the daemon (pid {}) still serves",
        daemon.pid()
    );

    // A daemon that runs but does not host the workspace can: the lock is free, so it takes it.
    drop(daemon);
    let daemon = Daemon::start(&env);
    std::fs::write(
        env.root().join("fresh.rs"),
        b"pub fn freshly_added() {}\npub fn freshly_again() {}\n",
    )
    .unwrap();
    let scan = env.run(&["scan", "--no-git-history"]);
    assert_eq!(code(&scan), 0, "{}", text(&scan));
    assert!(text(&scan).contains("scan via daemon"), "{}", text(&scan));
    assert!(
        !text(&scan).contains("git-history index:"),
        "`--no-git-history` is honoured: {}",
        text(&scan)
    );
    assert!(defines(&env, "freshly_again"));
    drop(daemon);
}

#[test]
fn with_no_daemon_a_scan_runs_directly_and_starts_none() {
    let env = Env::new();
    let off = env
        .command(&["scan"])
        .env("BASEMIND_NO_AUTOSPAWN", "1")
        .output()
        .expect("scan");
    assert_eq!(code(&off), 0, "{}", text(&off));
    assert!(!text(&off).contains("via daemon"), "{}", text(&off));
    assert!(!probe_alive(&env.socket()), "no daemon with autospawn off");

    // A CLI command does not start a daemon just to scan (ADR-0024 rule 5).
    let on = env.run(&["scan", "--quiet"]);
    assert_eq!(code(&on), 0, "{}", text(&on));
    assert!(!text(&on).contains("via daemon"), "{}", text(&on));
    assert!(!probe_alive(&env.socket()), "the scan started no daemon");
}

#[test]
fn a_flag_the_daemon_cannot_apply_is_refused_with_its_name() {
    let env = Env::new();
    let _daemon = Daemon::start(&env);
    let rescan = env.run(&["rescan"]);
    assert_eq!(code(&rescan), 0, "{}", text(&rescan));

    for flags in [&["--rebuild-git-history"][..], &["--documents-enabled", "false"][..]] {
        let out = env.run(&[&["scan"][..], flags].concat());
        assert_eq!(code(&out), 2, "`scan {flags:?}`: {}", text(&out));
        assert!(
            text(&out).contains(flags[0]),
            "the refusal names {}: {}",
            flags[0],
            text(&out)
        );
        assert!(text(&out).contains("basemind comms stop"), "{}", text(&out));
    }

    // The daemon holds the workspace and scans only the working tree.
    let staged = env.run(&["scan", "--staged"]);
    assert_eq!(code(&staged), 3, "{}", text(&staged));
    assert!(
        text(&staged).contains("`--staged` cannot be forwarded"),
        "{}",
        text(&staged)
    );
}

#[test]
fn a_scan_while_the_daemon_is_mid_rescan_waits_for_it() {
    let env = Env::new();
    for i in 0..6000 {
        std::fs::write(
            env.root().join(format!("f{i}.rs")),
            format!("pub fn f{i}() {{ f{}() }}\n", i + 1),
        )
        .unwrap();
    }
    let _daemon = Daemon::start(&env);
    let mut rescan = env
        .command(&["rescan"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("rescan");
    std::thread::sleep(Duration::from_millis(300));
    let overlapped = rescan.try_wait().expect("poll").is_none();
    let scan = env.run(&["scan"]);
    assert_eq!(code(&scan), 0, "never a collision error: {}", text(&scan));
    assert!(text(&scan).contains("scan via daemon"), "{}", text(&scan));
    assert!(rescan.wait().expect("rescan").success());
    if !overlapped {
        eprintln!("note: the rescan finished before the scan started on this machine");
    }
    let files = env.run(&["--json", "code", "symbols", "f5999"]);
    assert_eq!(code(&files), 0, "{}", text(&files));
    assert!(
        String::from_utf8_lossy(&files.stdout).contains("f5999.rs"),
        "{}",
        text(&files)
    );
}

#[test]
fn a_lock_held_by_a_process_other_than_the_daemon_keeps_the_notice() {
    let env = Env::new();
    assert_eq!(code(&env.run(&["scan", "--quiet"])), 0);
    let mut holder = env
        .command(&["watch"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("watch");
    // With no daemon yet, a direct scan exits busy (3) once the watcher owns the lock.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let out = env.run(&["scan", "--quiet"]);
        if code(&out) == 3 {
            break;
        }
        assert!(Instant::now() < deadline, "watch never took the lock: {}", text(&out));
        std::thread::sleep(Duration::from_millis(250));
    }
    let _daemon = Daemon::start(&env);
    let out = env.run(&["scan", "--quiet"]);
    assert_eq!(code(&out), 3, "{}", text(&out));
    assert!(text(&out).contains("already running"), "{}", text(&out));
    assert!(!text(&out).contains("via daemon"), "{}", text(&out));
    let _ = holder.kill();
    let _ = holder.wait();
}
