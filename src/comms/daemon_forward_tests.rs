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

    let ws = tempfile::tempdir().expect("workspace");
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
