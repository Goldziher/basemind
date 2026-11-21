//! Unit tests for the [`Broker`](super::Broker)'s forwarded-RPC round-trips (rescan). Split out of
//! `daemon_tests.rs` (via a `#[cfg(test)] #[path = "daemon_forward_tests.rs"] mod forward_tests;`
//! declaration) to keep it under the 1000-line `rust-max-lines` cap. `super` here resolves to the
//! `daemon` module.

use super::*;

fn temp_broker() -> (tempfile::TempDir, Arc<Broker>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let agents = super::super::agents_hub::AgentHub::open(dir.path());
    (dir, Arc::new(Broker::new(agents).expect("broker")))
}

/// Run a `git` subcommand in `repo` with a fixed identity, asserting success. Mirrors the fixture
/// setup in `tests/git_smoke.rs`: basemind never writes to a repo, so building the fixture through
/// the real `git` CLI is the most representative way to exercise the history reader.
fn git(repo: &std::path::Path, args: &[&str]) {
    let status = crate::testing::git_command()
        .args(args)
        .current_dir(repo)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

/// A hosted workspace shares the daemon's git-history handle: a `Sync` the daemon ran is visible to
/// it, read straight from the database the daemon holds, through the handle the hosted stack is
/// built with ([`Broker::hosted_history`]).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosted_workspace_reads_the_daemons_git_history() {
    use crate::git_history::proto::{GitHistoryOp, GitHistoryReply};

    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();

    let repo = tempfile::tempdir().expect("workspace");
    let root = repo.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("lib.rs"), "pub fn hosted_history() {}\n").expect("write source");
    git(root, &["add", "lib.rs"]);
    git(root, &["commit", "-qm", "seed hosted history"]);

    // ~keep Sync builds the index in-process (through the daemon's per-repo build lock).
    match broker
        .run_git_history_inproc(root.to_path_buf(), GitHistoryOp::Sync)
        .await
        .expect("in-process sync")
    {
        GitHistoryReply::Synced(_) => {}
        other => panic!("expected Synced, got {other:?}"),
    }

    let history = broker.hosted_history();
    let hosted_root = root.to_path_buf();
    let index = tokio::task::spawn_blocking(move || history(&hosted_root))
        .await
        .expect("join")
        .expect("the hosted workspace gets the daemon's handle")
        .index;
    let commits = index.recent_commits(0, 10, false);
    assert_eq!(
        commits.len(),
        1,
        "the one seeded commit is indexed and read through the shared handle"
    );
    assert_eq!(commits[0].summary, "seed hosted history");
    assert!(
        broker
            .history_lookup(&crate::git_history::shared_history_basemind_dir(root))
            .is_some_and(|entry| std::sync::Arc::ptr_eq(&entry.index, &index)),
        "the hosted workspace shares the daemon's handle, not a second open"
    );
}

/// A draining daemon must REFUSE new scans, not run them as doomed partial passes: the drain
/// token is process-global and never un-trips, and a cancelled pass never advances the
/// coalescing generation — so accepting the request would burn a full tree walk for a result
/// the dispatch layer surfaces as an error anyway.
#[tokio::test]
async fn rescan_is_refused_while_draining() {
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();
    let tx = CancellationToken::new();

    broker.begin_drain().await;

    let ws = tempfile::tempdir().expect("workspace");
    git(ws.path(), &["init", "-q"]);
    std::fs::write(ws.path().join("main.rs"), "pub fn f() {}\n").expect("write source");
    let mut session = Session::default();
    let resp = broker
        .handle(
            CommsRequest::Rescan {
                root: ws.path().to_path_buf(),
                paths: None,
                full: true,
                embed: false,
                verify: false,
            },
            &mut session,
            &tx,
        )
        .await;
    match resp {
        CommsResponse::Error { code, .. } => {
            assert_eq!(code, "rescan_draining", "the refusal is distinguishable from a failure")
        }
        other => panic!("a draining daemon must refuse the rescan, got {other:?}"),
    }
}

#[tokio::test]
async fn rescan_request_indexes_a_workspace_and_surfaces_it_as_accessed() {
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();
    let tx = CancellationToken::new();
    let mut session = Session::default();

    // The workspace-root allow-list (issue #62) only opens a project; `.git/` is invisible to the
    // scanner, so the `scanned` / `updated` counts below are unchanged by the init.
    let ws = tempfile::tempdir().expect("workspace");
    git(ws.path(), &["init", "-q"]);
    std::fs::write(ws.path().join("lib.rs"), "pub fn indexed() -> u32 { 7 }\n").expect("write source");

    let resp = broker
        .handle(
            CommsRequest::Rescan {
                root: ws.path().to_path_buf(),
                paths: None,
                full: false,
                embed: false,
                verify: false,
            },
            &mut session,
            &tx,
        )
        .await;
    match resp {
        CommsResponse::Rescanned { scanned, updated, .. } => {
            assert_eq!(scanned, 1);
            assert_eq!(updated, 1);
        }
        other => panic!("expected Rescanned, got {other:?}"),
    }

    let accessed = broker.handle(CommsRequest::AccessedPaths, &mut session, &tx).await;
    match accessed {
        CommsResponse::Accessed { workspaces } => {
            assert_eq!(workspaces.len(), 1);
            // The pool records the RESOLVED root (what it opens), and a macOS tempdir is reached
            // through a `/var` → `/private/var` symlink.
            assert_eq!(workspaces[0].root, ws.path().canonicalize().expect("canonicalize"));
        }
        other => panic!("expected Accessed, got {other:?}"),
    }
}

