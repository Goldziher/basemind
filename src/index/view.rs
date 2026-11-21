//! Read-only view over the fjall index.
//!
//! Every read of the index goes through [`IndexView`], which answers for one view of the group index
//! (ADR-0028). Writers keep using [`IndexWriter`](super::writer::IndexWriter) on the concrete
//! [`IndexDb`].
//!
//! The trait has two layers: the generic layer (`get` / `prefix` / `range` / `iter`, addressed by
//! [`Part`]) and the named accessors the read path performs, written once as default methods over
//! it. No fjall type appears in a signature: values come back as [`IndexBytes`].

use std::borrow::Borrow;
use std::ops::{Bound, Deref};
use std::path::Path;

use super::scoped::Scoped;

use super::grep_bloom::{GrepEntry, Needle};
use super::{
    IMPORT_PROBE_VER, IndexDb, IndexError, META_BM25_DOC_COUNT, META_BM25_TOTAL_LEN, META_IMPORT_PROBE_VER, keys,
    name_dict, probe_candidates,
};
use crate::extract::SymbolKind;
use crate::path::RelPath;

/// Bytes read from the index: a key or a value.
///
/// A refcounted handle, not a copy: it wraps the fjall slice the read already produced. The enum is
/// private so no backend type is nameable from a signature.
#[derive(Clone)]
pub struct IndexBytes(Repr);

#[derive(Clone)]
enum Repr {
    /// A fjall slice read from `offset` on: a view of a group index strips its key prefix this way.
    Fjall(fjall::Slice, usize),
    /// Bytes the view composed (a site key followed by the path of one live holder), in a buffer it
    /// writes again once no reader holds it.
    Buffer(std::sync::Arc<Vec<u8>>),
}

impl IndexBytes {
    pub(super) fn from_fjall(slice: fjall::Slice) -> Self {
        Self(Repr::Fjall(slice, 0))
    }

    /// `slice` without its first `offset` bytes.
    pub(super) fn from_fjall_at(slice: fjall::Slice, offset: usize) -> Self {
        debug_assert!(offset <= slice.len(), "a scoped key is at least its prefix");
        Self(Repr::Fjall(slice, offset))
    }

    /// No bytes: the value of a hit, which names its row by key alone.
    pub(super) fn empty() -> Self {
        Self(Repr::Fjall(fjall::Slice::from(&[][..]), 0))
    }

    /// Bytes composed by the view in a reusable buffer.
    pub(super) fn buffer(bytes: std::sync::Arc<Vec<u8>>) -> Self {
        Self(Repr::Buffer(bytes))
    }
}

impl Deref for IndexBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.0 {
            Repr::Fjall(slice, offset) => &slice[*offset..],
            Repr::Buffer(bytes) => bytes,
        }
    }
}

impl AsRef<[u8]> for IndexBytes {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl PartialEq for IndexBytes {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for IndexBytes {}

impl std::fmt::Debug for IndexBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}

/// One `(key, value)` row, or the error that interrupted the walk.
pub type KvResult = Result<(IndexBytes, IndexBytes), IndexError>;

/// Boxed row iterator. One virtual call per row; the boxing itself happens once per scan.
pub type KvIter<'a> = Box<dyn Iterator<Item = KvResult> + 'a>;

/// The keyspaces a view reads. The memory and embedding keyspaces are not here: they are never read
/// through a view.
///
/// The generic reads (`get`, `prefix`, `range`, `iter`) return stored rows as they are. Those of
/// the content-keyed keyspaces (`symbols_by_*`, `calls_by_*`) name content, not paths, and include
/// rows of content other views hold: read them through the named accessors, which filter them by
/// [`IndexView::live_keys`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Part {
    Meta,
    SymbolsByPath,
    SymbolsByName,
    CallsByPath,
    CallsByCallee,
    ImplementationsByTrait,
    ImplementationsByPath,
    RefsByDef,
    RefsByPath,
    ImportTargets,
    ImportersByProbe,
    CodeBm25Postings,
    CodeBm25ByPath,
    GrepBloom,
    CodeBm25Holders,
}

