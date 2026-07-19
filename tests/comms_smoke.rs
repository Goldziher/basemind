//! End-to-end smoke test for the thread-model comms client against a REAL detached broker daemon.
//!
//! The daemon is the actual `basemind comms daemon` process (spawned via the test binary's
//! `CARGO_BIN_EXE_basemind`), isolated to a tempdir via `BASEMIND_COMMS_DIR`. We drive the library
//! [`CommsClient`] directly. This pins the thread contract end to end:
//!
//! * `thread_start` needs ≥2 of subject / path / members;
//! * `inbox_ack` by `message_ids` advances ONLY the acking agent's read cursor — the acked
//!   messages drop out of that agent's next `inbox_read`, but `thread_history` STILL returns them,
//!   and a second agent's inbox is unaffected;
//! * the `to_seq` bulk mode clears a thread straight to a seq;
//! * a client transparently recovers when the daemon dies mid-session;
//! * discovery is scoped — a non-member with no path match sees nothing in `thread_list`;
//! * recency-aware `thread_history` honours an absolute `since_micros` cutoff.

#![cfg(feature = "comms")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use basemind::comms::client::CommsClient;
use basemind::comms::ids::AgentId;
use basemind::comms::singleton::{CommsPaths, comms_socket_path, probe_alive};

const BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// Owns the spawned daemon process so it is always reaped.
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
            // Isolate the daemon's workspace index writes to the same tempdir so a `rescan` RPC ~keep
            // never touches the real XDG cache. ~keep
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

fn agent(a: &str) -> AgentId {
    AgentId::parse(a).expect("agent")
}

/// Run a git command in `cwd`, asserting success. Used to build a real repo the daemon's machine
/// registry can enumerate when a client Hello auto-registers its cwd.
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

/// `thread_start` needs at least two addressing dimensions; one is rejected.
#[tokio::test(flavor = "multi_thread")]
async fn thread_start_enforces_two_of_three_dimensions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut alice = connect(&socket, "agent-alice", &root).await;

    // Only subject → rejected.
    let one = alice.start_thread(Some("solo-topic".to_string()), None, vec![]).await;
    assert!(one.is_err(), "a single dimension must be rejected");

    // subject + members → accepted.
    let ok = alice
        .start_thread(Some("topic".to_string()), None, vec![agent("agent-bob")])
        .await
        .expect("two dimensions accepted");
    assert!(ok.members.contains(&agent("agent-alice")), "creator is a member");
    assert!(ok.members.contains(&agent("agent-bob")), "explicit member added");
}

