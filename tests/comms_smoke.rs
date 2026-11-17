//! End-to-end smoke test of agent comms v2 (ADR-0021) against a REAL detached daemon.
//!
//! The daemon is the actual `basemind comms daemon` process (spawned via the test binary's
//! `CARGO_BIN_EXE_basemind`), isolated to a tempdir via `BASEMIND_COMMS_DIR`. We drive the library
//! [`CommsClient`] directly over protocol 7: posting, reading that marks read, the daemon-side
//! wait with and without a thread filter, membership refusals, recovery when the daemon dies,
//! and the legacy store left untouched.

#![cfg(feature = "comms")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use basemind::comms::client::{ClientHello, CommsClient, CommsClientError};
use basemind::comms::singleton::{CommsPaths, comms_socket_path, generation_dir, probe_alive};
use basemind::comms::v2::{
    Actor, CommsError, InboxBody, InboxSelect, PostBody, PostReply, PostTarget, Principal, ThreadAction, ThreadId,
    ThreadQuery, ThreadUpdate, WaitOutcome,
};

const BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// Owns the spawned daemon process so it is always reaped.
struct Daemon {
    child: Child,
    comms_dir: PathBuf,
    socket: PathBuf,
}

impl Daemon {
    fn start(comms_dir: &Path) -> Self {
        let socket = comms_socket_path(&generation_dir(comms_dir));
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
        // A cold daemon opens its fjall store (many F_FULLFSYNC calls) before it accepts, which stalls for tens of seconds when the disk is contended, so wait generously.
        let deadline = Instant::now() + Duration::from_secs(180);
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
    let hello = ClientHello::agent(Principal::parse(agent).expect("principal"), root.to_path_buf(), None);
    CommsClient::connect(&paths, hello)
        .await
        .unwrap_or_else(|e| panic!("connect {agent}: {e}"))
}

fn actor(name: &str) -> Actor {
    Actor::parse(name).expect("actor")
}

async fn start(client: &mut CommsClient, subject: &str, to: &[&str]) -> PostReply {
    let body = PostBody {
        target: PostTarget::NewThread {
            subject: subject.to_string(),
            to: to.iter().map(|name| actor(name)).collect(),
            paths: Vec::new(),
        },
        body: "first".to_string(),
    };
    client.agent_post(None, body).await.expect("start a thread")
}

fn into(thread: &ThreadId, body: &str) -> PostBody {
    PostBody {
        target: PostTarget::Thread(thread.clone()),
        body: body.to_string(),
    }
}

fn history_of(thread: &ThreadId) -> InboxBody {
    InboxBody {
        select: InboxSelect::Thread(thread.clone()),
        unread: false,
        ..InboxBody::default()
    }
}

fn refusal(result: Result<impl std::fmt::Debug, CommsClientError>) -> CommsError {
    match result {
        Err(CommsClientError::Agent(error)) => error,
        other => panic!("expected a refusal, got {other:?}"),
    }
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

/// Two agents of one repository: an open thread is found by query and joined; reading marks read,
/// history still returns everything, and the poster's own post is never unread for it.
#[tokio::test(flavor = "multi_thread")]
async fn agents_find_join_post_and_read_on_a_real_daemon() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();
    let mut alice = connect(&socket, "agent-alice", &root).await;
    let mut bob = connect(&socket, "agent-bob", &root).await;

    let created = start(&mut alice, "release plan", &[]).await;
    assert!(created.created);
    let found = bob
        .agent_threads(
            None,
            ThreadQuery::List(basemind::comms::v2::ThreadFilter {
                query: Some("release".to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("discover");
    assert_eq!(
        found.threads.len(),
        1,
        "an open thread of the repository is discoverable"
    );
    assert!(!found.threads[0].member);
    bob.agent_thread_update(
        None,
        ThreadUpdate {
            thread: created.thread.clone(),
            action: ThreadAction::Join,
        },
    )
    .await
    .expect("join");

    alice
        .agent_post(None, into(&created.thread, "second"))
        .await
        .expect("post");
    let read = bob.agent_inbox(None, InboxBody::default(), 0).await.expect("read");
    let bodies: Vec<&str> = read.messages.iter().map(|m| m.body.as_str()).collect();
    assert_eq!(bodies, ["second"], "a joiner reads from the head as of joining");
    let again = bob.agent_inbox(None, InboxBody::default(), 0).await.expect("read");
    assert!(again.messages.is_empty(), "reading marked it read");
    let history = bob
        .agent_inbox(None, history_of(&created.thread), 0)
        .await
        .expect("history");
    assert_eq!(history.messages.len(), 2, "history keeps every message");
    let own = alice.agent_inbox(None, InboxBody::default(), 0).await.expect("read");
    assert!(own.messages.is_empty(), "own posts are never unread");
}

/// A non-member can neither post into nor read an invitation-only thread.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_member_is_refused_post_and_history() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();
    let mut alice = connect(&socket, "agent-alice", &root).await;
    let mut eve = connect(&socket, "agent-eve", &root).await;

    let created = start(&mut alice, "private", &["agent-bob"]).await;
    let posted = eve.agent_post(None, into(&created.thread, "let me in")).await;
    assert!(matches!(refusal(posted), CommsError::NotMember { .. }));
    let read = eve.agent_inbox(None, history_of(&created.thread), 0).await;
    assert!(matches!(refusal(read), CommsError::NotMember { .. }));
    let joined = eve
        .agent_thread_update(
            None,
            ThreadUpdate {
                thread: created.thread.clone(),
                action: ThreadAction::Join,
            },
        )
        .await;
    assert!(matches!(refusal(joined), CommsError::ThreadClosed { .. }));
}

/// A client transparently recovers when the daemon dies mid-session, and what was posted before
/// is still there: the store is on disk, not in the daemon.
#[tokio::test(flavor = "multi_thread")]
async fn client_recovers_when_daemon_dies_mid_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();

    let mut daemon = Daemon::start(&comms_dir);
    let paths = CommsPaths {
        comms_dir: generation_dir(&comms_dir),
        socket_path: comms_socket_path(&generation_dir(&comms_dir)),
    };
    let spawn_dir = comms_dir.clone();
    let hello = ClientHello::agent(
        Principal::parse("agent-resilient").expect("principal"),
        root.clone(),
        None,
    );
    let mut client = CommsClient::connect_with_respawn(&paths, hello, move |_paths: &CommsPaths| {
        Command::new(BIN)
            .args(["comms", "daemon"])
            .env("BASEMIND_COMMS_DIR", &spawn_dir)
            .env("BASEMIND_DATA_HOME", &spawn_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
    })
    .await
    .expect("connect with respawn");

    let first = start(&mut client, "team", &["agent-peer"]).await;

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
        .agent_post(None, into(&first.thread, "after"))
        .await
        .expect("post after death must transparently recover");
    assert_ne!(first.id, second.id);
    let history = client
        .agent_inbox(None, history_of(&first.thread), 0)
        .await
        .expect("history");
    assert_eq!(history.messages.len(), 2, "both posts are in the thread");

    let _ = Command::new(BIN)
        .args(["comms", "stop"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .output();
}

/// The store of earlier releases in the generation directory is not read, renamed or imported:
/// the daemon serves agents from `agents.fjall` beside it.
#[tokio::test(flavor = "multi_thread")]
async fn the_legacy_store_is_left_where_it_is() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let legacy = generation_dir(&comms_dir).join("store.fjall");
    std::fs::create_dir_all(&legacy).expect("legacy store dir");
    std::fs::write(legacy.join("marker"), b"v1").expect("legacy marker");

    let daemon = Daemon::start(&comms_dir);
    let mut alice = connect(daemon.socket(), "agent-alice", tmp.path()).await;
    start(&mut alice, "fresh", &[]).await;
    drop(daemon);

    assert_eq!(std::fs::read(legacy.join("marker")).expect("legacy marker"), b"v1");
    assert!(generation_dir(&comms_dir).join("agents.fjall").is_dir());
}

/// The daemon is the machine's sole fjall writer: a `rescan` RPC indexes a workspace end to end
/// through a real detached daemon, and `accessed_paths` then reports that workspace hot.
#[tokio::test(flavor = "multi_thread")]
async fn rescan_rpc_indexes_a_workspace_and_reports_it_hot() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    // The workspace-root allow-list refuses a directory that is neither a git repo nor carries
    // `basemind.toml` (issue #62), and the daemon's pool is where it bites. `git init` rather than a
    // marker file because `.git/` is invisible to the scanner, so `scanned == 1` still holds. ~keep
    git(&["init", "--quiet"], &workspace);
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
    // The pool reports the RESOLVED root — the path the guard approved and the store actually
    // opened — and on macOS a tempdir is reached through the `/var` → `/private/var` symlink.
    let resolved = workspace.canonicalize().expect("canonicalize workspace");
    assert_eq!(hot[0].root, resolved, "the scanned workspace is reported hot");
}

/// Connecting a client with a git-repo cwd auto-registers it in the daemon's machine registry, so
/// `list_workspaces` / `list_worktrees` / `list_branches` surface the repo. The daemon's registry is
/// isolated to the tempdir via `BASEMIND_DATA_HOME`.
#[tokio::test(flavor = "multi_thread")]
async fn machine_registry_auto_registers_a_connecting_repo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

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

    let branches = alice.list_branches(repo_id.clone()).await.expect("list branches");
    assert_eq!(branches.len(), 1, "one local branch");
    assert_eq!(branches[0].name, "main", "the main branch");
}

/// The daemon owns the wait: B waits up to 20 s while A posts within moments, and B wakes on the
/// post, well under the bound, with the message marked read.
#[tokio::test(flavor = "multi_thread")]
async fn a_wait_ends_promptly_on_a_peer_post() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();
    let mut alice = connect(&socket, "agent-alice", &root).await;
    let mut bob = connect(&socket, "agent-bob", &root).await;
    let thread = start(&mut alice, "waits", &["agent-bob"]).await.thread;
    bob.agent_inbox(None, InboxBody::default(), 0)
        .await
        .expect("bob reads the first post");

    let waiting = tokio::spawn(async move {
        let started = Instant::now();
        let reply = bob.agent_inbox(None, InboxBody::default(), 20).await;
        (reply, started.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    alice.agent_post(None, into(&thread, "ping")).await.expect("post");

    let (reply, waited) = waiting.await.expect("waiter");
    let reply = reply.expect("wait");
    assert_eq!(reply.wait, WaitOutcome::Arrived);
    assert_eq!(reply.messages.len(), 1);
    assert!(
        waited < Duration::from_secs(10),
        "woke on the post, not the bound: {waited:?}"
    );
}

/// A wait limited to one thread is not ended by a post to another.
#[tokio::test(flavor = "multi_thread")]
async fn a_wait_with_a_thread_filter_ignores_other_threads() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();
    let mut alice = connect(&socket, "agent-alice", &root).await;
    let mut bob = connect(&socket, "agent-bob", &root).await;
    let watched = start(&mut alice, "watched", &["agent-bob"]).await.thread;
    let other = start(&mut alice, "other", &["agent-bob"]).await.thread;
    bob.agent_inbox(None, InboxBody::default(), 0)
        .await
        .expect("bob reads both first posts");

    let filter = InboxBody {
        select: InboxSelect::Thread(watched.clone()),
        ..InboxBody::default()
    };
    let waiting = tokio::spawn(async move { bob.agent_inbox(None, filter, 20).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    alice.agent_post(None, into(&other, "elsewhere")).await.expect("post");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !waiting.is_finished(),
        "a post to another thread ends no wait on this one"
    );
    alice.agent_post(None, into(&watched, "here")).await.expect("post");

    let reply = waiting.await.expect("waiter").expect("wait");
    let bodies: Vec<&str> = reply.messages.iter().map(|m| m.body.as_str()).collect();
    assert_eq!(bodies, ["here"]);
}

/// With nobody posting, a wait ends at its bound with nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_wait_with_no_peer_activity_times_out() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let root = tmp.path().to_path_buf();
    let daemon = Daemon::start(&comms_dir);
    let mut bob = connect(daemon.socket(), "agent-bob", &root).await;
    bob.agent_inbox(None, InboxBody::default(), 0)
        .await
        .expect("a first read, once the store has opened");

    let started = Instant::now();
    let reply = bob.agent_inbox(None, InboxBody::default(), 1).await.expect("wait");
    assert_eq!(reply.wait, WaitOutcome::TimedOut);
    assert!(reply.messages.is_empty());
    assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
}

/// A cold daemon whose store open is artificially slow still answers `Ping` immediately, while a
/// store-dependent request (the client `Hello`) waits for readiness and then succeeds.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn daemon_answers_ping_while_the_store_is_still_opening() {
    const OPEN_DELAY: Duration = Duration::from_secs(8);
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let socket = comms_socket_path(&generation_dir(&comms_dir));
    let child = Command::new(BIN)
        .args(["comms", "daemon"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .env("BASEMIND_DATA_HOME", &comms_dir)
        .env(
            "BASEMIND_TEST_COMMS_STORE_OPEN_DELAY_MS",
            OPEN_DELAY.as_millis().to_string(),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn comms daemon");
    let daemon = Daemon {
        child,
        comms_dir: comms_dir.clone(),
        socket: socket.clone(),
    };

    // Process start-up (loading a large binary) can take tens of seconds on a busy machine, so the
    // clock starts when the daemon binds its socket, which is when the store open begins.
    let deadline = Instant::now() + Duration::from_secs(180);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "daemon never bound its socket");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let bound = Instant::now();
    while !probe_alive(&socket) {
        assert!(Instant::now() < deadline, "daemon never answered Ping");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        bound.elapsed() < OPEN_DELAY / 2,
        "Ping must be answered while the {OPEN_DELAY:?} store open is still running, took {:?}",
        bound.elapsed()
    );

    // The agent store opens on a thread of its own: the handshake is served at once, and an agent
    // request waits for the open, then completes.
    let mut client = connect(&socket, "agent-early", tmp.path()).await;
    assert!(
        bound.elapsed() < OPEN_DELAY - Duration::from_secs(1),
        "Hello must not wait for the agent store, took {:?}",
        bound.elapsed()
    );
    start(&mut client, "early", &[]).await;
    assert!(
        bound.elapsed() >= OPEN_DELAY - Duration::from_secs(1),
        "an agent request must not be served before the store is open, took {:?}",
        bound.elapsed()
    );
    drop(daemon);
}