impl Part {
    /// Every keyspace a view reads, in declaration order. Checked against [`KEYSPACE_MEMTABLE_BYTES`] in
    /// `view_tests`, so a keyspace added to either list alone fails there.
    pub(super) const ALL: [Part; 15] = [
        Part::Meta,
        Part::SymbolsByPath,
        Part::SymbolsByName,
        Part::CallsByPath,
        Part::CallsByCallee,
        Part::ImplementationsByTrait,
        Part::ImplementationsByPath,
        Part::RefsByDef,
        Part::RefsByPath,
        Part::ImportTargets,
        Part::ImportersByProbe,
        Part::CodeBm25Postings,
        Part::CodeBm25ByPath,
        Part::GrepBloom,
        Part::CodeBm25Holders,
    ];

    /// The keyspace's on-disk name.
    pub(super) const fn name(self) -> &'static str {
        match self {
            Part::Meta => "meta",
            Part::SymbolsByPath => "symbols_by_path",
            Part::SymbolsByName => "symbols_by_name",
            Part::CallsByPath => "calls_by_path",
            Part::CallsByCallee => "calls_by_callee",
            Part::ImplementationsByTrait => "implementations_by_trait",
            Part::ImplementationsByPath => "implementations_by_path",
            Part::RefsByDef => "refs_by_def",
            Part::RefsByPath => "refs_by_path",
            Part::ImportTargets => "import_targets",
            Part::ImportersByProbe => "importers_by_probe",
            Part::CodeBm25Postings => "code_bm25_postings",
            Part::CodeBm25ByPath => "code_bm25_by_path",
            Part::GrepBloom => "grep_bloom",
            Part::CodeBm25Holders => "code_bm25_holders",
        }
    }
}

impl IndexDb {
    pub(super) fn keyspace(&self, part: Part) -> &Scoped {
        match part {
            Part::Meta => &self.meta,
            Part::SymbolsByPath => &self.symbols_by_path,
            Part::SymbolsByName => &self.symbols_by_name,
            Part::CallsByPath => &self.calls_by_path,
            Part::CallsByCallee => &self.calls_by_callee,
            Part::ImplementationsByTrait => &self.implementations_by_trait,
            Part::ImplementationsByPath => &self.implementations_by_path,
            Part::RefsByDef => &self.refs_by_def,
            Part::RefsByPath => &self.refs_by_path,
            Part::ImportTargets => &self.import_targets,
            Part::ImportersByProbe => &self.importers_by_probe,
            Part::CodeBm25Postings => &self.code_bm25_postings,
            Part::CodeBm25ByPath => &self.code_bm25_by_path,
            Part::GrepBloom => &self.grep_bloom,
            Part::CodeBm25Holders => &self.code_bm25_holders,
        }
    }

    /// Raw keyspace access for tests that assert on stored rows or drive the name dictionaries.
    #[cfg(test)]
    pub(crate) fn keyspace_for_test(&self, part: Part) -> &fjall::Keyspace {
        self.keyspace(part).raw()
    }

    /// The keyspace as this view reads it, for tests that drive the name dictionaries.
    #[cfg(test)]
    pub(crate) fn scoped_for_test(&self, part: Part) -> &Scoped {
        self.keyspace(part)
    }

    #[cfg(test)]
    pub(crate) fn callee_dict(&self) -> &std::sync::Arc<name_dict::NameDict> {
        &self.callee_names
    }

    #[cfg(test)]
    pub(crate) fn trait_dict(&self) -> &std::sync::Arc<name_dict::NameDict> {
        &self.trait_names
    }

    /// A borrowing [`IndexView`] over this database.
    pub fn view(&self) -> StandaloneView<&IndexDb> {
        StandaloneView(self)
    }
}

/// Read access to the index. See the module docs.
pub trait IndexView: Send + Sync {
    /// Point lookup of `key` in `part`.
    fn get(&self, part: Part, key: &[u8]) -> Result<Option<IndexBytes>, IndexError>;

