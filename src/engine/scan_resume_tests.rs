use std::path::Path;
use std::sync::Arc;

use crate::embed_progress;
use crate::engine::workspace::{OpenOptions, Workspace};
use crate::scanner::EmbedMode;
use crate::store::{Store, VIEW_WORKING};

const GUIDES: [&str; 3] = ["a.md", "b.md", "c.md"];

/// A workspace over three indexed guides, each recorded as owing its vectors as a killed embedding
/// pass leaves them. The config embeds nothing, so each batch settles its guides by their stat and
/// no model is loaded.
fn workspace_owing_vectors(root: &Path, git_cache: &Path) -> Workspace {
    crate::store::init_isolated_cache();
    // A workspace rescans only a root the root guard admits as a project.
    std::fs::write(root.join("basemind.toml"), "").expect("basemind.toml");
    for guide in GUIDES {
        std::fs::write(
            root.join(guide),
            format!("# {guide}\n\nHow the harbour is run, long enough to index as a document.\n"),
        )
        .expect("guide");
    }
    let mut config = crate::config::Config::with_defaults();
    config.documents.enabled = true;
    config.documents.embed = false;
    let ws = Workspace::new(
        Store::open(root, VIEW_WORKING).expect("open"),
        root.to_path_buf(),
        Arc::new(config),
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
    );
    ws.rescan(None, EmbedMode::Inline, false).expect("first pass");
    let mut store = ws.store_write();
    for guide in GUIDES {
        let mut entry = store.lookup_doc(guide).expect("the guide is indexed").clone();
        entry.embedded = false;
        entry.embed_attempted = false;
        store.upsert_doc(guide, entry);
    }
    drop(store);
    ws
}

/// The resume embeds the owed documents batch by batch, releasing the store write lock between
/// batches so a read is answered mid-pass, and advances the progress record after each.
#[test]
fn a_resumed_embedding_pass_releases_the_store_between_batches() {
    let (tmp, git_cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let ws = workspace_owing_vectors(tmp.path(), git_cache.path());
    let cache_dir = crate::store_layout::workspace_cache_dir(tmp.path());

    let mut seen = Vec::new();
    ws.embed_owed_documents(1, &mut |done| {
        let readable = ws.try_store_read().is_ok();
        let record = embed_progress::read(&cache_dir).map(|record| (record.done, record.total));
        seen.push((done, readable, record));
    })
    .expect("resume");

    assert_eq!(
        seen,
        vec![
            (1, true, Some((1, 3))),
            (2, true, Some((2, 3))),
            (3, true, Some((3, 3))),
        ]
    );
}

/// The whole-tree pass that ends a resume settles the record: nothing is owed, so it is removed.
#[test]
fn a_completed_resume_removes_the_progress_record() {
    let (tmp, git_cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let ws = workspace_owing_vectors(tmp.path(), git_cache.path());
    let cache_dir = crate::store_layout::workspace_cache_dir(tmp.path());
    embed_progress::record_owed(&cache_dir, 3);

    ws.resume_embedding().expect("resume");

    assert_eq!(embed_progress::read(&cache_dir), None);
}
