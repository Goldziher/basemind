use std::ops::Bound;

use fjall::{Readable, Snapshot};

use super::layered_merge::merge;
use super::layered_shadow::Shadow;
use super::view::{IndexBytes, IndexView, KvIter, Part, kv};
use super::{IndexDb, IndexError, META_BM25_DOC_COUNT, META_BM25_TOTAL_LEN, META_IMPORT_PROBE_VER, keys};

struct Reader {
    db: IndexDb,
    snapshot: Snapshot,
}

impl Reader {
    fn new(db: &IndexDb) -> Self {
        Self {
            db: db.clone(),
            snapshot: db.db.snapshot(),
        }
    }
}

impl IndexView for Reader {
    fn get(&self, part: Part, key: &[u8]) -> Result<Option<IndexBytes>, IndexError> {
        Ok(self
            .snapshot
            .get(self.db.keyspace(part), key)?
            .map(IndexBytes::from_fjall))
    }
    fn prefix(&self, part: Part, prefix: &[u8]) -> KvIter<'_> {
        Box::new(self.snapshot.prefix(self.db.keyspace(part), prefix).map(kv))
    }
    fn range(&self, part: Part, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> KvIter<'_> {
        Box::new(
            self.snapshot
                .range::<&[u8], _>(self.db.keyspace(part), (lower, upper))
                .map(kv),
        )
    }
    fn iter(&self, part: Part) -> KvIter<'_> {
        self.range(part, Bound::Unbounded, Bound::Unbounded)
    }
    fn callee_keys_matching(&self, _: &str, _: Option<&[u8]>, lower: Bound<Vec<u8>>) -> KvIter<'_> {
        self.range(Part::CallsByCallee, lower.as_ref().map(Vec::as_slice), Bound::Unbounded)
    }
    fn trait_keys_matching(&self, _: &str, _: Option<&[u8]>, lower: Bound<Vec<u8>>) -> KvIter<'_> {
        self.range(
            Part::ImplementationsByTrait,
            lower.as_ref().map(Vec::as_slice),
            Bound::Unbounded,
        )
    }
}

pub(crate) struct LayeredView {
    base: Box<dyn IndexView>,
    overlay: Box<dyn IndexView>,
    shadow: Shadow,
    bm25: Option<(u64, u64)>,
}

impl LayeredView {
    #[cfg(test)]
    pub(crate) fn new<'a>(base: &'a IndexDb, overlay: &'a IndexDb, shadow: Shadow) -> anyhow::Result<LayeredView> {
        Self::from_view(Box::new(super::view::StandaloneView(base.clone())), overlay, shadow)
    }

    pub(crate) fn from_view(base: Box<dyn IndexView>, overlay: &IndexDb, shadow: Shadow) -> anyhow::Result<Self> {
        Self::from_views(base, Box::new(Reader::new(overlay)), shadow)
    }

    pub(crate) fn from_views(
        base: Box<dyn IndexView>,
        overlay: Box<dyn IndexView>,
        shadow: Shadow,
    ) -> anyhow::Result<Self> {
        let bm25 = effective_bm25(&*base, &*overlay, &shadow)?;
        Ok(LayeredView {
            base,
            overlay,
            shadow,
            bm25,
        })
    }

    fn name_keys(&self, part: Part, needle: &str, cursor_after: Option<&[u8]>, lower: Bound<Vec<u8>>) -> KvIter<'_> {
        let base = match part {
            Part::CallsByCallee => self.base.callee_keys_matching(needle, cursor_after, lower.clone()),
            Part::ImplementationsByTrait => self.base.trait_keys_matching(needle, cursor_after, lower.clone()),
            _ => unreachable!("name dictionaries only cover callees and traits"),
        };
        let rows = self
            .overlay
            .range(part, lower.as_ref().map(Vec::as_slice), Bound::Unbounded);
        let finder = memchr::memmem::Finder::new(needle.as_bytes()).into_owned();
        let overlay = rows.filter(move |row| {
            let Ok((key, _)) = row else {
                return true;
            };
            keys::read_len_prefixed_ref(key, &mut 0).is_some_and(|name| finder.find(name).is_some())
        });
        merge(base, Box::new(overlay), move |key| self.shadow.hides(part, key))
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

