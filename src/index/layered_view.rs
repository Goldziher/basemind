use std::ops::Bound;

use fjall::{Readable, Snapshot};

use super::layered_merge::merge;
use super::layered_shadow::Shadow;
use super::view::{IndexBytes, IndexView, KvIter, Part, kv};
use super::{IndexDb, IndexError, META_BM25_DOC_COUNT, META_BM25_TOTAL_LEN, META_IMPORT_PROBE_VER, keys, name_dict};

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
    fn new<'a>(base: &'a IndexDb, overlay: &'a IndexDb, shadow: Shadow<'a>) -> anyhow::Result<LayeredView<'a>> {
        let base = Reader::new(base);
        let overlay = Reader::new(overlay);
        let bm25 = effective_bm25(&base, &overlay, &shadow)?;
        Ok(LayeredView {
            base,
            overlay,
            shadow,
            bm25,
        })
    }

    fn name_keys(&self, part: Part, needle: &str, cursor_after: Option<&[u8]>, lower: Bound<Vec<u8>>) -> KvIter<'_> {
        let names = |reader: &Reader<'_>| {
            let dictionary = match part {
                Part::CallsByCallee => &reader.db.callee_names,
                Part::ImplementationsByTrait => &reader.db.trait_names,
                _ => unreachable!("name dictionaries only cover callees and traits"),
            };
            dictionary.matching_prefixes(reader.db.keyspace(part), needle)
        };
        let (Some(mut base), Some(overlay)) = (names(&self.base), names(&self.overlay)) else {
            return self.range(part, lower.as_ref().map(Vec::as_slice), Bound::Unbounded);
        };
        base.extend(overlay);
        base.sort_unstable();
        base.dedup();
        let cursor = cursor_after.map(<[u8]>::to_vec);
        Box::new(base.into_iter().flat_map(move |prefix| {
            if cursor
                .as_ref()
                .is_some_and(|cursor| cursor > &prefix && !cursor.starts_with(&prefix))
            {
                return Box::new(std::iter::empty()) as KvIter<'_>;
            }
            let lower = match cursor.as_ref().filter(|cursor| cursor.starts_with(&prefix)) {
                Some(cursor) => Bound::Excluded(cursor.as_slice()),
                None => Bound::Included(prefix.as_slice()),
            };
            let upper = name_dict::upper_bound(&prefix);
            self.range(part, lower, upper.as_deref().map_or(Bound::Unbounded, Bound::Excluded))
        }))
    }

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

fn effective_bm25(base: &Reader<'_>, overlay: &Reader<'_>, shadow: &Shadow<'_>) -> anyhow::Result<Option<(u64, u64)>> {
    let stats = |reader: &Reader<'_>| -> anyhow::Result<Option<(u64, u64)>> {
        let scalar = |key| -> anyhow::Result<Option<u64>> {
            reader
                .get(Part::Meta, key)?
                .map(|bytes| Ok(u64::from_be_bytes(bytes.as_ref().try_into()?)))
                .transpose()
        };
        match (scalar(META_BM25_DOC_COUNT)?, scalar(META_BM25_TOTAL_LEN)?) {
            (Some(count), Some(length)) => Ok(Some((count, length))),
            (None, None) => Ok(None),
            _ => anyhow::bail!("incomplete BM25 statistics"),
        }
    };
    let base_stats = stats(base)?;
    let overlay_stats = stats(overlay)?;
    if base_stats.is_none() && overlay_stats.is_none() {
        return Ok(None);
    }

    let (mut count, mut length) = base_stats.unwrap_or_default();
    for path in shadow.paths() {
        for guard in base
            .snapshot
            .prefix(&base.db.code_bm25_by_path, keys::code_bm25_by_path_prefix(path))
        {
            let (_, value) = guard.into_inner()?;
            let doclen = value
                .get(..4)
                .ok_or_else(|| anyhow::anyhow!("truncated BM25 forward row"))?;
            let doclen = u64::from(u32::from_be_bytes(doclen.try_into()?));
            count = count
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("BM25 count underflow"))?;
            length = length
                .checked_sub(doclen)
                .ok_or_else(|| anyhow::anyhow!("BM25 length underflow"))?;
        }
    }
    let (added_count, added_length) = overlay_stats.unwrap_or_default();
    Ok(Some((
        count
            .checked_add(added_count)
            .ok_or_else(|| anyhow::anyhow!("BM25 count overflow"))?,
        length
            .checked_add(added_length)
            .ok_or_else(|| anyhow::anyhow!("BM25 length overflow"))?,
    )))
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
        needle: &str,
        cursor_after: Option<&[u8]>,
        lower: Bound<Vec<u8>>,
    ) -> KvIter<'a> {
        self.name_keys(Part::CallsByCallee, needle, cursor_after, lower)
    }

    fn trait_keys_matching<'a>(
        &'a self,
        needle: &str,
        cursor_after: Option<&[u8]>,
        lower: Bound<Vec<u8>>,
    ) -> KvIter<'a> {
        self.name_keys(Part::ImplementationsByTrait, needle, cursor_after, lower)
    }
}

#[test]
fn pinned_overlay_reads_match_standalone_and_ignore_later_writes() {
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
    let removed = RelPath::from("remove.rs");
    let mut delta = DeltaManifest::empty(1, "commit".into(), 1);
    delta.delete(removed.clone());
    base.code_bm25_by_path
        .insert(keys::code_bm25_by_path(&removed, "old:0"), 2u32.to_be_bytes())
        .unwrap();
    for db in [&base, &overlay, &reference] {
        db.callee_names.always_use_dictionary();
        db.callee_names.build_blocking(&db.calls_by_callee);
    }
    base.meta.insert(META_BM25_DOC_COUNT, 3u64.to_be_bytes()).unwrap();
    base.meta.insert(META_BM25_TOTAL_LEN, 15u64.to_be_bytes()).unwrap();
    overlay.meta.insert(META_BM25_DOC_COUNT, 1u64.to_be_bytes()).unwrap();
    overlay.meta.insert(META_BM25_TOTAL_LEN, 10u64.to_be_bytes()).unwrap();
    let view = LayeredView::new(&base, &overlay, delta.shadow(&Default::default(), |_| None)).unwrap();
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
    assert_eq!(view.bm25_stats(), Some((3, 23)));
    let scan = |view: &dyn IndexView, after: Option<&[u8]>| {
        let page = crate::mcp::scan_calls_by_name(view, "run", 1, after).unwrap();
        (
            page.hit_keys,
            page.total,
            page.total_is_partial,
            page.next_cursor.map(|cursor| cursor.0),
        )
    };
    let reference = reference.view();
    assert_eq!(scan(&view, None), scan(&reference, None));
    assert_eq!(scan(&view, Some(&alpha)), scan(&reference, Some(&alpha)));
}
