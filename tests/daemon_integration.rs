//! Daemon lifecycle integration: state that must outlive a single daemon process.
//!
//! The scattered comms/concurrency smokes already pin the in-session guarantees — two `serve`
//! sessions on one repo both read AND write through the daemon (`concurrency_smoke::
//! daemon_writer_serve_forwards_rescan_and_sees_fresh_symbols`), the machine registry
//! auto-registers a Hello cwd and a two-claimant race resolves to one winner
//! (`comms_smoke::machine_registry_auto_registers_and_worktree_claim_is_exclusive`), and the
//! blob GC reclaims only orphans (`schema_bump::schema_bump_refreshes_blobs_in_place_and_gc_
//! reclaims_only_orphans`). What none of them exercise is **durability across the daemon's own
//! lifecycle**: the registry is an atomic msgpack snapshot, so a repo registration and an advisory
//! worktree claim must survive the daemon exiting and a fresh daemon reloading the same
//! `BASEMIND_DATA_HOME` — and the reload must not clobber a live claim when a new session's Hello
//! re-enumerates the repo (`populate_git` preserves `claimed_by`). This test pins that path end to
//! end against a real detached daemon.

#![cfg(feature = "comms")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use basemind::comms::client::CommsClient;
use basemind::comms::ids::AgentId;
use basemind::comms::singleton::{CommsPaths, comms_socket_path, probe_alive};

const BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// Owns a spawned daemon process so it is always reaped. Constructed twice per test to exercise a
/// restart on the same `comms_dir` / `BASEMIND_DATA_HOME`.
struct Daemon {
    child: Child,
    comms_dir: PathBuf,
    socket: PathBuf,
}