/// `inbox_ack` advances only the acking agent's cursor; the shared log and other agents are intact.
#[tokio::test(flavor = "multi_thread")]
async fn inbox_ack_advances_cursor_without_touching_shared_log_or_other_agents() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut alice = connect(&socket, "agent-alice", &root).await;
    let thread = alice
        .start_thread(
            Some("team".to_string()),
            None,
            vec![agent("agent-bob"), agent("agent-carol")],
        )
        .await
        .expect("start thread")
        .id;

    let m1 = alice
        .post_message(
            thread.clone(),
            "first".to_string(),
            b"body one".to_vec(),
            vec!["ops".to_string()],
            None,
        )
        .await
        .expect("post m1");
    let _m2 = alice
        .post_message(thread.clone(), "second".to_string(), b"body two".to_vec(), vec![], None)
        .await
        .expect("post m2");

    let mut bob = connect(&socket, "agent-bob", &root).await;
    let mut carol = connect(&socket, "agent-carol", &root).await;

    let (bob_inbox, _unread, _c) = bob
        .read_inbox(None, None, None, 100, false, None)
        .await
        .expect("bob inbox");
    assert_eq!(bob_inbox.len(), 2, "both messages are unread for Bob (he's a member)");
    let first = bob_inbox.iter().find(|sm| sm.meta.id == m1).expect("m1 in inbox");
    assert!(first.meta.ts_micros > 0, "ts_micros surfaced");
    assert_eq!(first.meta.tags, vec!["ops".to_string()], "tags surfaced");
    assert_eq!(first.seq, 1, "seq surfaced (first message in the thread)");

    let (acked, cursors) = bob.ack_inbox(vec![m1.clone()], None, None).await.expect("bob ack m1");
    assert_eq!(acked, 1);
    assert_eq!(cursors, vec![(thread.as_str().to_string(), 1)]);

    let (bob_after, _u, _c) = bob
        .read_inbox(None, None, None, 100, false, None)
        .await
        .expect("bob inbox after");
    assert_eq!(bob_after.len(), 1);
    assert_eq!(bob_after[0].meta.subject, "second");

    let (history, _next) = bob
        .read_history(thread.clone(), None, 100, None)
        .await
        .expect("history");
    assert_eq!(history.len(), 2, "ack must not delete from the shared log");

    let (carol_inbox, _u, _c) = carol
        .read_inbox(None, None, None, 100, false, None)
        .await
        .expect("carol inbox");
    assert_eq!(carol_inbox.len(), 2, "another agent's inbox is untouched");

    let (acked2, cursors2) = carol
        .ack_inbox(vec![], Some(thread.clone()), Some(2))
        .await
        .expect("carol bulk ack");
    assert_eq!(acked2, 0, "bulk mode acks no specific ids");
    assert_eq!(cursors2, vec![(thread.as_str().to_string(), 2)]);
    let (carol_after, _u, _c) = carol
        .read_inbox(None, None, None, 100, false, None)
        .await
        .expect("carol after");
    assert!(carol_after.is_empty(), "to_seq bulk-acked the whole thread");

    let err = bob.ack_inbox(vec![], None, None).await;
    assert!(err.is_err(), "empty ack must be rejected");
}