    /// Every row of `part` whose key starts with `prefix`, in key order.
    fn prefix<'a>(&'a self, part: Part, prefix: &[u8]) -> KvIter<'a>;

    /// Rows of `part` between the bounds, in key order.
    fn range<'a>(&'a self, part: Part, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> KvIter<'a>;

    /// Every row of `part`, in key order.
    fn iter<'a>(&'a self, part: Part) -> KvIter<'a>;

    /// The content keys this view holds, with their paths ([`super::live_keys`]). Every read of a
    /// content-keyed keyspace (`symbols_by_*`, `calls_by_*`) filters through them.
    fn live_keys(&self) -> std::sync::Arc<super::live_keys::LiveKeys>;

    /// Hit keys of `calls_by_callee` after `cursor_after` (a hit or site key), in key order, whose
    /// callee name may contain `needle` (callers still filter by name): each call site of each row
    /// once per live holder, `site ‖ path` ([`super::keys_content`]); a site no holder is live for
    /// comes back as its bare site key, which names no holder; values are empty. `lower` is ignored:
    /// the position is the cursor's.
    /// Served from the callee-name dictionary when it is built.
    fn callee_keys_matching<'a>(
        &'a self,
        needle: &str,
        cursor_after: Option<&[u8]>,
        lower: Bound<Vec<u8>>,
    ) -> KvIter<'a>;

    /// [`Self::callee_keys_matching`] for the trait names of `implementations_by_trait`.
    fn trait_keys_matching<'a>(
        &'a self,
        needle: &str,
        cursor_after: Option<&[u8]>,
        lower: Bound<Vec<u8>>,
    ) -> KvIter<'a>;

    /// True when the view holds no symbol row: none of its live content keys has one. Stops at the
    /// first key that does.
    ///
    /// Used by the MCP startup auto-scan to detect a present-but-empty index (e.g. a
    /// `views/<view>/index.fjall/` wiped out-of-band while the msgpack `index.msgpack` survived). The
    /// in-RAM map cache then looks populated while the index-backed tools would silently return
    /// nothing, so a rescan is warranted.
    fn symbols_index_is_empty(&self) -> bool {
        let live = self.live_keys();
        !live
            .keys()
            .any(|key| self.prefix(Part::SymbolsByPath, key).next().is_some())
    }

    /// The call rows of `content`, in start-byte order: keys `content key ‖ start`, values msgpack
    /// `Call`. The caller names the content: a hit key carries it, and a lookup by path takes it from
    /// one [`Self::live_keys`] snapshot for the whole query.
    fn calls_in_content<'a>(&'a self, content: &keys::ContentKey) -> KvIter<'a> {
        self.prefix(Part::CallsByPath, content)
    }

    /// The msgpack `Call` starting at `start_byte` in `content`.
    fn call_at(&self, content: &keys::ContentKey, start_byte: u32) -> Result<Option<IndexBytes>, IndexError> {
        self.get(Part::CallsByPath, &keys::content_row(content, start_byte))
    }

    /// Resolved uses of the definition at `(def_path, def_start)`: the scope/import-resolved backing
    /// for `find_references`, each as `(use_path, use_start)`. Empty when the definition has no
    /// resolved uses (or resolution never ran for its file).
    fn references_to(&self, def_path: &RelPath, def_start: u32) -> Vec<(RelPath, u32)> {
        let prefix = keys::refs_by_def_prefix(def_path, def_start);
        self.prefix(Part::RefsByDef, &prefix)
            .filter_map(|row| row.ok())
            .filter_map(|(k, _)| keys::parse_ref_by_def(&k))
            .map(|(_, _, use_path, use_start)| (use_path, use_start))
            .collect()
    }

    /// The definition the use at `(use_path, use_start)` resolves to; backs `goto_definition`.
    /// Cross-file definitions take precedence over the local import binding that an imported use also
    /// resolves through. `None` when the position isn't a resolved reference.
    fn definition_of(&self, use_path: &RelPath, use_start: u32) -> Option<(RelPath, u32)> {
        let prefix = keys::refs_by_use_prefix(use_path, use_start);
        let mut local = None;
        for row in self.prefix(Part::RefsByPath, &prefix) {
            if let Ok((k, _)) = row
                && let Some((_use_path, _use_start, def_path, def_start)) = keys::parse_ref_by_path(&k)
            {
                if def_path != *use_path {
                    return Some((def_path, def_start));
                }
                local = Some((def_path, def_start));
            }
        }
        local
    }

    /// The stitched bindings of `importer`: `(local_start, def_path, def_start)`.
    fn import_targets_of(&self, importer: &RelPath) -> Vec<(u32, RelPath, u32)> {
        self.prefix(Part::ImportTargets, &keys::import_targets_prefix(importer))
            .filter_map(|row| row.ok())
            .filter_map(|(k, v)| keys::parse_import_target(&k, &v))
            .map(|(_, local_start, def_path, def_start)| (local_start, def_path, def_start))
            .collect()
    }

    /// The file-level import edges of every importer this view's last stitch resolved, as
    /// `(importer, edges)` in importer order: the input of the import graph. Complete only when
    /// [`Self::import_probes_complete`]; a file in a language with no resolver has no row. One walk
    /// of `import_targets`, whose other rows (one per resolved binding, one manifest per importer)
    /// are skipped. Each row is decoded as it is reached, so a caller that folds them never holds
    /// them all.
    fn file_edge_rows<'a>(&'a self) -> Box<dyn Iterator<Item = (RelPath, keys::FileEdges)> + 'a> {
        Box::new(
            self.iter(Part::ImportTargets)
                .filter_map(|row| row.ok())
                .filter_map(|(k, v)| keys::parse_file_edges(&k, &v)),
        )
    }

    /// [`Self::file_edge_rows`], collected.
    fn file_edges(&self) -> Vec<(RelPath, keys::FileEdges)> {
        self.file_edge_rows().collect()
    }

    /// Every importer whose resolution consulted `path` (repo-relative, forward-slashed) or one of its
    /// ancestor directories: a prefix scan of `importers_by_probe` per candidate, O(importers).
    ///
    /// The ancestors matter because the resolver does not descend into a directory it found missing.
    /// An importer that probed `util` and found nothing never probed `util/index.ts`, so adding
    /// `util/index.ts` must reach it through the directory. (A directory that existed when the
    /// importer resolved is never recorded, so a path under an existing directory costs only empty
    /// scans.)
    fn importers_probing(&self, path: &str) -> Vec<RelPath> {
        probe_candidates(path)
            .flat_map(|candidate| {
                let prefix = keys::importers_by_probe_prefix(&keys::probe_hash(candidate));
                self.prefix(Part::ImportersByProbe, &prefix)
                    .filter_map(|row| row.ok())
                    .filter_map(|(k, _)| keys::parse_importer_by_probe(&k))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Whether any importer consulted `path` or an ancestor directory of it (see
    /// [`Self::importers_probing`]). Cheap enough to gate a rescan on a non-indexed file
    /// (`tsconfig.json`, `package.json`) the watcher saw change.
    fn is_probed(&self, path: &str) -> bool {
        probe_candidates(path).any(|candidate| {
            let prefix = keys::importers_by_probe_prefix(&keys::probe_hash(candidate));
            self.prefix(Part::ImportersByProbe, &prefix).next().is_some()
        })
    }

    /// Whether the probe keyspaces cover every importer (a wholesale resolve pass completed since they
    /// were created). When false, the incremental pass must run the wholesale one first.
    fn import_probes_complete(&self) -> bool {
        self.get(Part::Meta, META_IMPORT_PROBE_VER)
            .ok()
            .flatten()
            .is_some_and(|v| v.first() == Some(&IMPORT_PROBE_VER))
    }

    /// Symbols whose name starts with `name` (length-prefixed keys isolate `Foo` from `Foobar`), as
    /// `(name, kind, path, start_byte)`, capped at `cap`. Backs the code-search exact lane: an
    /// identifier-shaped query resolves to the symbols that define it, which then map to their owning
    /// chunks. The returned `name` lets the caller rank exact-name matches ahead of longer prefix
    /// matches. `start_byte` is the L1 `Symbol.start_byte` (node start), which falls inside the
    /// symbol's owning chunk span.
    fn symbols_by_name_lookup(&self, name: &str, cap: usize) -> Vec<(String, SymbolKind, RelPath, u32)> {
        let prefix = keys::symbols_by_name_prefix(name);
        let rows = self
            .prefix(Part::SymbolsByName, &prefix)
            .map(|row| row.map(|(key, _)| key));
        symbols_from_rows(rows, &self.live_keys(), name, &prefix, cap)
    }

    /// Corpus-global BM25 stats for the code-search keyword lane: `(N, total_len)` where `N` is the
    /// number of indexed chunks and `total_len` the sum of their token lengths (so `avgdl = total_len
    /// / N`). `None` (or `N == 0`) means the BM25 index is empty: no chunks were indexed, or
    /// [`IndexDb::recompute_bm25_stats`] never ran.
    fn bm25_stats(&self) -> Option<(u64, u64)> {
        let stat = |key: &[u8]| {
            self.get(Part::Meta, key)
                .ok()
                .flatten()
                .and_then(|b| <[u8; 8]>::try_from(&b[..]).ok())
                .map(u64::from_be_bytes)
        };
        let n = stat(META_BM25_DOC_COUNT)?;
        Some((n, stat(META_BM25_TOTAL_LEN).unwrap_or(0)))
    }

    /// The BM25 chunks this view holds: the keys of its `code_bm25_by_path` rows, each with its
    /// content hash. The scorer reads only postings of these keys.
    fn bm25_live(&self) -> Result<std::sync::Arc<super::bm25_live::Bm25Live>, IndexError> {
        super::bm25_live::Bm25Live::from_rows(self.iter(Part::CodeBm25ByPath)).map(std::sync::Arc::new)
    }

    /// The stored trigram bloom row for the content `hash_hex`.
    fn grep_bloom_row(&self, hash_hex: &str) -> Option<IndexBytes> {
        self.get(Part::GrepBloom, hash_hex.as_bytes()).ok().flatten()
    }

    /// For each of `entries`, whether the file can be SKIPPED by a grep for `needle`: its content has
    /// an intact row, the row rejects the needle, and the live file still has the stamp the entry was
    /// indexed under. Rows are fetched and dropped one at a time, in parallel; nothing is held between
    /// files.
    fn grep_skip_verdicts(&self, root: &Path, entries: &[GrepEntry], needle: &Needle) -> Vec<bool> {
        super::grep_bloom::skip_verdicts(|hash| self.grep_bloom_row(hash), root, entries, needle)
    }
}

/// [`IndexView::symbols_by_name_lookup`] over the keys of the `symbols_by_name` rows under
/// `prefix`: one hit per live holder of each row, no hit key composed, and a row no live path holds
/// skipped before anything is decoded.
fn symbols_from_rows(
    rows: impl Iterator<Item = Result<IndexBytes, IndexError>>,
    live: &super::live_keys::LiveKeys,
    name: &str,
    prefix: &[u8],
    cap: usize,
) -> Vec<(String, SymbolKind, RelPath, u32)> {
    if prefix.len() != 2 + name.len() {
        // Longer than a key holds: no symbol is stored under it. ~keep
        return Vec::new();
    }
    let mut out = Vec::with_capacity(cap.min(256));
    let mut live = live.cursor();
    for row in rows {
        if out.len() >= cap {
            break;
        }
        let Some((kind, content, start)) = row.ok().and_then(|key| keys::parse_symbol_row(&key)) else {
            continue;
        };
        let holders = live.holders(&content);
        let holders = &holders[..holders.len().min(cap - out.len())];
        if let Some(((_, last), rest)) = holders.split_last() {
            // The length-prefixed name is the whole name: every row under it is named `name`. ~keep
            let found = name.to_owned();
            out.extend(rest.iter().map(|(_, path)| (found.clone(), kind, path.clone(), start)));
            out.push((found, kind, last.clone(), start));
        }
    }
    out
}

/// The view of one [`IndexDb`] handle, read as is. `T` is `&IndexDb` to borrow it or `IndexDb` to
/// own a handle (clones share the same keyspaces).
pub struct StandaloneView<T = IndexDb>(pub(crate) T);

impl<T: Borrow<IndexDb>> StandaloneView<T> {
    fn db(&self) -> &IndexDb {
        self.0.borrow()
    }
}

impl<T: Borrow<IndexDb> + Send + Sync> IndexView for StandaloneView<T> {
    fn get(&self, part: Part, key: &[u8]) -> Result<Option<IndexBytes>, IndexError> {
        self.db().keyspace(part).get(key)
    }

    fn prefix<'a>(&'a self, part: Part, prefix: &[u8]) -> KvIter<'a> {
        self.db().keyspace(part).prefix(prefix)
    }

    fn range<'a>(&'a self, part: Part, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> KvIter<'a> {
        self.db().keyspace(part).range(lower, upper)
    }

    fn iter<'a>(&'a self, part: Part) -> KvIter<'a> {
        self.db().keyspace(part).iter()
    }

    fn bm25_live(&self) -> Result<std::sync::Arc<super::bm25_live::Bm25Live>, IndexError> {
        self.db().bm25_live()
    }

    fn live_keys(&self) -> std::sync::Arc<super::live_keys::LiveKeys> {
        self.db().live_keys()
    }

    fn symbols_by_name_lookup(&self, name: &str, cap: usize) -> Vec<(String, SymbolKind, RelPath, u32)> {
        let prefix = keys::symbols_by_name_prefix(name);
        let db = self.db();
        // Keys only, statically dispatched: the rows' values are empty. ~keep
        symbols_from_rows(
            db.symbols_by_name.prefix_keys(&prefix),
            &db.live_keys(),
            name,
            &prefix,
            cap,
        )
    }

    fn callee_keys_matching<'a>(
        &'a self,
        needle: &str,
        cursor_after: Option<&[u8]>,
        _lower: Bound<Vec<u8>>,
    ) -> KvIter<'a> {
        let db = self.db();
        let (lower, resume) = super::live_keys::resume_after(cursor_after, keys::NameRow::Call);
        let rows = name_dict::name_ordered_keys(&db.calls_by_callee, &db.callee_names, needle, lower);
        Box::new(super::live_keys::ExpandHits::new(
            rows,
            db.live_keys(),
            keys::NameRow::Call,
            resume,
        ))
    }

    fn trait_keys_matching<'a>(
        &'a self,
        needle: &str,
        _cursor_after: Option<&[u8]>,
        lower: Bound<Vec<u8>>,
    ) -> KvIter<'a> {
        let db = self.db();
        name_dict::name_ordered_keys(&db.implementations_by_trait, &db.trait_names, needle, lower)
    }
}

