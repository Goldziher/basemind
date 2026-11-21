use std::path::Path;

use crate::engine::Workspace;
use crate::engine::memory::test_support::{DIM, workspace};
use crate::engine::memory::vectors::memory_lance_dir;
use crate::engine::types::memory::{MemoryListParams, MemoryPutParams, Visibility};
use crate::lance::LanceStore;

/// A vector that depends on the text's length alone.
fn by_length(text: &str) -> anyhow::Result<Vec<f32>> {
    Ok(vec![text.len() as f32; usize::from(DIM)])
}

fn put(ws: &Workspace, key: &str, visibility: Visibility) {
    let saved = ws
        .memory_put(MemoryPutParams {
            key: key.into(),
            value: format!("the {key} note"),
            tags: None,
            embed: true,
            visibility,
        })
        .unwrap();
    assert!(saved.embedded, "{key}");
}

fn listed(ws: &Workspace, visibility: Visibility) -> usize {
    let params = MemoryListParams {
        prefix: None,
        tag: None,
        limit: None,
        cursor: None,
        visibility,
    };
    ws.memory_list(params).unwrap().total
}

/// The rows, rows marked deleted and versions of the memory table a model's store holds.
fn store_state(root: &Path, model: &str) -> (usize, Option<usize>, Vec<usize>) {
    let store = LanceStore::open(&memory_lance_dir(root, model, DIM), DIM, model).unwrap();
    let scope = format!("path:{}", root.display());
    let rows = ["group", "individual"]
        .iter()
        .zip(["", "owner"])
        .map(|(visibility, owner)| store.memory_stamps(&scope, visibility, owner).unwrap().stamps.len())
        .sum();
    let versions = store
        .version_counts()
        .unwrap()
        .into_iter()
        .filter(|(table, _)| table == "memory")
        .map(|(_, count)| count)
        .collect();
    (rows, store.memory_rows_marked_deleted().unwrap(), versions)
}

/// A clear removes the notes of every tier and their rows from every model's store, which it
/// leaves purged.
#[test]
fn a_clear_removes_every_note_and_purges_every_models_store() {
    let root = tempfile::tempdir().unwrap();
    let first = workspace(root.path(), "model-a", by_length);
    put(&first, "shared", Visibility::Group);
    put(&first, "mine", Visibility::Individual);
    drop(first);
    let second = workspace(root.path(), "model-b", by_length);
    put(&second, "later", Visibility::Group);

    let cleared = second.memory_clear_group().unwrap();

    assert_eq!(
        (
            cleared.notes_removed,
            cleared.vector_stores_purged,
            cleared.vector_stores_failed
        ),
        (3, 2, 0)
    );
    assert_eq!(listed(&second, Visibility::Group), 0);
    assert_eq!(listed(&second, Visibility::Individual), 0);
    drop(second);
    for model in ["model-a", "model-b"] {
        assert_eq!(store_state(root.path(), model), (0, Some(0), vec![1]), "{model}");
    }
}

/// A clear where nothing was ever saved removes nothing and creates no store.
#[test]
fn a_clear_with_no_notes_removes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let ws = workspace(root.path(), "fake", by_length);

    let cleared = ws.memory_clear_group().unwrap();

    assert_eq!(
        (
            cleared.notes_removed,
            cleared.vector_stores_purged,
            cleared.vector_stores_failed
        ),
        (0, 0, 0)
    );
    assert!(!crate::store_layout::memory_vectors_dir(root.path()).exists());
}

/// A store that cannot be opened is reported by a clear and a delete, which still remove the notes,
/// and a retry reaches the stores that work.
#[test]
fn a_store_that_cannot_be_purged_is_reported_not_hidden() {
    let root = tempfile::tempdir().unwrap();
    let ws = workspace(root.path(), "model-a", by_length);
    put(&ws, "shared", Visibility::Group);
    let broken = crate::store_layout::memory_vectors_dir(root.path()).join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("meta.json"), "not json").unwrap();

    let deleted = ws
        .memory_delete_note(crate::engine::types::memory::MemoryDeleteParams {
            key: "shared".into(),
            visibility: Visibility::Group,
        })
        .unwrap();
    assert!(deleted.deleted);
    assert_eq!(deleted.vector_stores_failed, 1);

    put(&ws, "again", Visibility::Group);
    let cleared = ws.memory_clear_group().unwrap();
    assert_eq!(
        (
            cleared.notes_removed,
            cleared.vector_stores_purged,
            cleared.vector_stores_failed
        ),
        (1, 1, 1)
    );
}

/// A clear forgets which namespaces were level, so a note saved while the rows were purged is
/// backfilled by the next search instead of staying unsearchable until its mark lapses.
#[test]
fn a_clear_forgets_the_level_marks() {
    let root = tempfile::tempdir().unwrap();
    let ws = workspace(root.path(), "model-a", by_length);
    put(&ws, "shared", Visibility::Group);
    ws.memory_backfill
        .lock()
        .unwrap()
        .insert((0, String::new()), crate::engine::memory::vectors::Backfill::Running);

    ws.memory_clear_group().unwrap();

    assert!(ws.memory_backfill.lock().unwrap().is_empty());
}
