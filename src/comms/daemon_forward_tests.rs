//! Unit tests for the [`Broker`](super::Broker)'s forwarded-RPC round-trips (rescan). Split out of
//! `daemon_tests.rs` (via a `#[cfg(test)] #[path = "daemon_forward_tests.rs"] mod forward_tests;`
//! declaration) to keep it under the 1000-line `rust-max-lines` cap. `super` here resolves to the
//! `daemon` module.

use super::*;

fn temp_broker() -> (tempfile::TempDir, Arc<Broker>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(CommsStore::open(dir.path()).expect("store"));
    (dir, Arc::new(Broker::new(store)))
}

/// Run a `git` subcommand in `repo` with a fixed identity, asserting success. Mirrors the fixture
/// setup in `tests/git_smoke.rs`: basemind never writes to a repo, so building the fixture through
/// the real `git` CLI is the most representative way to exercise the history reader.
fn git(repo: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
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

/// The daemon-hosted git-history seam: a hosted connection runs its history ops IN-PROCESS through
/// [`Broker::run_git_history_inproc`] and the [`HistoryHost`](crate::git_history::remote::HistoryHost)
/// trait, instead of dialing the daemon over its own socket. This asserts a `Sync` builds the index
/// and a subsequent read returns the committed history — through the trait object the hosted stack
/// actually holds, so the whole wiring (not just the inherent method) is covered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_git_history_runs_in_process() {
    use crate::git_history::proto::{GitHistoryOp, GitHistoryReply};
    use crate::git_history::remote::HistoryHost;

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

    // ~keep Read back through the HistoryHost trait object — exactly what a hosted stack holds.
    let host: Arc<dyn HistoryHost> = Arc::clone(&broker) as Arc<dyn HistoryHost>;
    let reply = host
        .run_history(
            root.to_path_buf(),
            GitHistoryOp::RecentCommits {
                skip: 0,
                take: 10,
                include_files: false,
            },
        )
        .await
        .expect("in-process recent-commits");
    match reply {
        GitHistoryReply::Commits(commits) => {
            assert_eq!(
                commits.len(),
                1,
                "the one seeded commit is indexed and read back in-process"
            );
            assert_eq!(commits[0].summary, "seed hosted history");
        }
        other => panic!("expected Commits, got {other:?}"),
    }
}

/// A draining daemon must REFUSE new scans, not run them as doomed partial passes: the drain
/// token is process-global and never un-trips, and a cancelled pass never advances the
/// coalescing generation — so accepting the request would burn a full tree walk for a result
/// the dispatch layer surfaces as an error anyway.
#[tokio::test]
async fn rescan_is_refused_while_draining() {
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);

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
    let (tx, _rx) = mpsc::channel(8);
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
            assert_eq!(workspaces[0].root, ws.path());
        }
        other => panic!("expected Accessed, got {other:?}"),
    }
}