impl Daemon {
    fn start(comms_dir: &Path) -> Self {
        let socket = comms_socket_path(comms_dir);
        let child = Command::new(BIN)
            .args(["comms", "daemon"])
            .env("BASEMIND_COMMS_DIR", comms_dir)
            // Isolate the daemon's registry snapshot + index writes to the same tempdir so this ~keep
            // test never touches the real XDG cache, and a restart reloads the same state. ~keep
            .env("BASEMIND_DATA_HOME", comms_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn comms daemon");
        let daemon = Self {
            child,
            comms_dir: comms_dir.to_path_buf(),
            socket,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if probe_alive(&daemon.socket) {
                return daemon;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("comms daemon did not become ready");
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    /// Stop the daemon and wait for the socket to go dead, so a restart on the same path binds
    /// cleanly instead of racing the outgoing process.
    fn stop(self) {
        let socket = self.socket.clone();
        drop(self);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !probe_alive(&socket) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("comms daemon did not release its socket after stop");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .args(["comms", "stop"])
            .env("BASEMIND_COMMS_DIR", &self.comms_dir)
            .output();
        if self.child.try_wait().ok().flatten().is_none() {
            std::thread::sleep(Duration::from_millis(200));
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
    }
}

/// Connect a client whose Hello carries `root` as cwd, so the daemon auto-registers that workspace.
async fn connect(socket: &Path, agent: &str, root: &Path) -> CommsClient {
    let paths = CommsPaths {
        comms_dir: socket.parent().expect("socket parent").to_path_buf(),
        socket_path: socket.to_path_buf(),
    };
    CommsClient::connect(
        &paths,
        AgentId::parse(agent).expect("agent id"),
        None,
        Some(root.to_path_buf()),
    )
    .await
    .unwrap_or_else(|e| panic!("connect {agent}: {e}"))
}

/// Run a git command in `cwd`, asserting success.
fn git(args: &[&str], cwd: &Path) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A committed git repo on branch `main` with one source file, rooted at `main`.
fn init_git_repo(main: &Path) {
    std::fs::create_dir_all(main).expect("mkdir main");
    git(&["init", "-q", "-b", "main"], main);
    git(&["config", "user.email", "t@example.com"], main);
    git(&["config", "user.name", "Test"], main);
    std::fs::write(main.join("a.rs"), b"pub fn alpha() {}\n").expect("write a.rs");
    git(&["add", "."], main);
    git(&["commit", "-qm", "init"], main);
}

/// A committed git repo with `n_files` small Rust sources under `src/`, so a rescan does real
/// per-file extraction work — the substrate for the concurrency stress (vs the single-file
/// [`init_git_repo`]).
fn init_bulk_git_repo(main: &Path, n_files: usize) {
    std::fs::create_dir_all(main.join("src")).expect("mkdir src");
    git(&["init", "-q", "-b", "main"], main);
    git(&["config", "user.email", "t@example.com"], main);
    git(&["config", "user.name", "Test"], main);
    for i in 0..n_files {
        let body = format!(
            "pub fn f{i}() -> u32 {{ {i} }}\npub struct S{i};\nimpl S{i} {{ pub fn m{i}(&self) -> u32 {{ f{i}() }} }}\n"
        );
        std::fs::write(main.join("src").join(format!("m{i}.rs")), body).expect("write src file");
    }
    git(&["add", "."], main);
    git(&["commit", "-qm", "bulk"], main);
}

/// A committed git repo whose scan takes tens of seconds in a debug build: `n_files` sources of
/// `fns_per_file` functions each. The substrate for the SIGTERM-mid-scan regression, where the
/// in-flight scan must comfortably outlast the daemon's exit deadline — a small fixture would finish
/// before the drain grace and mask the pre-fix hang entirely. Feature-neutral (plain Rust sources).
#[cfg(unix)]
fn init_heavy_git_repo(main: &Path, n_files: usize, fns_per_file: usize) {
    std::fs::create_dir_all(main.join("src")).expect("mkdir src");
    git(&["init", "-q", "-b", "main"], main);
    git(&["config", "user.email", "t@example.com"], main);
    git(&["config", "user.name", "Test"], main);
    for i in 0..n_files {
        let mut body = String::with_capacity(fns_per_file * 80);
        for j in 0..fns_per_file {
            body.push_str(&format!(
                "pub fn f{i}_{j}(x: u32) -> u32 {{ let y = x + {j}; y.wrapping_mul({m}) }}\n",
                m = i % 97 + 1
            ));
        }
        body.push_str(&format!("pub struct S{i} {{ pub a: u32, pub b: String }}\n"));
        std::fs::write(main.join("src").join(format!("m{i}.rs")), body).expect("write src file");
    }
    git(&["add", "."], main);
    git(&["commit", "-qm", "heavy"], main);
}

/// Read a positive-integer stress knob from the environment, defaulting when unset or unparseable.
/// Lets the concurrency stress be cranked far harder on a big machine (`BASEMIND_STRESS_CLIENTS=64`).
fn stress_knob(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// Regression: concurrent FIRST-touch rescans of the same COLD workspace must all succeed. The
/// daemon's workspace pool opens each cold store under fjall's exclusive index lock; before the open
/// was serialized, two rescans racing that cold open left the loser failing on the lock ("another
/// basemind process holds the lock") instead of sharing the winner's pooled entry — the post-open
/// reconciliation never ran because the loser failed inside `Store::open`. Two agents auto-scanning
/// the same repo at the same instant is exactly this race. Surfaced by the concurrency stress below.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_cold_rescans_open_the_workspace_once_and_all_succeed() {
    const RACERS: usize = 6;
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut clients = Vec::with_capacity(RACERS);
    for c in 0..RACERS {
        clients.push(connect(&socket, &format!("agent-race-{c}"), &repo).await);
    }
    let mut tasks = Vec::with_capacity(RACERS);
    for (c, mut client) in clients.into_iter().enumerate() {
        let repo = repo.clone();
        tasks.push(tokio::spawn(async move {
            client
                .rescan(repo, None, true, false)
                .await
                .map(|_| ())
                .map_err(|e| format!("racer {c}: {e}"))
        }));
    }
    for task in tasks {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(message)) => panic!("a cold-open racer must succeed, not fail on the lock: {message}"),
            Err(join) => panic!("a cold-open racer panicked: {join}"),
        }
    }
    daemon.stop();
}

/// Sustained multi-session stress against ONE daemon: many concurrent clients, each running its own
/// interleaved loop of full rescans + registry reads + advisory claim/release churn, all funneled
/// through the daemon's sole-writer workspace pool (the whole rearchitecture's promise — N sessions
/// on one repo all read AND write with no fjall lock downgrade). Asserts the daemon serves every
/// request with no torn index, no deadlock, and no panic, then stays responsive and consistent
/// afterward. `#[ignore]` (heavy); tune with `BASEMIND_STRESS_{CLIENTS,ITERS,FILES}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "stress: many concurrent clients hammer one daemon; run with --ignored"]
async fn stress_many_concurrent_sessions_read_and_write_through_one_daemon() {
    let clients = stress_knob("BASEMIND_STRESS_CLIENTS", 8);
    let iters = stress_knob("BASEMIND_STRESS_ITERS", 8);
    let files = stress_knob("BASEMIND_STRESS_FILES", 150);

    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_bulk_git_repo(&repo, files);

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut boot = connect(&socket, "agent-stress-boot", &repo).await;
    let repo_id = boot.list_workspaces().await.expect("list workspaces")[0]
        .repo_id
        .clone()
        .expect("git workspace has a repo id");
    drop(boot);

    let mut tasks = Vec::with_capacity(clients);
    for c in 0..clients {
        let socket = socket.clone();
        let repo = repo.clone();
        let repo_id = repo_id.clone();
        tasks.push(tokio::spawn(async move {
            let agent = format!("agent-stress-{c}");
            let mut client = connect(&socket, &agent, &repo).await;
            for _ in 0..iters {
                client
                    .rescan(repo.clone(), None, true, false)
                    .await
                    .map_err(|e| format!("{agent} rescan: {e}"))?;
                let workspaces = client
                    .list_workspaces()
                    .await
                    .map_err(|e| format!("{agent} list_workspaces: {e}"))?;
                if workspaces.is_empty() {
                    return Err(format!("{agent}: registry lost the workspace mid-storm"));
                }
                let name = format!("wt-{c}");
                let _ = client
                    .claim_worktree(repo_id.clone(), name.clone(), agent.clone())
                    .await;
                let _ = client.release_worktree(repo_id.clone(), name, agent.clone()).await;
            }
            Ok::<(), String>(())
        }));
    }

    let join_all = async {
        let mut outcomes = Vec::with_capacity(tasks.len());
        for task in tasks {
            outcomes.push(task.await);
        }
        outcomes
    };
    let outcomes = tokio::time::timeout(Duration::from_secs(180), join_all)
        .await
        .expect("all stress clients must finish within 180s (no daemon deadlock)");
    for (i, outcome) in outcomes.into_iter().enumerate() {
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(message)) => panic!("stress client {i} failed: {message}"),
            Err(join) => panic!("stress client {i} panicked: {join}"),
        }
    }

    let mut after = connect(&socket, "agent-stress-after", &repo).await;
    let workspaces = after.list_workspaces().await.expect("post-storm list_workspaces");
    assert_eq!(
        workspaces.len(),
        1,
        "exactly one workspace must remain registered after the storm, got {}",
        workspaces.len()
    );
    let report = after
        .rescan(repo.clone(), None, true, false)
        .await
        .expect("post-storm rescan");
    assert!(
        report.scanned >= 1,
        "a post-storm rescan must still do real work (index not torn), got scanned={}",
        report.scanned
    );
    drop(after);
    daemon.stop();
}

/// Regression for the `comms stop` no-op (#34): a `Stop` RPC must actually terminate the daemon by
/// firing the accept-loop shutdown signal — not merely ack while the process lingers, which left
/// orphaned daemons piling up across sessions, reaped only by an external kill. This spawns a real
/// daemon, sends `basemind comms stop`, and asserts the process exits ON ITS OWN within a tight
/// window, WITHOUT the test ever killing it. Before the fix the broker held no shutdown sender, so
/// `begin_drain` set `Draining` but never broke the accept loop and this would time out.
#[test]
fn comms_stop_terminates_the_daemon_without_an_external_kill() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    std::fs::create_dir_all(&comms_dir).expect("mkdir comms");
    let socket = comms_socket_path(&comms_dir);

