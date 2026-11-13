use std::ops::Bound;

use fjall::{Readable, Snapshot};

use super::layered_merge::merge;
use super::layered_shadow::Shadow;
use super::view::{IndexBytes, IndexView, KvIter, Part, kv};
use super::{IndexDb, IndexError, META_BM25_DOC_COUNT, META_BM25_TOTAL_LEN, META_IMPORT_PROBE_VER};

struct Reader<'a> {
    db: &'a IndexDb,
    snapshot: Snapshot,
}

impl<'a> Reader<'a> {
    fn new(db: &'a IndexDb) -> Self {
        Self {
            db,
            snapshot: db.db.snapshot(),
        }
    }

    fn get(&self, part: Part, key: &[u8]) -> Result<Option<IndexBytes>, IndexError> {
        Ok(self
            .snapshot
            .get(self.db.keyspace(part), key)?
            .map(IndexBytes::from_fjall))
    }
}

struct LayeredView<'a> {
    base: Reader<'a>,
    overlay: Reader<'a>,
    shadow: Shadow<'a>,
    bm25: Option<(u64, u64)>,
}

impl LayeredView<'_> {
    fn meta(&self, key: &[u8]) -> Result<Option<IndexBytes>, IndexError> {
        let value = match (key, self.bm25) {
            (META_BM25_DOC_COUNT, Some((count, _))) => count,
            (META_BM25_TOTAL_LEN, Some((_, length))) => length,
            (META_BM25_DOC_COUNT | META_BM25_TOTAL_LEN, None) => return Ok(None),
            (META_IMPORT_PROBE_VER, _) => {
                let base = self.base.get(Part::Meta, key)?;
                let overlay = self.overlay.get(Part::Meta, key)?;
                return Ok(base.filter(|value| overlay.as_ref() == Some(value)));
            }
            _ => return self.base.get(Part::Meta, key),
        };
        Ok(Some(IndexBytes::from_fjall(value.to_be_bytes().to_vec().into())))
    }
}

impl IndexView for LayeredView<'_> {
    fn get(&self, part: Part, key: &[u8]) -> Result<Option<IndexBytes>, IndexError> {
        if part == Part::Meta {
            return self.meta(key);
        }
        let overlay = self.overlay.get(part, key)?;
        if overlay.is_some() || self.shadow.hides(part, key) {
            return Ok(overlay);
        }
        self.base.get(part, key)
    }

    fn prefix<'a>(&'a self, part: Part, prefix: &[u8]) -> KvIter<'a> {
        if part == Part::Meta {
            return Box::new(std::iter::empty());
        }
        let base = self.base.snapshot.prefix(self.base.db.keyspace(part), prefix);
        let overlay = self.overlay.snapshot.prefix(self.overlay.db.keyspace(part), prefix);
        merge(Box::new(base.map(kv)), Box::new(overlay.map(kv)), move |key| {
            self.shadow.hides(part, key)
        })
    }

    fn range<'a>(&'a self, part: Part, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> KvIter<'a> {
        if part == Part::Meta {
            return Box::new(std::iter::empty());
        }
        let base = self
            .base
            .snapshot
            .range::<&[u8], _>(self.base.db.keyspace(part), (lower, upper));
        let overlay = self
            .overlay
            .snapshot
            .range::<&[u8], _>(self.overlay.db.keyspace(part), (lower, upper));
        merge(Box::new(base.map(kv)), Box::new(overlay.map(kv)), move |key| {
            self.shadow.hides(part, key)
        })
    }

    fn iter(&self, part: Part) -> KvIter<'_> {
        self.range(part, Bound::Unbounded, Bound::Unbounded)
    }

    fn callee_keys_matching<'a>(
        &'a self,
        _needle: &str,
        _cursor_after: Option<&[u8]>,
        lower: Bound<Vec<u8>>,
    ) -> KvIter<'a> {
        self.range(Part::CallsByCallee, lower.as_ref().map(Vec::as_slice), Bound::Unbounded)
    }

    fn trait_keys_matching<'a>(
        &'a self,
        _needle: &str,
        _cursor_after: Option<&[u8]>,
        lower: Bound<Vec<u8>>,
    ) -> KvIter<'a> {
        self.range(
            Part::ImplementationsByTrait,
            lower.as_ref().map(Vec::as_slice),
            Bound::Unbounded,
        )
    }
}

#[test]
fn pinned_overlay_reads_match_standalone_and_ignore_later_writes() {
    use super::keys;
    use super::layered_shadow::DeltaManifest;
    use crate::path::RelPath;

    let directory = tempfile::tempdir().unwrap();
    let base = IndexDb::open(&directory.path().join("base")).unwrap();
    let overlay = IndexDb::open(&directory.path().join("overlay")).unwrap();
    let reference = IndexDb::open(&directory.path().join("standalone")).unwrap();
    let alpha = keys::call_by_callee("run", &RelPath::from("alpha.rs"), 1).unwrap();
    let bravo = keys::call_by_callee("run", &RelPath::from("bravo.rs"), 1).unwrap();
    let charlie = keys::call_by_callee("run", &RelPath::from("charlie.rs"), 1).unwrap();
    base.calls_by_callee.insert(&alpha, b"base").unwrap();
    overlay.calls_by_callee.insert(&bravo, b"overlay").unwrap();
    reference.calls_by_callee.insert(&alpha, b"base").unwrap();
    reference.calls_by_callee.insert(&bravo, b"overlay").unwrap();
    let delta = DeltaManifest::empty(1, "commit".into(), 1);
    let view = LayeredView {
        base: Reader::new(&base),
        overlay: Reader::new(&overlay),
        shadow: delta.shadow(&Default::default(), |_| None),
        bm25: Some((4, 25)),
    };
    overlay.calls_by_callee.insert(&bravo, b"new value").unwrap();
    overlay.calls_by_callee.insert(&charlie, b"new row").unwrap();
    let part = Part::CallsByCallee;
    let prefix = &alpha[..5];
    let expected = reference
        .view()
        .prefix(part, prefix)
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), 2);
    assert_eq!(
        view.prefix(part, prefix).map(Result::unwrap).collect::<Vec<_>>(),
        expected
    );
    let lower = Bound::Excluded(alpha.as_slice());
    let after = view
        .range(part, lower, Bound::Unbounded)
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    assert_eq!(after, expected[1..]);
    assert_eq!(&*view.get(part, &bravo).unwrap().unwrap(), b"overlay");
    assert!(view.get(part, &charlie).unwrap().is_none());
    assert_eq!(view.bm25_stats(), Some((4, 25)));
}
