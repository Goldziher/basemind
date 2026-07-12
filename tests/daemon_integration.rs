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
        drop(self); // Drop runs `comms stop` + reaps the child.
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

/// The machine registry and an advisory worktree claim are a durable msgpack snapshot: both must
/// survive the daemon exiting and a fresh daemon reloading the same `BASEMIND_DATA_HOME`, and the
/// reload must not clobber the live claim when a new session's Hello re-enumerates the repo.
#[tokio::test(flavor = "multi_thread")]
async fn registry_and_worktree_claim_survive_a_daemon_restart() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    // --- First daemon: register the repo (via Hello cwd) and take an advisory claim. ---
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

    // Drop the client before restarting so its link does not outlive the daemon.
    drop(alice);
    daemon.stop();

    // --- Second daemon on the SAME data home: state must reload from the snapshot. ---
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    // A fresh session (whose Hello re-registers + re-enumerates the repo) still sees the workspace...
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

    // ...and the advisory claim survives: the row is still held by alice through the re-enumeration.
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

    // A competing claimant is therefore rejected until the holder releases.
    let bob_won = bob
        .claim_worktree(repo_id.clone(), "(main)".to_string(), "agent-bob".to_string())
        .await
        .expect("bob claim after restart");
    assert!(
        !bob_won,
        "the surviving claim blocks a second claimant across the restart"
    );

    // Releasing as the original holder frees it, and bob can then claim — proving the reloaded
    // claim is a real, releasable row and not a frozen artifact.
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
