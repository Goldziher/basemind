//! The git-history index under the daemon-as-sole-writer model.
//!
//! On a `comms` build `basemind serve` opens its store read-only and forwards every write to the
//! machine daemon. The git-history index (`.basemind/git-history.fjall/`) is a fjall database, and
//! fjall takes an EXCLUSIVE process lock on its directory — so exactly one process may hold it.
//! These tests pin the contract that falls out of that:
//!
//! * a `daemon_writer` serve NEVER builds (or opens) the index in-process — the ~4 GB, minutes-long
//!   walk on a deep monorepo must not land in the process an agent is actively querying, and N
//!   sessions must not each run it;
//! * the daemon builds it on request, exactly once, and answers the serve's history queries from it,
//!   so git tools stay index-backed instead of silently degrading to the live walk.
//!
//! Hermetic: `init_isolated_cache` redirects `BASEMIND_DATA_HOME` + `BASEMIND_COMMS_DIR` to a
//! per-process tempdir, so the daemon, its socket, and every index live there — never in the
//! machine-global cache.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use basemind::comms::client::CommsClient;
use basemind::comms::ids::AgentId;
use basemind::comms::singleton::{CommsPaths, comms_socket_path, probe_alive};
use basemind::git_history::proto::{GitHistoryOp, GitHistoryReply, SyncOutcome};
use rmcp::ServiceExt;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use tempfile::TempDir;
use tokio::process::Command as AsyncCommand;

/// A commit-message body token that exists ONLY in a commit body — never in a summary, an author
/// name, or a file. The live-walk fallback in `search_git_history` searches summaries + authors of a
/// bounded window and never loads bodies, so a hit on this token can ONLY come from the git-history
/// index's full-text posting lists. It is the observable that separates "index-backed" from
/// "degraded to the live walk".
const BODY_ONLY_TOKEN: &str = "zzqqxindexonlytoken";

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

/// A three-commit repo whose middle commit carries [`BODY_ONLY_TOKEN`] in its message BODY.
fn build_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").expect("write a.rs");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);

    std::fs::write(root.join("b.rs"), b"pub fn beta() {}\n").expect("write b.rs");
    git(root, &["add", "."]);
    git(
        root,
        &[
            "commit",
            "-qm",
            "add beta",
            "-m",
            &format!("body line {BODY_ONLY_TOKEN} here"),
        ],
    );

    std::fs::write(root.join("a.rs"), b"pub fn alpha() -> u32 { 1 }\n").expect("rewrite a.rs");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "widen alpha"]);
    dir
}

/// Spawn a real `basemind serve` (a `comms` build ⇒ `read_only` + `daemon_writer`) and handshake.
async fn spawn_server(root: &Path) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    ().serve(transport).await.expect("rmcp handshake")
}

/// The isolated comms dir `init_isolated_cache` pointed this process — and every child it spawns —
/// at, so the daemon, its socket, and every index stay in a per-process tempdir.
fn comms_paths() -> CommsPaths {
    let comms_dir = std::path::PathBuf::from(std::env::var("BASEMIND_COMMS_DIR").expect("isolated comms dir"));
    CommsPaths {
        socket_path: comms_socket_path(&comms_dir),
        comms_dir,
    }
}

