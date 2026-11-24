use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use crate::engine::workspace::{OpenOptions, Workspace};
use crate::store::{Store, VIEW_WORKING};

/// Long enough for a thread that is not blocked to finish, short enough that a blocked one fails
/// the test quickly.
const PROMPT: Duration = Duration::from_secs(10);

fn workspace_over(store: Store, root: &Path, git_cache: &Path) -> Workspace {
    Workspace::new(
        store,
        root.to_path_buf(),
        Arc::new(crate::config::Config::with_defaults()),
        None,
        Arc::new(crate::git_cache::GitCache::open(git_cache, 1, false).expect("git cache")),
        None,
        OpenOptions {
            background: false,
            read_only: false,
            forwards_writes: false,
            lazy_cache: false,
            history_sync: None,
            #[cfg(feature = "memory")]
            memory_owner: |_| "owner".to_string(),
        },
    )
}

/// A read-only store borrowing its index per call, as a daemon-hosted workspace holds, over a root
/// that was opened once as a writer so its view exists.
fn hosted_store(root: &Path) -> Store {
    crate::store::init_isolated_cache();
    drop(Store::open(root, VIEW_WORKING).expect("open"));
    let mut store = Store::open_read_only_no_index(root, VIEW_WORKING).expect("read-only store");
    store.index_source = Some(Arc::new(|| None));
    store
}

/// A store told apart from the one it replaces by a field a reader can see.
fn marked(mut store: Store) -> Store {
    store.reset_stale_schema = true;
    store
}

#[test]
fn a_publish_and_the_reads_after_it_do_not_wait_on_a_read_in_flight() {
    let (tmp, git_cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let ws = Arc::new(workspace_over(hosted_store(tmp.path()), tmp.path(), git_cache.path()));
    let long_read = ws.store_read();
    assert!(!long_read.reset_stale_schema);

    let (done, finished) = mpsc::channel();
    let publisher = {
        let (ws, root) = (Arc::clone(&ws), tmp.path().to_path_buf());
        std::thread::spawn(move || {
            ws.replace_store(marked(hosted_store(&root)));
            let after = ws.store_read().reset_stale_schema;
            done.send(after).expect("report");
        })
    };

    let after = finished
        .recv_timeout(PROMPT)
        .expect("the publish and the read after it finished while a read was still in flight");
    assert!(after, "a read begun after the publish serves the published store");
    assert!(
        !long_read.reset_stale_schema,
        "the read in flight keeps the store it began with"
    );
    publisher.join().expect("publisher");
}

#[test]
fn a_hosted_workspace_has_no_store_to_write_in_process() {
    let (tmp, git_cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // A rescan reaches the store only past the root guard, which admits a project root.
    std::fs::write(tmp.path().join("basemind.toml"), "").expect("basemind.toml");
    let ws = workspace_over(hosted_store(tmp.path()), tmp.path(), git_cache.path());

    assert!(ws.store_write().is_none());
    assert!(ws.try_store_read().is_ok());
    assert!(
        ws.rescan(None, crate::scanner::EmbedMode::Deferred, false).is_err(),
        "a hosted workspace scans through the daemon's pool"
    );
}

#[test]
fn a_local_workspace_still_writes_its_store_in_place_and_takes_a_replacement() {
    crate::store::init_isolated_cache();
    let (tmp, git_cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let ws = workspace_over(
        Store::open(tmp.path(), VIEW_WORKING).expect("open"),
        tmp.path(),
        git_cache.path(),
    );

    ws.store_write().expect("a local store").reset_stale_schema = true;
    assert!(ws.store_read().reset_stale_schema);
    ws.replace_store(Store::open_read_only_no_index(tmp.path(), VIEW_WORKING).expect("replacement"));
    assert!(!ws.store_read().reset_stale_schema);
}
