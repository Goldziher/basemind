//! A workspace over a temporary directory with a stand-in embedding model, for the memory tests.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::embeddings::{FakeEmbed, SharedEmbedder};
use crate::engine::Workspace;

/// The vector length of every stand-in model.
pub(crate) const DIM: u16 = 4;

/// A workspace over the directory `root` whose embedder reports `model` and answers with `embed`.
pub(crate) fn workspace(root: &Path, model: &str, embed: FakeEmbed) -> Workspace {
    use crate::config::Config;
    use crate::store::{Store, VIEW_WORKING};

    crate::store::init_isolated_cache();
    // An earlier workspace over the same root releases its index lock a moment after it is dropped.
    let opening = std::time::Instant::now();
    let store = loop {
        match Store::open(root, VIEW_WORKING) {
            Ok(store) => break store,
            Err(_) if opening.elapsed() < Duration::from_secs(20) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => panic!("open the store: {error}"),
        }
    };
    let ws = Workspace::new(
        store,
        root.to_path_buf(),
        Arc::new(Config::with_defaults()),
        None,
        Arc::new(crate::git_cache::GitCache::open(root, 1, false).unwrap()),
        None,
        crate::engine::workspace::OpenOptions {
            background: false,
            read_only: false,
            forwards_writes: false,
            lazy_cache: false,
            history_sync: None,
            memory_owner: |_| "owner".to_string(),
        },
    );
    ws.embedder
        .set(Arc::new(SharedEmbedder::fake(model, DIM, embed)))
        .ok()
        .unwrap();
    ws
}