/// Bring up a REAL `basemind comms daemon` and wait for it to answer.
///
/// Never call `CommsClient::ensure_and_connect` from a TEST: its spawn strategy execs
/// `current_exe()`, which here is the test harness binary — libtest reads `comms daemon` as a filter
/// argument and re-runs the whole suite, which spawns another "daemon", and so on. Spawn the real
/// binary explicitly instead. Idempotent: a second daemon on the same socket loses the bind race and
/// exits, so racing tests converge on one.
// The daemon is a per-process singleton shared by every test in this binary and outlives any one of
// them, so it is deliberately never `wait()`ed on: it self-terminates when idle.
#[allow(clippy::zombie_processes)]
fn ensure_real_daemon() {
    let paths = comms_paths();
    if probe_alive(&paths.socket_path) {
        return;
    }
    let _child = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["comms", "daemon"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn comms daemon");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if probe_alive(&paths.socket_path) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("comms daemon did not become ready");
}

/// Connect to the daemon serving this process's isolated cache.
async fn connect(root: &Path, agent: &str) -> CommsClient {
    ensure_real_daemon();
    CommsClient::connect(
        &comms_paths(),
        AgentId::parse(agent).expect("agent id"),
        None,
        Some(root.to_path_buf()),
    )
    .await
    .unwrap_or_else(|e| panic!("connect {agent}: {e}"))
}

async fn sync(client: &mut CommsClient, root: &Path) -> SyncOutcome {
    match client
        .git_history(root.to_path_buf(), GitHistoryOp::Sync)
        .await
        .expect("git_history sync")
    {
        GitHistoryReply::Synced(outcome) => outcome,
        other => panic!("expected Synced, got {other:?}"),
    }
}

/// The serve process must not hold `git-history.fjall/`. Fjall's directory lock is exclusive, so a
/// serve that built the index in-process (the pre-fix behavior of a writable serve) would still be
/// HOLDING the lock while it serves — and the daemon's own sync would then fail with `Locked`.
///
/// So: with a live `daemon_writer` serve attached to the repo, an independent client asks the daemon
/// to sync. Success is only possible if the serve process opened nothing. That the daemon can also
/// READ the index back proves the build landed where the serve's queries look for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_writer_serve_holds_no_git_history_lock_so_the_daemon_can_build() {
    let dir = build_repo();
    let root = dir.path();

    let service = spawn_server(root).await;

    let mut client = connect(root, "gh-lock-probe").await;
    let outcome = sync(&mut client, root).await;
    assert!(
        matches!(
            outcome,
            SyncOutcome::FullRebuild { .. } | SyncOutcome::Incremental { .. } | SyncOutcome::Fresh
        ),
        "the daemon must be able to open + sync the index while a serve session is live \
         (a serve holding the fjall lock would fail this): {outcome:?}"
    );

    let head = match client
        .git_history(root.to_path_buf(), GitHistoryOp::IndexedHead)
        .await
        .expect("indexed head")
    {
        GitHistoryReply::IndexedHead(head) => head,
        other => panic!("expected IndexedHead, got {other:?}"),
    };
    assert_eq!(
        head.as_deref(),
        Some(
            String::from_utf8_lossy(
                &Command::new("git")
                    .args(["rev-parse", "HEAD"])
                    .current_dir(root)
                    .output()
                    .expect("git rev-parse")
                    .stdout
            )
            .trim()
        ),
        "the daemon's index is synced to the repo's HEAD"
    );

    let _ = service.cancel().await;
}

/// N sessions on one repo must cause ONE history walk, not N. The daemon is the serialization point:
/// it takes a per-repo build lock, and `builder::sync` is freshness-checked (`last_indexed_head ==
/// HEAD` ⇒ `Fresh`), so exactly one racer rebuilds and the rest observe `Fresh`.
///
/// Without the daemon owning the build this is unenforceable — each serve would walk history itself,
/// and on a deep monorepo that is N × (minutes, multi-GB).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_cause_exactly_one_history_build() {
    const RACERS: usize = 4;
    let dir = build_repo();
    let root = dir.path();

    let mut clients = Vec::with_capacity(RACERS);
    for c in 0..RACERS {
        clients.push(connect(root, &format!("gh-racer-{c}")).await);
    }
    let mut tasks = Vec::with_capacity(RACERS);
    for mut client in clients {
        let root = root.to_path_buf();
        tasks.push(tokio::spawn(async move { sync(&mut client, &root).await }));
    }
    let mut outcomes = Vec::with_capacity(RACERS);
    for task in tasks {
        outcomes.push(task.await.expect("sync task"));
    }

    let builds = outcomes
        .iter()
        .filter(|o| matches!(o, SyncOutcome::FullRebuild { .. }))
        .count();
    assert_eq!(
        builds, 1,
        "exactly one of {RACERS} concurrent syncs may walk history; the rest see Fresh: {outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .all(|o| !matches!(o, SyncOutcome::Incremental { added } if *added > 0)),
        "no racer may append commits the winner already indexed: {outcomes:?}"
    );
}