fn effective_bm25(
    base: &dyn IndexView,
    overlay: &dyn IndexView,
    shadow: &Shadow,
) -> anyhow::Result<Option<(u64, u64)>> {
    let base_stats = base.bm25_stats();
    let overlay_stats = overlay
        .get(Part::Meta, META_BM25_DOC_COUNT)?
        .map(|count| -> anyhow::Result<_> {
            let length = overlay
                .get(Part::Meta, META_BM25_TOTAL_LEN)?
                .ok_or_else(|| anyhow::anyhow!("incomplete BM25 statistics"))?;
            Ok((
                u64::from_be_bytes(count.as_ref().try_into()?),
                u64::from_be_bytes(length.as_ref().try_into()?),
            ))
        })
        .transpose()?;
    if base_stats.is_none() && overlay_stats.is_none() {
        return Ok(None);
    }

    let (mut count, mut length) = base_stats.unwrap_or_default();
    for path in shadow.paths() {
        for row in base.prefix(Part::CodeBm25ByPath, &keys::code_bm25_by_path_prefix(path)) {
            let (_, value) = row?;
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

impl IndexView for LayeredView {
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
        let base = self.base.prefix(part, prefix);
        let overlay = self.overlay.prefix(part, prefix);
        merge(base, overlay, move |key| self.shadow.hides(part, key))
    }

    fn range<'a>(&'a self, part: Part, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> KvIter<'a> {
        if part == Part::Meta {
            return Box::new(std::iter::empty());
        }
        let base = self.base.range(part, lower, upper);
        let overlay = self.overlay.range(part, lower, upper);
        merge(base, overlay, move |key| self.shadow.hides(part, key))
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
    reference.meta.insert(META_BM25_DOC_COUNT, 3u64.to_be_bytes()).unwrap();
    reference.meta.insert(META_BM25_TOTAL_LEN, 23u64.to_be_bytes()).unwrap();
    super::layered_verify::verify_rows(&view, &reference.view()).unwrap();
    reference.calls_by_callee.insert(&charlie, b"unexpected").unwrap();
    let mismatch = super::layered_verify::verify_rows(&view, &reference.view()).unwrap_err();
    assert!(mismatch.to_string().contains("calls_by_callee"));
    reference.calls_by_callee.remove(&charlie).unwrap();
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

#[test]
fn scanned_caller_pages_match_after_generated_worktree_edits() {
    use super::layered_shadow::DeltaManifest;
    use crate::config::Config as ScanConfig;
    use crate::path::RelPath;
    use crate::scanner::{EmbedMode, ScanSource, scan, scan_paths};
    use crate::store::Store;
    use ahash::AHashMap;
    use proptest::prelude::*;
    use proptest::test_runner::{Config, TestRunner};

    fn fixture(files: &[(String, String)], incremental: bool) -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(directory.path().join(name), text).unwrap();
        }
        let mut store = Store::open(directory.path(), "working").unwrap();
        let mut config = ScanConfig::with_defaults();
        config.documents.enabled = false;
        let root = directory.path();
        let report = if incremental {
            let candidates = files.iter().map(|(name, _)| root.join(name)).collect::<Vec<_>>();
            scan_paths(root, &mut store, &config, &candidates, EmbedMode::Deferred)
        } else {
            scan(root, &mut store, &config, ScanSource::WorkingTree, EmbedMode::Deferred)
        }
        .unwrap();
        assert_eq!(report.stats.read_failed + report.stats.extract_failed, 0);
        assert_eq!(store.index.files.len(), files.len());
        (directory, store)
    }

    fn pages(view: &dyn IndexView) -> Vec<serde_json::Value> {
        let mut output = Vec::new();
        let mut after = None;
        loop {
            let page = crate::mcp::scan_calls_by_name(view, "target", 2, after.as_deref()).unwrap();
            after = page.hit_keys.last().cloned();
            let cursor = page.next_cursor.map(|cursor| cursor.0);
            let done = cursor.is_none();
            output.push(serde_json::json!({
                "hits": page.hits,
                "keys": page.hit_keys,
                "total": page.total,
                "partial": page.total_is_partial,
                "cursor": cursor,
            }));
            if done {
                return output;
            }
            assert!(output.len() < 10);
        }
    }

    crate::store::init_isolated_cache();
    let original = vec![
        ("keep.rs".into(), "fn a() { target(); }".into()),
        ("edit.rs".into(), "fn b() { target(); }".into()),
        ("delete.rs".into(), "fn c() { target(); }".into()),
        ("rename.rs".into(), "fn d() { target(); }".into()),
    ];
    let (_base_root, base) = fixture(&original, false);
    let counts = base.index.files.values().fold(AHashMap::new(), |mut counts, entry| {
        *counts.entry(entry.hash_hex.as_str()).or_default() += 1;
        counts
    });
    let case_count = std::env::var("BASEMIND_LAYERED_CASES")
        .map_or(12, |value| value.parse::<u32>().expect("integer layered case count"));
    assert!(case_count > 0);
    let executed = std::cell::Cell::new(0);
    let cases = (2usize..5, 0u8..8, 0u8..3);
    let mut runner = TestRunner::new(Config {
        cases: case_count,
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(&cases, |(calls, flags, worktree)| {
            executed.set(executed.get() + 1);
            let delete = flags & 1 != 0;
            let rename = flags & 2 != 0;
            let duplicate = flags & 4 != 0;
            let mut changed = vec![
                ("edit.rs".into(), format!("fn b() {{ {} }}", "target(); ".repeat(calls))),
                ("added.rs".into(), format!("fn worktree_{worktree}() {{ target(); }}")),
            ];
            if rename {
                changed.push(("renamed.rs".into(), original[3].1.clone()));
            }
            if duplicate {
                changed.push(("duplicate.rs".into(), original[0].1.clone()));
            }
            let (_overlay_root, overlay) = fixture(&changed, true);
            let mut working = changed;
            working.push(original[0].clone());
            if !delete {
                working.push(original[2].clone());
            }
            if !rename {
                working.push(original[3].clone());
            }
            let (_reference_root, reference) = fixture(&working, false);
            let mut delta = DeltaManifest::empty(1, "fixture".into(), 1);
            for (path, entry) in &overlay.index.files {
                delta.upsert(path.clone(), entry.clone(), base.index.files.get(path));
            }
            if delete {
                delta.delete(RelPath::from("delete.rs"));
            }
            if rename {
                delta.delete(RelPath::from("rename.rs"));
            }
            let shadow = delta.shadow(&counts, |path| base.index.files.get(path));
            let view = LayeredView::new(
                base.index_db.as_ref().unwrap(),
                overlay.index_db.as_ref().unwrap(),
                shadow,
            )
            .unwrap();
            let expected = pages(&reference.index_view().unwrap());
            prop_assert!(expected.len() >= 2);
            prop_assert_eq!(pages(&view), expected);
            super::layered_verify::verify_rows(&view, &reference.index_view().unwrap())
                .map_err(|error| TestCaseError::fail(error.to_string()))?;
            Ok(())
        })
        .unwrap();
    assert_eq!(executed.get(), case_count);
    eprintln!("verified {case_count} layered parity cases");
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
#[test]
fn merged_probes_restitch_an_unchanged_importer_to_a_changed_export() {
    use super::layered_shadow::DeltaManifest;
    use crate::config::Config;
    use crate::intel::resolve_pass::{PathDelta, resolve_with_probe_view};
    use crate::path::RelPath;
    use crate::scanner::{EmbedMode, ScanSource, scan};
    use crate::store::Store;

    fn scan_fixture(export: &str, importer: bool) -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("target.ts"), export).unwrap();
        if importer {
            std::fs::write(root.join("use.ts"), "import { target } from './target';\ntarget();\n").unwrap();
        }
        let mut store = Store::open(root, "working").unwrap();
        let mut config = Config::with_defaults();
        config.documents.enabled = false;
        let report = scan(root, &mut store, &config, ScanSource::WorkingTree, EmbedMode::Deferred).unwrap();
        assert_eq!(report.stats.read_failed + report.stats.extract_failed, 0);
        (directory, store)
    }

    crate::store::init_isolated_cache();
    let (_base_root, base) = scan_fixture("export function target() {}\n", true);
    let new_export = "// moved\nexport function target() {}\n";
    let (working, mut overlay) = scan_fixture(new_export, false);
    let importer = RelPath::from("use.ts");
    let target = RelPath::from("target.ts");
    std::fs::write(
        working.path().join("use.ts"),
        "import { target } from './target';\ntarget();\n",
    )
    .unwrap();
    let (_reference_root, reference) = scan_fixture(new_export, true);
    let unchanged = base.lookup(&importer).unwrap().clone();
    let l1 = base.read_l1_by_hex(&unchanged.hash_hex).unwrap().unwrap();
    let l2 = base.read_l2_by_hex(&unchanged.hash_hex).unwrap();
    overlay
        .write_filemap_hex(&unchanged.hash_hex, &l1, l2.as_ref())
        .unwrap();
    overlay.upsert(importer.clone(), unchanged);
    let mut delta = DeltaManifest::empty(1, "fixture".into(), 1);
    delta.upsert(
        target.clone(),
        overlay.lookup(&target).unwrap().clone(),
        base.lookup(&target),
    );
    let counts = base
        .index
        .files
        .values()
        .map(|entry| (entry.hash_hex.as_str(), 1))
        .collect();
    let base_db = base.index_db.as_ref().unwrap();
    let overlay_db = overlay.index_db.as_ref().unwrap();
    let probe_view = LayeredView::new(base_db, overlay_db, delta.shadow(&counts, |path| base.lookup(path))).unwrap();
    assert_eq!(probe_view.importers_probing("target.ts"), [importer.clone()]);
    let config = Config::with_defaults();
    let changed = ["target.ts".to_string()];
    let (complete, restitched) = resolve_with_probe_view(
        working.path(),
        &overlay,
        &PathDelta {
            changed: &changed,
            removed: &[],
            other: &[],
        },
        config.code_intel.precise_resolution,
        config.resources.max_footprint_mb,
        &probe_view,
    );
    assert!(complete);
    assert_eq!(restitched, ["use.ts"]);
    let definition = u32::try_from(new_export.find("target").unwrap()).unwrap();
    let expected = reference.index_view().unwrap().references_to(&target, definition);
    assert!(!expected.is_empty());
    assert_eq!(
        overlay.index_view().unwrap().references_to(&target, definition),
        expected
    );
}
