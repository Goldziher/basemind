//! The `IndexView` contract: the generic layer reads exactly what the keyspaces hold. The read paths
//! that go through it are covered in `mcp::index_view_tests`.

use std::ops::Bound;

use super::IndexDb;
use super::view::{IndexView, KvIter, Part};
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

type Rows = Vec<(Vec<u8>, Vec<u8>)>;
type Case<'a> = (&'a str, Bound<&'a [u8]>, Bound<&'a [u8]>);

fn rows(iter: KvIter<'_>) -> Rows {
    iter.map(|r| r.expect("row"))
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect()
}

fn direct_rows(db: &IndexDb, part: Part) -> Rows {
    db.keyspace_for_test(part)
        .iter()
        .map(|g| g.into_inner().expect("row"))
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect()
}

fn only(direct: &Rows, keep: impl Fn(&[u8]) -> bool) -> Rows {
    direct.iter().filter(|(k, _)| keep(k)).cloned().collect()
}

#[test]
fn the_generic_layer_reads_what_each_keyspace_holds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = scanned_store(dir.path());
    let db = store.index_db.as_ref().expect("index");
    let view = db.view();
    let mut non_empty = 0;
    let mut bounded = 0;
    for part in Part::ALL {
        let direct = direct_rows(db, part);
        let name = part.name();
        assert_eq!(rows(view.iter(part)), direct, "{name} iter");
        assert_eq!(rows(view.prefix(part, b"")), direct, "{name} empty prefix");
        assert_eq!(
            rows(view.range(part, Bound::Unbounded, Bound::Unbounded)),
            direct,
            "{name} unbounded range"
        );
        assert_eq!(view.get(part, &[0xFF; 3]).expect("get"), None, "{name} miss");
        let Some((mid, value)) = direct.get(direct.len() / 2) else {
            continue;
        };
        non_empty += 1;
        assert_eq!(
            view.get(part, mid).expect("get").map(|v| v.to_vec()),
            Some(value.clone()),
            "{name} get"
        );
        for len in [1, mid.len() / 2, mid.len()] {
            let prefix = &mid[..len.max(1)];
            assert_eq!(
                rows(view.prefix(part, prefix)),
                only(&direct, |k| k.starts_with(prefix)),
                "{name} prefix of {len} bytes of a mid-keyspace key"
            );
        }
        let last = &direct[direct.len() - 1].0;
        let cases: [Case<'_>; 6] = [
            ("[mid, ..)", Bound::Included(mid), Bound::Unbounded),
            ("(mid, ..)", Bound::Excluded(mid), Bound::Unbounded),
            ("(.., mid]", Bound::Unbounded, Bound::Included(mid)),
            ("(.., mid)", Bound::Unbounded, Bound::Excluded(mid)),
            ("[mid, last]", Bound::Included(mid), Bound::Included(last)),
            ("(mid, last)", Bound::Excluded(mid), Bound::Excluded(last)),
        ];
        for (label, lower, upper) in cases {
            let in_lower = |k: &[u8]| match lower {
                Bound::Included(b) => k >= b,
                Bound::Excluded(b) => k > b,
                Bound::Unbounded => true,
            };
            let in_upper = |k: &[u8]| match upper {
                Bound::Included(b) => k <= b,
                Bound::Excluded(b) => k < b,
                Bound::Unbounded => true,
            };
            let expected = only(&direct, |k| in_lower(k) && in_upper(k));
            assert_eq!(rows(view.range(part, lower, upper)), expected, "{name} range {label}");
            if direct.len() > 2 {
                bounded += 1;
            }
        }
    }
    assert!(
        non_empty >= 6,
        "the corpus must populate the layered keyspaces, got {non_empty}"
    );
    assert!(
        bounded >= 6 * 6,
        "bounded ranges must be exercised on populated keyspaces"
    );
}

#[test]
fn parts_name_exactly_the_layered_keyspaces() {
    let names: std::collections::BTreeSet<_> = Part::ALL.iter().map(|p| p.name()).collect();
    assert_eq!(names.len(), Part::ALL.len(), "part names are unique");
    let unlayered = ["embeddings", "memory_by_key", "memory_archive", "proposals"];
    for name in &names {
        assert!(
            super::KEYSPACE_MEMTABLE_BYTES.iter().any(|(k, _)| k == name),
            "{name} must be a real keyspace"
        );
        assert!(!unlayered.contains(name), "{name} is per-group or per-worktree state");
    }
    for (keyspace, _) in super::KEYSPACE_MEMTABLE_BYTES {
        assert!(
            names.contains(keyspace) || unlayered.contains(keyspace),
            "keyspace {keyspace} is neither a layered Part nor listed as unlayered"
        );
    }
}

#[test]
fn named_reads_follow_the_data_they_derive_from() {
    let empty_dir = tempfile::tempdir().expect("tempdir");
    let empty = IndexDb::open(empty_dir.path()).expect("open");
    assert!(empty.view().symbols_index_is_empty());
    assert_eq!(empty.view().bm25_stats(), None);
    assert!(!empty.view().import_probes_complete());

    let dir = tempfile::tempdir().expect("tempdir");
    let store = scanned_store(dir.path());
    let view = store.index_view().expect("view");
    assert!(!view.symbols_index_is_empty());
    let defined = view.symbols_by_name_lookup("driver_1", 10);
    assert_eq!(defined.len(), 1, "one definition of driver_1: {defined:?}");
    assert_eq!(
        view.symbols_by_name_lookup("driver_1", 0),
        vec![],
        "cap 0 reads nothing"
    );
    let all_runs = view.symbols_by_name_lookup("run", 100);
    assert!(
        all_runs.len() > 3,
        "every trait and impl `run` is indexed: {all_runs:?}"
    );
    assert_eq!(
        view.symbols_by_name_lookup("run", 3),
        all_runs[..3],
        "the cap truncates in key order"
    );
}