#[cfg(test)]
pub(crate) use recording::{ReadLog, RecordingView};

/// A view that records which reads reach it, so a test can prove a code path is routed through
/// [`IndexView`].
#[cfg(test)]
mod recording {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Shared handle on the reads a [`RecordingView`] has served, as `"<op>[:<keyspace>]"` strings.
    #[derive(Clone, Default)]
    pub(crate) struct ReadLog(Arc<Mutex<Vec<String>>>);

    impl ReadLog {
        fn record(&self, op: &str, part: Option<Part>) {
            let entry = part.map_or_else(|| op.to_owned(), |p| format!("{op}:{}", p.name()));
            self.0.lock().expect("read log").push(entry);
        }

        pub(crate) fn entries(&self) -> Vec<String> {
            self.0.lock().expect("read log").clone()
        }

        pub(crate) fn clear(&self) {
            self.0.lock().expect("read log").clear();
        }
    }

    /// Delegates every read to a [`StandaloneView`] after logging it.
    pub(crate) struct RecordingView {
        inner: StandaloneView<IndexDb>,
        log: ReadLog,
    }

    impl RecordingView {
        pub(crate) fn new(db: IndexDb) -> (Self, ReadLog) {
            let log = ReadLog::default();
            (
                Self {
                    inner: StandaloneView(db),
                    log: log.clone(),
                },
                log,
            )
        }
    }

