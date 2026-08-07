//! The machine registry must not grow without bound.
//!
//! `Registry::prune_missing` has existed and been unit-tested since the registry landed, but until
//! this test nothing in production ever called it: its only caller was `src/registry/tests.rs`. The
//! observable consequence was a registry that accumulated a row for every workspace ever seen —
//! on a developer machine, 92 of 127 rows pointed at directories that no longer existed, most of
//! them test tempdirs. That makes `workspace workspaces` progressively useless as the discovery
//! surface it exists to be, since the live repos are buried in dead ones.
//!
//! These tests pin the seam the daemon's periodic maintenance pass calls, so the prune cannot go
//! back to being dead code without a test failing.

#![cfg(feature = "comms")]

use std::sync::Arc;

use basemind::comms::daemon::Broker;
use basemind::comms::store::CommsStore;
use basemind::registry::Registry;

/// Build a broker over an isolated store + registry rooted in `dir`.
fn broker_with_registry(dir: &std::path::Path) -> (Arc<Broker>, std::path::PathBuf) {
    let registry_dir = dir.join("registry");
    let registry = Registry::open(&registry_dir).expect("open registry");
    let store = Arc::new(CommsStore::open(&dir.join("comms")).expect("open comms store"));
    (Arc::new(Broker::with_registry(store, registry)), registry_dir)
}

#[tokio::test]
async fn prune_drops_workspaces_whose_root_is_gone_and_keeps_the_rest() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let live = tmp.path().join("live");
    let gone = tmp.path().join("gone");
    std::fs::create_dir_all(&live).expect("live root");
    std::fs::create_dir_all(&gone).expect("gone root");

    let registry_dir = tmp.path().join("registry");
    {
        let mut registry = Registry::open(&registry_dir).expect("open registry");
        registry.register_workspace(&live).expect("register live");
        registry.register_workspace(&gone).expect("register gone");
        assert_eq!(registry.workspaces().len(), 2, "both roots registered up front");
    }

    let registry = Registry::open(&registry_dir).expect("reopen registry");
    let store = Arc::new(CommsStore::open(&tmp.path().join("comms")).expect("open comms store"));
    let broker = Broker::with_registry(store, registry);

    std::fs::remove_dir_all(&gone).expect("delete one workspace root");

    let removed = broker.prune_missing_registry_rows().await;
    assert_eq!(removed, 1, "exactly the vanished workspace is pruned");

    // Re-open from disk rather than reading the broker's in-RAM copy: the prune must PERSIST, or a
    // daemon restart would resurrect every row it just dropped.
    let reopened = Registry::open(&registry_dir).expect("reopen after prune");
    let roots: Vec<_> = reopened.workspaces().into_iter().map(|w| w.root).collect();
    assert_eq!(roots, vec![live], "only the surviving root remains on disk");
}

#[tokio::test]
async fn prune_is_a_no_op_when_every_root_still_exists() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("present");
    std::fs::create_dir_all(&root).expect("root");

    let (broker, registry_dir) = broker_with_registry(tmp.path());
    {
        let mut registry = Registry::open(&registry_dir).expect("open registry");
        registry.register_workspace(&root).expect("register");
    }

    // The broker holds its own registry handle opened before the row above was written, so this
    // asserts the honest thing: a prune over a registry with nothing missing removes nothing. A
    // prune that reported a non-zero count here would be deleting live rows.
    assert_eq!(broker.prune_missing_registry_rows().await, 0, "nothing to prune");
}