/// A long-lived client transparently recovers when the daemon dies mid-session.
#[tokio::test(flavor = "multi_thread")]
async fn client_recovers_when_daemon_dies_mid_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();

    let mut daemon = Daemon::start(&comms_dir);
    let paths = CommsPaths {
        comms_dir: comms_dir.clone(),
        socket_path: comms_socket_path(&comms_dir),
    };
    let spawn_dir = comms_dir.clone();
    let mut client = CommsClient::connect_with_respawn(
        &paths,
        AgentId::parse("agent-resilient").expect("agent id"),
        None,
        Some(root.clone()),
        move |_paths: &CommsPaths| {
            Command::new(BIN)
                .args(["comms", "daemon"])
                .env("BASEMIND_COMMS_DIR", &spawn_dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map(|_| ())
        },
    )
    .await
    .expect("connect with respawn");

    let thread = client
        .start_thread(Some("team".to_string()), None, vec![agent("agent-peer")])
        .await
        .expect("start thread")
        .id;
    let first = client
        .post_message(thread.clone(), "before".to_string(), b"first".to_vec(), vec![], None)
        .await
        .expect("post before death");
    assert!(!first.is_empty());

    let _ = daemon.child.kill();
    let _ = daemon.child.wait();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && probe_alive(&paths.socket_path) {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !probe_alive(&paths.socket_path),
        "daemon must be dead before the recovery post"
    );

    let second = client
        .post_message(thread.clone(), "after".to_string(), b"second".to_vec(), vec![], None)
        .await
        .expect("post after death must transparently recover");
    assert!(!second.is_empty());
    assert_ne!(first, second);

    let (history, _next) = client
        .read_history(thread.clone(), None, 100, None)
        .await
        .expect("history");
    assert_eq!(
        history.len(),
        2,
        "both the pre-death and post-recovery messages are in the log"
    );

    let _ = Command::new(BIN)
        .args(["comms", "stop"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .output();
}

/// Discovery is scoped: a non-member whose cwd doesn't match a thread's path glob never sees it in
/// `thread_list`, while members and path-matched agents do. Two members chat and both see the log.
#[tokio::test(flavor = "multi_thread")]
async fn scoped_discovery_and_shared_thread_chat() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut reviewer = connect(&socket, "reviewer", &root).await;
    let mut tester = connect(&socket, "tester", &root).await;
    let mut outsider = connect(&socket, "outsider", &root).await;

    let thread = reviewer
        .start_thread(Some("review".to_string()), None, vec![agent("tester")])
        .await
        .expect("start thread")
        .id;

    // Outsider (not a member, no path) sees nothing.
    let outsider_list = outsider
        .list_threads(None, None, None, false)
        .await
        .expect("outsider list");
    assert!(outsider_list.is_empty(), "a non-member with no path match sees nothing");

    // Reviewer + tester (members) see it.
    let reviewer_list = reviewer
        .list_threads(None, None, None, false)
        .await
        .expect("reviewer list");
    assert_eq!(reviewer_list.len(), 1);
    let tester_list = tester.list_threads(None, None, None, false).await.expect("tester list");
    assert_eq!(tester_list.len(), 1);

    reviewer
        .post_message(
            thread.clone(),
            "from reviewer".to_string(),
            b"hi".to_vec(),
            vec![],
            None,
        )
        .await
        .expect("reviewer post");
    tester
        .post_message(thread.clone(), "from tester".to_string(), b"yo".to_vec(), vec![], None)
        .await
        .expect("tester post");

    let (history, _next) = reviewer
        .read_history(thread.clone(), None, 100, None)
        .await
        .expect("history");
    let senders: Vec<String> = history.iter().map(|m| m.meta.from.as_str().to_string()).collect();
    assert!(
        senders.contains(&"reviewer".to_string()) && senders.contains(&"tester".to_string()),
        "both senders appear in the shared thread: {senders:?}"
    );

    // The outsider was never a member, so the thread's posts never reach its inbox.
    let (outsider_inbox, _u, _c) = outsider
        .read_inbox(None, None, None, 100, false, None)
        .await
        .expect("inbox");
    assert!(
        !outsider_inbox.iter().any(|sm| sm.meta.subject == "from reviewer"),
        "posts must not leak to a non-member"
    );
}

/// Recency-aware `read_history` honours an absolute `since_micros` cutoff deterministically.
#[tokio::test(flavor = "multi_thread")]
async fn read_history_recency_cutoff() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut alice = connect(&socket, "agent-alice", &root).await;
    let thread = alice
        .start_thread(Some("freshness".to_string()), Some("src/**".to_string()), vec![])
        .await
        .expect("start thread")
        .id;

    let before_posts = basemind::comms::model::now_micros();
    for subject in ["first", "second"] {
        alice
            .post_message(
                thread.clone(),
                subject.to_string(),
                format!("body of {subject}").into_bytes(),
                vec![],
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("post {subject}: {e}"));
    }

    const ONE_HOUR_MICROS: i64 = 3_600_000_000;

    let (future, _next) = alice
        .read_history(thread.clone(), None, 100, Some(before_posts + ONE_HOUR_MICROS))
        .await
        .expect("history with future cutoff");
    assert!(future.is_empty(), "a cutoff after both posts elides every message");

    let (all_none, _n) = alice
        .read_history(thread.clone(), None, 100, None)
        .await
        .expect("no cutoff");
    assert_eq!(all_none.len(), 2, "None cutoff returns the whole log");

    let (all_zero, _n) = alice
        .read_history(thread.clone(), None, 100, Some(0))
        .await
        .expect("zero cutoff");
    assert_eq!(all_zero.len(), 2, "a 0 cutoff also returns the whole log");
}