/// `run_git_history_inproc` takes a root straight from a client RPC and does NOT go through
/// [`WorkspacePool::get_or_open`](super::workspace_pool::WorkspacePool), so it needs its own trip
/// through the root guard: `GitHistoryOp::Sync` walks the whole history and creates
/// `shared_history_basemind_dir(root)`, which would mint cache state for a root every other consumer
/// refuses. Only reachable over the 0600 socket, so this is an invariant, not a privilege boundary —
/// but an invariant the other fixes are pointless without.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_history_refuses_a_non_project_root_without_minting_cache_state() {
    use crate::git_history::{GitHistoryError, proto::GitHistoryOp};

    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();

    let plain = tempfile::tempdir().expect("workspace");
    let root = plain.path().canonicalize().expect("canonicalize");
    std::fs::write(root.join("notes.md"), "not a repo\n").expect("write file");

    match broker.run_git_history_inproc(root.clone(), GitHistoryOp::Sync).await {
        Err(GitHistoryError::RootRefused(message)) => assert!(
            message.contains("basemind init"),
            "the operator guidance is carried through: {message}"
        ),
        other => panic!("expected RootRefused, got {other:?}"),
    }
    assert!(
        !crate::git_history::shared_history_basemind_dir(&root).exists(),
        "a refused root must not have history cache state created for it"
    );
}

/// A `basemind rescan` forwarded by the CLI is served over a plain link, which does not make the
/// daemon refuse `Stop`. A takeover's `Stop` accepted then would cancel the scan half way, so the
/// rescan counts as work for as long as it runs, from before its first wait.
#[tokio::test]
async fn stop_is_refused_while_a_forwarded_rescan_is_running() {
    let (_dir, broker) = temp_broker();
    let tx = CancellationToken::new();
    let ws = tempfile::tempdir().expect("workspace");
    let gate = broker.workspaces.blob_gc_gate();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (held, holding) = std::sync::mpsc::channel::<()>();
    let sweep = std::thread::spawn(move || {
        let _sweeping = gate.try_sweep(Duration::ZERO).expect("nothing is scanning");
        held.send(()).expect("signal the sweep holds the gate");
        let _ = released.recv();
    });
    holding.recv().expect("the sweep holds the gate");
    let mut scanning = Session::default();
    let rescan = broker.handle(
        CommsRequest::Rescan {
            root: ws.path().to_path_buf(),
            paths: None,
            full: false,
            embed: false,
            verify: false,
        },
        &mut scanning,
        &tx,
    );
    tokio::pin!(rescan);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut rescan)
            .await
            .is_err(),
        "the rescan waits for the blob sweep to finish"
    );

    let mut stopping = Session::default();
    let refused = broker.handle(CommsRequest::Stop, &mut stopping, &tx).await;
    assert!(
        matches!(refused, CommsResponse::Error { ref code, ref message } if code == "daemon_busy" && message.contains("work=1")),
        "a stop must not cancel a rescan in progress: {refused:?}"
    );
    assert!(!broker.scan_cancel.is_cancelled());

    drop(release);
    sweep.join().expect("the sweep thread");
    rescan.await;
    assert_eq!(
        broker.handle(CommsRequest::Stop, &mut stopping, &tx).await,
        CommsResponse::Ok,
        "a finished rescan no longer holds the daemon"
    );
}

/// A hosted workspace's git-history sync keeps the daemon up while it runs. It is started by the
/// stack with no client link waiting on it, so without a work pin the idle reaper could drain the
/// daemon in the middle of a first build's write to `git-history.fjall/`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosted_git_history_sync_pins_the_daemon() {
    const POLL: std::time::Duration = std::time::Duration::from_millis(10);

    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();
    let repo = tempfile::tempdir().expect("workspace");
    let root = repo.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("lib.rs"), "pub fn pinned() {}\n").expect("write source");
    git(root, &["add", "lib.rs"]);
    git(root, &["commit", "-qm", "seed"]);

    let history = broker.hosted_history();
    let open_root = root.to_path_buf();
    let index = tokio::task::spawn_blocking(move || history(&open_root))
        .await
        .expect("join")
        .expect("the daemon opens the history index")
        .index;
    // Held on a thread of its own: the guard must not live across this test's awaits. ~keep
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let building = std::thread::spawn(move || {
        let _held = index.hold_build_lock();
        locked_tx.send(()).expect("test alive");
        let _ = release_rx.recv();
    });
    locked_rx.recv().expect("the build lock is held");

    let (served, conn) = broker
        .workspaces
        .serve_conn(root, broker.hosted_history())
        .await
        .expect("hosted stack");
    // The stack requests its sync, and takes the pin, before `serve_conn` returns; the build lock
    // keeps the sync from ending.
    let syncing = Arc::clone(&served.workspace.history_syncing);
    assert!(
        syncing.load(Ordering::Acquire),
        "the stack's sync waits on the build lock"
    );
    assert_eq!(broker.work_inflight(), 1, "the waiting sync pins the daemon");

    drop(release_tx);
    building.join().expect("lock holder");
    // No deadline: the sync's walk and compaction share a process-wide pool and the disk with every
    // other test, so how long it takes says nothing about the pin. ~keep
    while syncing.load(Ordering::Acquire) {
        tokio::time::sleep(POLL).await;
    }
    assert_eq!(
        broker.work_inflight(),
        0,
        "the pin is released by the time the sync is seen finished"
    );
    drop((served, conn));
}