    let mut child = Command::new(BIN)
        .args(["comms", "daemon"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .env("BASEMIND_DATA_HOME", &comms_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn comms daemon");

    let ready_by = Instant::now() + Duration::from_secs(10);
    while Instant::now() < ready_by && !probe_alive(&socket) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(probe_alive(&socket), "daemon did not become ready");

    let stop = Command::new(BIN)
        .args(["comms", "stop"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .output()
        .expect("run comms stop");
    assert!(
        stop.status.success(),
        "comms stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );

    let exit_by = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while Instant::now() < exit_by {
        if child.try_wait().expect("try_wait").is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
        panic!("daemon did not self-terminate after `comms stop` within 10s (the #34 no-op bug)");
    }
    let _ = child.wait();
    assert!(
        !probe_alive(&socket),
        "the socket must be released once the daemon self-terminates"
    );
}

/// Issue #44: SIGTERM must interrupt a MID-SCAN daemon. Every drain route (Stop RPC, SIGTERM,
/// idle-reap, socket-ownership loss) converges on `Broker::finish_drain`, which now trips the
/// broker's `ScanCancel` token so the in-flight `spawn_blocking` scan stops at per-file granularity,
/// and the daemon entry point bounds its runtime teardown (`RUNTIME_SHUTDOWN_TIMEOUT`) instead of
/// letting the implicit runtime drop wait forever on the blocking thread. Before the fix, the drain
/// grace elapsed, the accept loop exited — and the process then hung inside the runtime drop until
/// the scan of the whole tree completed, so only SIGKILL could end a scanning daemon; against this
/// fixture (a scan that outlasts the 12s exit deadline) the try_wait poll below timed out.
///
/// Consistency after the interrupt is the second half: blobs are content-addressed, fjall commits
/// per batch, and the cancelled pass skips the stale purge, so a fresh daemon on the same comms dir
/// must reopen the partially-committed index cleanly and complete a full rescan.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_mid_scan_exits_within_grace_and_index_reopens_cleanly() {
    // ~47 MB of generated Rust: a debug-build scan of this runs well past the 12s exit deadline ~keep
    // (~23s measured on an M4), so a pre-fix daemon — which cannot exit before the scan finishes — ~keep
    // reliably fails the deadline, while a post-fix daemon cancels and exits within a few seconds. ~keep
    const HEAVY_FILES: usize = 3000;
    const HEAVY_FNS_PER_FILE: usize = 200;

    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    std::fs::create_dir_all(&comms_dir).expect("mkdir comms");
    let repo = tmp.path().join("repo");
    init_heavy_git_repo(&repo, HEAVY_FILES, HEAVY_FNS_PER_FILE);

    let socket = comms_socket_path(&comms_dir);
    let mut child = Command::new(BIN)
        .args(["comms", "daemon"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .env("BASEMIND_DATA_HOME", &comms_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn comms daemon");
    let ready_by = Instant::now() + Duration::from_secs(10);
    while Instant::now() < ready_by && !probe_alive(&socket) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(probe_alive(&socket), "daemon did not become ready");

    let mut client = connect(&socket, "agent-sigterm", &repo).await;
    let rescan_repo = repo.clone();
    let rescan_task = tokio::spawn(async move { client.rescan(rescan_repo, None, true, false).await });
    // Long enough for the scan to be well in flight, far shorter than the scan itself. ~keep
    tokio::time::sleep(Duration::from_millis(300)).await;

    let term = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("run kill -TERM");
    assert!(term.success(), "kill -TERM must be delivered");

    let exit_by = Instant::now() + Duration::from_secs(12);
    let mut exited = false;
    while Instant::now() < exit_by {
        if child.try_wait().expect("try_wait").is_some() {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
        panic!("daemon did not exit within 12s of SIGTERM with a scan in flight (the SIGKILL-only hang)");
    }
    let _ = child.wait();

    // The in-flight rescan resolved rather than hanging: Ok if it somehow finished before the ~keep
    // drain, otherwise a clean Err (cancelled-pass error or broken link) — never a panic. ~keep
    let rescan_outcome = tokio::time::timeout(Duration::from_secs(15), rescan_task)
        .await
        .expect("the in-flight rescan future must resolve once the daemon exits, not hang");
    match rescan_outcome {
        Ok(Ok(_)) | Ok(Err(_)) => {}
        Err(join) => panic!("rescan task must not panic: {join}"),
    }

    // A fresh daemon reopens the partially-committed index cleanly and completes the full pass. ~keep
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();
    let mut client = connect(&socket, "agent-sigterm-2", &repo).await;
    let report = client
        .rescan(repo.clone(), None, true, false)
        .await
        .expect("a full rescan after the interrupted pass must succeed");
    assert!(
        report.scanned >= HEAVY_FILES,
        "the interrupted index must reopen cleanly and the full rescan cover every file, got scanned={}",
        report.scanned
    );
    drop(client);
    daemon.stop();
}

/// The machine registry and an advisory worktree claim are a durable msgpack snapshot: both must
/// survive the daemon exiting and a fresh daemon reloading the same `BASEMIND_DATA_HOME`, and the
/// reload must not clobber the live claim when a new session's Hello re-enumerates the repo.
#[tokio::test(flavor = "multi_thread")]
async fn registry_and_worktree_claim_survive_a_daemon_restart() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut alice = connect(&socket, "agent-alice", &repo).await;
    let workspaces = alice.list_workspaces().await.expect("list workspaces");
    assert_eq!(workspaces.len(), 1, "Hello cwd auto-registers exactly one workspace");
    let repo_id = workspaces[0].repo_id.clone().expect("a git workspace has a repo id");

    let claimed = alice
        .claim_worktree(repo_id.clone(), "(main)".to_string(), "agent-alice".to_string())
        .await
        .expect("alice claim");
    assert!(claimed, "alice takes the previously-unclaimed (main) worktree");

    drop(alice);
    daemon.stop();

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut bob = connect(&socket, "agent-bob", &repo).await;
    let workspaces = bob.list_workspaces().await.expect("list workspaces after restart");
    assert_eq!(
        workspaces.len(),
        1,
        "the registered workspace survives the daemon restart"
    );
    assert_eq!(
        workspaces[0].repo_id.as_deref(),
        Some(repo_id.as_str()),
        "the same repo id reloads from the snapshot"
    );

    let worktrees = bob
        .list_worktrees(repo_id.clone())
        .await
        .expect("list worktrees after restart");
    let main = worktrees
        .iter()
        .find(|w| w.name == "(main)")
        .expect("(main) worktree present after restart");
    assert_eq!(
        main.claimed_by.as_deref(),
        Some("agent-alice"),
        "populate_git preserves the reloaded claim when Hello re-enumerates the repo"
    );

    let bob_won = bob
        .claim_worktree(repo_id.clone(), "(main)".to_string(), "agent-bob".to_string())
        .await
        .expect("bob claim after restart");
    assert!(
        !bob_won,
        "the surviving claim blocks a second claimant across the restart"
    );

    let released = bob
        .release_worktree(repo_id.clone(), "(main)".to_string(), "agent-alice".to_string())
        .await
        .expect("release alice's surviving claim");
    assert!(released, "the reloaded claim is releasable by its original holder");
    let bob_won = bob
        .claim_worktree(repo_id.clone(), "(main)".to_string(), "agent-bob".to_string())
        .await
        .expect("bob claim after release");
    assert!(bob_won, "with the claim released, the worktree is claimable again");

    drop(bob);
    daemon.stop();
}

/// Two `basemind comms daemon` processes racing a cold `comms_dir` converge on exactly one live
/// daemon: the bind-as-lock in `singleton::bind_listener` means the loser's bind fails
/// (`AddrInUse`), it probes the winner's socket, finds it alive, and exits cleanly rather than
/// unlinking and rebinding. Both children are reaped regardless of who won.
#[tokio::test(flavor = "multi_thread")]
async fn should_converge_on_one_live_daemon_when_two_processes_race_a_cold_bind() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let socket = comms_socket_path(&comms_dir);

    let spawn_one = || {
        Command::new(BIN)
            .args(["comms", "daemon"])
            .env("BASEMIND_COMMS_DIR", &comms_dir)
            .env("BASEMIND_DATA_HOME", &comms_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn comms daemon")
    };

    let mut child_a = spawn_one();
    let mut child_b = spawn_one();

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut alive = false;
    while Instant::now() < deadline {
        if probe_alive(&socket) {
            alive = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(alive, "one of the two racing daemons must come up serving");

    let mut client = connect(&socket, "agent-race", tmp.path()).await;
    let report = client.status().await.expect("status against the winning daemon");
    assert!(
        report.pid > 0,
        "the winning daemon reports a real pid, got {}",
        report.pid
    );
    drop(client);

    let _ = Command::new(BIN)
        .args(["comms", "stop"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .output();

    for child in [&mut child_a, &mut child_b] {
        if child.try_wait().ok().flatten().is_none() {
            std::thread::sleep(Duration::from_millis(300));
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
        }
        let _ = child.wait();
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !probe_alive(&socket) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("socket still answers after both racing daemons were reaped");
}

/// The registry's branch and worktree rows track real git state through the daemon's lifetime:
/// creating a branch, adding a linked worktree, and removing it are all picked up on the NEXT
/// Hello (`populate_git` re-enumerates from git plumbing), without restarting the daemon itself.
#[tokio::test(flavor = "multi_thread")]
async fn should_reflect_branch_creation_and_worktree_add_remove_on_fresh_connects() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut client = connect(&socket, "agent-reg", &repo).await;
    let workspaces = client.list_workspaces().await.expect("list workspaces");
    assert_eq!(workspaces.len(), 1, "Hello cwd auto-registers exactly one workspace");
    let repo_id = workspaces[0].repo_id.clone().expect("a git workspace has a repo id");

    let branches = client.list_branches(repo_id.clone()).await.expect("list branches");
    assert!(
        branches.iter().any(|b| b.name == "main"),
        "the initial checkout's branch is enumerated, got {branches:?}"
    );
    drop(client);

    git(&["branch", "feature"], &repo);
    let mut client = connect(&socket, "agent-reg-2", &repo).await;
    let branches = client
        .list_branches(repo_id.clone())
        .await
        .expect("list branches after branch create");
    assert!(
        branches.iter().any(|b| b.name == "feature"),
        "the new branch is enumerated after a fresh Hello re-populates, got {branches:?}"
    );
    drop(client);

    let linked_path = tmp.path().join("linked-wt");
    git(
        &[
            "worktree",
            "add",
            "-b",
            "wt-feature",
            linked_path.to_str().expect("utf8 path"),
            "feature",
        ],
        &repo,
    );
    let mut client = connect(&socket, "agent-reg-3", &repo).await;
    let worktrees = client
        .list_worktrees(repo_id.clone())
        .await
        .expect("list worktrees after add");
    let linked = worktrees
        .iter()
        .find(|w| w.path == linked_path.canonicalize().expect("canonicalize linked path"));
    assert!(
        linked.is_some(),
        "the newly linked worktree is enumerated after a fresh Hello, got {worktrees:?}"
    );
    assert_eq!(
        linked.expect("checked above").branch.as_deref(),
        Some("wt-feature"),
        "the linked worktree's checked-out branch is recorded"
    );
    drop(client);

    git(&["worktree", "remove", linked_path.to_str().expect("utf8 path")], &repo);
    let mut client = connect(&socket, "agent-reg-4", &repo).await;
    let worktrees = client
        .list_worktrees(repo_id.clone())
        .await
        .expect("list worktrees after remove");
    assert!(
        !worktrees
            .iter()
            .any(|w| w.path == linked_path.canonicalize().unwrap_or(linked_path.clone())),
        "the removed worktree is pruned from the registry after a fresh Hello, got {worktrees:?}"
    );
    drop(client);

    daemon.stop();
}

/// Guards the known orphan-daemon-pile-up bug: a daemon whose socket is unlinked out from under
/// it (e.g. reclaimed by a second daemon after a crash left a stale file) must notice via its
/// ownership watchdog and self-terminate, rather than lingering as an unreachable orphan.
///
/// Run explicitly: the watchdog fires on a ~30s timer (`OWNERSHIP_CHECK_EVERY` in
/// `src/cli/comms_daemon.rs`), so this test is inherently slow.
#[cfg(all(feature = "comms", unix))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "watchdog fires on a ~30s timer; run explicitly with --ignored"]
async fn should_self_terminate_when_its_socket_is_reclaimed_by_another_daemon() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let socket = comms_socket_path(&comms_dir);

    let mut child_a = Command::new(BIN)
        .args(["comms", "daemon"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .env("BASEMIND_DATA_HOME", &comms_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon A");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !probe_alive(&socket) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(probe_alive(&socket), "daemon A must come up before we orphan it");

    std::fs::remove_file(&socket).expect("unlink daemon A's socket");

    let mut child_b = Command::new(BIN)
        .args(["comms", "daemon"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .env("BASEMIND_DATA_HOME", &comms_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon B");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !probe_alive(&socket) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(probe_alive(&socket), "daemon B must come up on the rebound socket");

    let watchdog_deadline = Instant::now() + Duration::from_secs(90);
    let mut a_exited = false;
    while Instant::now() < watchdog_deadline {
        if child_a.try_wait().ok().flatten().is_some() {
            a_exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(
        a_exited,
        "daemon A must self-terminate once its socket is reclaimed by daemon B (orphan watchdog)"
    );

    let _ = child_a.wait();
    let _ = Command::new(BIN)
        .args(["comms", "stop"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .output();
    if child_b.try_wait().ok().flatten().is_none() {
        std::thread::sleep(Duration::from_millis(300));
        if child_b.try_wait().ok().flatten().is_none() {
            let _ = child_b.kill();
        }
    }
    let _ = child_b.wait();
}

/// A pre-0.22 install left a legacy IN-REPO `.basemind/index.msgpack` (stale `schema_ver`) at the
/// old location. Since the global-cache re-root, the daemon's rescan never reads or writes that
/// file — all state lives under `BASEMIND_DATA_HOME/cache/workspaces/<key>/` — so a `rescan`
/// against such a repo must succeed, leave the legacy file untouched, and populate the global
/// cache for that workspace key.
#[tokio::test(flavor = "multi_thread")]
async fn should_rescan_successfully_and_ignore_a_stale_legacy_in_repo_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    #[derive(serde::Serialize)]
    struct LegacyIndex {
        schema_ver: u16,
        files: std::collections::BTreeMap<String, ()>,
        doc_files: std::collections::BTreeMap<String, ()>,
    }
    let legacy_dir = repo.join(".basemind");
    std::fs::create_dir_all(&legacy_dir).expect("mkdir legacy .basemind");
    let legacy_index = LegacyIndex {
        schema_ver: 21,
        files: std::collections::BTreeMap::new(),
        doc_files: std::collections::BTreeMap::new(),
    };
    let legacy_bytes = rmp_serde::to_vec_named(&legacy_index).expect("encode legacy index");
    let legacy_index_path = legacy_dir.join("index.msgpack");
    std::fs::write(&legacy_index_path, &legacy_bytes).expect("write legacy index.msgpack");
    let legacy_bytes_before = std::fs::read(&legacy_index_path).expect("read back legacy index.msgpack");

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut client = connect(&socket, "agent-migrate", &repo).await;
    let report = client
        .rescan(repo.clone(), None, true, false)
        .await
        .expect("rescan a repo carrying a stale legacy in-repo index");
    assert!(
        report.scanned >= 1,
        "rescan must actually scan the repo's files, got scanned={}",
        report.scanned
    );

    let legacy_bytes_after = std::fs::read(&legacy_index_path).expect("re-read legacy index.msgpack");
    assert_eq!(
        legacy_bytes_after, legacy_bytes_before,
        "the in-repo legacy .basemind/index.msgpack must be left byte-for-byte untouched"
    );

    // NOTE: we assert the workspace directory's existence (the weaker, currently-checkable
    let workspace_key = basemind::store::workspace_key(&repo);
    let workspace_dir = comms_dir.join("cache").join("workspaces").join(&workspace_key);
    assert!(
        workspace_dir.exists(),
        "the global cache must gain a workspace dir for this repo at {}",
        workspace_dir.display()
    );

    drop(client);
    daemon.stop();
}

/// A rescan in flight when `comms stop` is requested must resolve cleanly — either it finishes
/// before the process actually goes away, or the connection breaks and the client surfaces a
/// clean `Err` — never a panic and never a hang. A subsequent daemon restart on the same
/// `comms_dir` reloads the registry snapshot intact, proving the stop did not corrupt durable
/// state.
///
/// `CommsRequest::Stop` routes through `Broker::begin_drain` (`src/comms/daemon.rs`), which now
/// fires the accept-loop shutdown signal the daemon entry point installed, so the process actually
/// self-terminates (its dedicated regression is
/// `comms_stop_terminates_the_daemon_without_an_external_kill`). This test pins the harder property
/// under that shutdown: a rescan in flight when the stop lands still resolves cleanly — it finishes
/// before the runtime tears down, or the link breaks and the client surfaces a clean `Err` — never
/// a panic and never a hang. A fresh daemon on the same `comms_dir` then reloads the registry
/// snapshot intact, proving the abrupt stop left no torn durable state.
#[tokio::test(flavor = "multi_thread")]
async fn should_drain_cleanly_with_an_in_flight_rescan_and_reload_registry_after_restart() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut client = connect(&socket, "agent-drain", &repo).await;
    let workspaces = client.list_workspaces().await.expect("list workspaces");
    let repo_id = workspaces[0].repo_id.clone().expect("git workspace has a repo id");

    let rescan_repo = repo.clone();
    let rescan_task = tokio::spawn(async move { client.rescan(rescan_repo, None, true, false).await });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let comms_dir_for_stop = comms_dir.clone();
    let stop_output = tokio::task::spawn_blocking(move || {
        Command::new(BIN)
            .args(["comms", "stop"])
            .env("BASEMIND_COMMS_DIR", &comms_dir_for_stop)
            .output()
    })
    .await
    .expect("join stop task")
    .expect("run the `comms stop` CLI invocation");
    assert!(
        stop_output.status.success(),
        "the `comms stop` RPC must succeed even with a rescan in flight, stderr: {}",
        String::from_utf8_lossy(&stop_output.stderr)
    );

    let rescan_outcome = tokio::time::timeout(Duration::from_secs(15), rescan_task)
        .await
        .expect("in-flight rescan must resolve within the timeout, not hang");
    match rescan_outcome {
        Ok(Ok(report)) => {
            assert!(
                report.scanned >= 1,
                "a rescan that completed must report real work, got scanned={}",
                report.scanned
            );
        }
        Ok(Err(client_error)) => {
            let _ = client_error;
        }
        Err(join_error) => panic!("rescan task must not panic, got: {join_error}"),
    }
    daemon.stop();

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();
    let mut client = connect(&socket, "agent-drain-2", &repo).await;
    let workspaces = client.list_workspaces().await.expect("list workspaces after restart");
    assert_eq!(
        workspaces.len(),
        1,
        "the registered workspace survives the drain + restart"
    );
    assert_eq!(
        workspaces[0].repo_id.as_deref(),
        Some(repo_id.as_str()),
        "the same repo id reloads from the snapshot after the drain"
    );

    drop(client);
    daemon.stop();
}