/// A creator can archive a thread, dropping it from active listings; a non-creator member cannot.
#[tokio::test(flavor = "multi_thread")]
async fn creator_archives_thread_and_it_leaves_active_listings() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut alice = connect(&socket, "agent-alice", &root).await;
    let mut bob = connect(&socket, "agent-bob", &root).await;
    let thread = alice
        .start_thread(Some("topic".to_string()), None, vec![agent("agent-bob")])
        .await
        .expect("start thread")
        .id;

    // Bob (member, not creator) cannot archive.
    assert!(
        bob.archive_thread(thread.clone()).await.is_err(),
        "non-creator cannot archive"
    );

    // Alice (creator) can.
    alice.archive_thread(thread.clone()).await.expect("creator archives");

    let active = alice.list_threads(None, None, None, false).await.expect("active list");
    assert!(active.is_empty(), "archived thread drops out of active listing");
    let with_archived = alice.list_threads(None, None, None, true).await.expect("archived list");
    assert_eq!(with_archived.len(), 1);
    assert!(!with_archived[0].active);
}

/// The daemon is the machine's sole fjall writer: a `rescan` RPC indexes a workspace end to end
/// through a real detached daemon, and `accessed_paths` then reports that workspace hot.
#[tokio::test(flavor = "multi_thread")]
async fn rescan_rpc_indexes_a_workspace_and_reports_it_hot() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    std::fs::write(workspace.join("lib.rs"), "pub fn indexed() -> u32 { 7 }\n").expect("write source");

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();
    let mut client = connect(&socket, "agent-scan", &workspace).await;

    let report = client
        .rescan(workspace.clone(), None, false, false)
        .await
        .expect("rescan");
    assert_eq!(report.scanned, 1, "the single source is considered");
    assert_eq!(report.updated, 1, "the single source is newly indexed");

    let hot = client.accessed_paths().await.expect("accessed_paths");
    assert_eq!(hot.len(), 1, "exactly one workspace is hot");
    assert_eq!(hot[0].root, workspace, "the scanned workspace is reported hot");
}

/// Connecting a client with a git-repo cwd auto-registers it in the daemon's machine registry, so
/// `list_workspaces` / `list_worktrees` surface the repo, and a two-claimant worktree race resolves
/// to exactly one holder. The daemon's registry is isolated to the tempdir via `BASEMIND_DATA_HOME`.
#[tokio::test(flavor = "multi_thread")]
async fn machine_registry_auto_registers_and_worktree_claim_is_exclusive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    // The Hello handshake carries the repo root as cwd; the daemon auto-registers it.
    let mut alice = connect(&socket, "agent-alice", &repo).await;

    let workspaces = alice.list_workspaces().await.expect("list workspaces");
    assert_eq!(
        workspaces.len(),
        1,
        "the connecting repo is the only registered workspace"
    );
    let ws = &workspaces[0];
    assert_eq!(ws.root, repo, "the registered root is the repo root");
    let repo_id = ws.repo_id.clone().expect("a git workspace has a repo id");

    let worktrees = alice.list_worktrees(repo_id.clone()).await.expect("list worktrees");
    assert_eq!(worktrees.len(), 1, "one (main) worktree");
    assert_eq!(worktrees[0].name, "(main)", "the main worktree name");
    assert_eq!(worktrees[0].claimed_by, None, "unclaimed at first");

    let branches = alice.list_branches(repo_id.clone()).await.expect("list branches");
    assert_eq!(branches.len(), 1, "one local branch");
    assert_eq!(branches[0].name, "main", "the main branch");

    // Two claimants race for the (main) worktree; exactly one wins.
    let mut bob = connect(&socket, "agent-bob", &repo).await;
    let a_won = alice
        .claim_worktree(repo_id.clone(), "(main)".to_string(), "agent-alice".to_string())
        .await
        .expect("alice claim");
    let b_won = bob
        .claim_worktree(repo_id.clone(), "(main)".to_string(), "agent-bob".to_string())
        .await
        .expect("bob claim");
    assert!(a_won, "alice's first claim wins");
    assert!(!b_won, "bob cannot claim a worktree alice holds");

    // The registry now reflects the holder.
    let worktrees = alice
        .list_worktrees(repo_id.clone())
        .await
        .expect("list worktrees after claim");
    assert_eq!(
        worktrees[0].claimed_by.as_deref(),
        Some("agent-alice"),
        "the (main) worktree is claimed by alice"
    );

    // Alice releases; bob can then claim.
    let released = alice
        .release_worktree(repo_id.clone(), "(main)".to_string(), "agent-alice".to_string())
        .await
        .expect("alice release");
    assert!(released, "alice releases her own claim");
    let b_won_now = bob
        .claim_worktree(repo_id.clone(), "(main)".to_string(), "agent-bob".to_string())
        .await
        .expect("bob claim after release");
    assert!(b_won_now, "bob claims once the worktree is freed");

    // An unknown worktree name is not a hard error; it just fails to claim.
    let unknown = bob
        .claim_worktree(repo_id.clone(), "no-such-worktree".to_string(), "agent-bob".to_string())
        .await
        .expect("claim of unknown worktree returns Ok(false)");
    assert!(!unknown, "an unknown worktree cannot be claimed");
}