    impl IndexView for RecordingView {
        fn get(&self, part: Part, key: &[u8]) -> Result<Option<IndexBytes>, IndexError> {
            self.log.record("get", Some(part));
            self.inner.get(part, key)
        }

        fn prefix<'a>(&'a self, part: Part, prefix: &[u8]) -> KvIter<'a> {
            self.log.record("prefix", Some(part));
            self.inner.prefix(part, prefix)
        }

        fn range<'a>(&'a self, part: Part, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> KvIter<'a> {
            self.log.record("range", Some(part));
            self.inner.range(part, lower, upper)
        }

        fn iter<'a>(&'a self, part: Part) -> KvIter<'a> {
            self.log.record("iter", Some(part));
            self.inner.iter(part)
        }

        fn bm25_live(&self) -> Result<Arc<crate::index::bm25_live::Bm25Live>, IndexError> {
            self.log.record("bm25_live", None);
            self.inner.bm25_live()
        }

        fn live_keys(&self) -> Arc<crate::index::live_keys::LiveKeys> {
            self.inner.live_keys()
        }

        fn callee_keys_matching<'a>(
            &'a self,
            needle: &str,
            cursor_after: Option<&[u8]>,
            lower: Bound<Vec<u8>>,
        ) -> KvIter<'a> {
            self.log.record("callee_keys_matching", None);
            self.inner.callee_keys_matching(needle, cursor_after, lower)
        }

        fn trait_keys_matching<'a>(
            &'a self,
            needle: &str,
            cursor_after: Option<&[u8]>,
            lower: Bound<Vec<u8>>,
        ) -> KvIter<'a> {
            self.log.record("trait_keys_matching", None);
            self.inner.trait_keys_matching(needle, cursor_after, lower)
        }
    }
}
