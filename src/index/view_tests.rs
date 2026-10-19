//! The `IndexView` contract: the generic layer reads exactly what the keyspaces hold. The read paths
//! that go through it are covered in `mcp::index_view_tests`.

use super::IndexDb;
use super::view::{IndexView, Part};
use crate::config::ConfigV1;
use crate::scanner::{EmbedMode, ScanSource, scan};
use crate::store::{Store, VIEW_WORKING};

fn scanned_store(root: &std::path::Path) -> Store {
    for k in 0..6 {
        std::fs::write(
            root.join(format!("m{k}.rs")),
            format!(
                "pub trait Tr{k} {{ fn run(&self); }}\n\
                 pub struct S{k};\n\
                 impl Tr{k} for S{k} {{ fn run(&self) {{ shared_helper(); }} }}\n\
                 pub fn driver_{k}() {{ shared_helper(); local_{k}(); }}\n"
            ),
        )
        .expect("write corpus file");
    }
    let mut store = Store::open(root, VIEW_WORKING).expect("open store");
    scan(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Inline,
    )
    .expect("scan");
    store
}

#[test]
fn the_generic_layer_reads_what_each_keyspace_holds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = scanned_store(dir.path());
    let db = store.index_db.as_ref().expect("index");
    let view = db.view();
    let mut non_empty = 0;
    for part in Part::ALL {
        let keyspace = db.keyspace_for_test(part);
        let direct: Vec<_> = keyspace
            .iter()
            .map(|g| g.into_inner().expect("row"))
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect();
        let rows = |iter: super::view::KvIter<'_>| -> Vec<(Vec<u8>, Vec<u8>)> {
            iter.map(|r| r.expect("row"))
                .map(|(k, v)| (k.to_vec(), v.to_vec()))
                .collect()
        };
        assert_eq!(rows(view.iter(part)), direct, "{} iter", part.name());
        assert_eq!(rows(view.prefix(part, b"")), direct, "{} empty prefix", part.name());
        assert_eq!(
            rows(view.range(part, std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)),
            direct,
            "{} unbounded range",
            part.name()
        );
        if let Some((key, value)) = direct.first() {
            non_empty += 1;
            assert_eq!(
                view.get(part, key).expect("get").map(|v| v.to_vec()),
                Some(value.clone())
            );
            assert_eq!(rows(view.prefix(part, &key[..1])).first(), direct.first());
        }
        assert_eq!(view.get(part, &[0xFF; 3]).expect("get"), None);
    }
    assert!(
        non_empty >= 6,
        "the corpus must populate the layered keyspaces, got {non_empty}"
    );
}

#[test]
fn parts_name_exactly_the_layered_keyspaces() {
    let names: std::collections::BTreeSet<_> = Part::ALL.iter().map(|p| p.name()).collect();
    assert_eq!(names.len(), Part::ALL.len(), "part names are unique");
    for name in &names {
        assert!(
            super::KEYSPACE_MEMTABLE_BYTES.iter().any(|(k, _)| k == name),
            "{name} must be a real keyspace"
        );
    }
    for unlayered in ["embeddings", "memory_by_key", "memory_archive", "proposals"] {
        assert!(
            !names.contains(unlayered),
            "{unlayered} is per-group or per-worktree state"
        );
    }
}

#[test]
fn store_level_reads_use_the_store_view() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = scanned_store(dir.path());
    let view = store.index_view().expect("view");
    let db: &IndexDb = store.index_db.as_ref().expect("index");
    assert_eq!(view.symbols_index_is_empty(), db.symbols_index_is_empty());
    assert!(!view.symbols_index_is_empty());
}