/// `inbox_wait` against a REAL detached daemon (not the in-process/UDS-in-test-process setups used
/// elsewhere): B waits up to 10s while A posts to their shared thread within ~100ms. B must wake
/// from the notification push, well under the timeout — proving the subscribe-then-block path
/// works end to end against the actual daemon binary, not just an in-process broker.
#[tokio::test(flavor = "multi_thread")]
async fn inbox_wait_delivers_peer_post_promptly() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut alice = connect(&socket, "agent-alice", &root).await;
    let mut bob = connect(&socket, "agent-bob", &root).await;
    let thread = alice
        .start_thread(Some("wait-team".to_string()), None, vec![agent("agent-bob")])
        .await
        .expect("start thread")
        .id;

    let waiter = tokio::spawn(async move {
        let started = Instant::now();
        let result = bob
            .wait_inbox(None, None, None, None, None, 100, Duration::from_secs(10))
            .await
            .expect("bob wait_inbox");
        (result, started.elapsed())
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    alice
        .post_message(
            thread.clone(),
            "from alice".to_string(),
            b"hi bob".to_vec(),
            vec![],
            None,
        )
        .await
        .expect("alice posts while bob waits");

    let ((timed_out, rows, _unread, _next), elapsed) = tokio::time::timeout(Duration::from_secs(15), waiter)
        .await
        .expect("bob's wait_inbox task did not finish in time")
        .expect("bob's wait_inbox task panicked");

    assert!(!timed_out, "bob must wake from alice's post, not time out");
    assert_eq!(rows.len(), 1, "bob's woken page carries alice's message");
    assert_eq!(rows[0].meta.subject, "from alice");
    assert!(
        elapsed < Duration::from_secs(2),
        "bob should wake on the push within ~100ms, not the 10s timeout: {elapsed:?}"
    );
}

/// Companion to [`inbox_wait_delivers_peer_post_promptly`]: with no peer posting anything, a real
/// daemon's `inbox_wait` still returns `timed_out: true` once the timeout elapses.
#[tokio::test(flavor = "multi_thread")]
async fn inbox_wait_with_no_peer_activity_times_out() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut alice = connect(&socket, "agent-alice", &root).await;
    alice
        .start_thread(Some("lonely".to_string()), None, vec![agent("agent-bob")])
        .await
        .expect("start thread");

    let started = Instant::now();
    let (timed_out, rows, _unread, _next) = alice
        .wait_inbox(None, None, None, None, None, 100, Duration::from_millis(500))
        .await
        .expect("wait_inbox");
    let elapsed = started.elapsed();

    assert!(timed_out, "no peer activity landed; the wait must time out");
    assert!(rows.is_empty(), "a timed-out wait returns no rows");
    assert!(
        elapsed >= Duration::from_millis(500) && elapsed < Duration::from_secs(5),
        "elapsed {elapsed:?} should be close to the 500ms timeout"
    );
}
