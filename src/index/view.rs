//! Read-only view over the fjall index.
//!
//! Every read of the index goes through [`IndexView`] so a later phase can answer from a shared base
//! index plus a per-worktree overlay without touching a call site (ADR-0016, phase A3). Writers keep
//! using [`IndexWriter`](super::writer::IndexWriter) on the concrete [`IndexDb`].
//!
//! The trait has two layers. The generic layer (`get` / `prefix` / `range` / `iter`, addressed by
//! [`Part`]) is what a merging implementation has to provide; the named accessors are the lookups the
//! read path performs, written once as default methods over that layer so a layered view inherits them
//! (ADR-0016 A5 "Merge contract"). No fjall type appears in a signature: values come back as
//! [`IndexBytes`], which a later mapped base can serve without a copy.

use std::borrow::Borrow;
use std::ops::{Bound, Deref};
use std::path::Path;

use fjall::Keyspace;

use super::grep_bloom::{GrepEntry, Needle};
use super::{
    IMPORT_PROBE_VER, IndexDb, IndexError, META_BM25_DOC_COUNT, META_BM25_TOTAL_LEN, META_IMPORT_PROBE_VER, keys,
    name_dict, probe_candidates,
};
use crate::extract::SymbolKind;
use crate::path::RelPath;

/// Bytes read from the index: a key or a value.
///
/// A refcounted handle, not a copy: today it wraps the fjall block slice the read already produced, and
/// a mapped immutable base will add a variant borrowing the mapping, so a merging view hands either
/// out per row without allocating. The enum is private so no backend type is nameable from a signature.
#[derive(Clone)]
pub struct IndexBytes(Repr);

#[derive(Clone)]
enum Repr {
    Fjall(fjall::Slice),
}

impl IndexBytes {
    fn from_fjall(slice: fjall::Slice) -> Self {
        Self(Repr::Fjall(slice))
    }
}

impl Deref for IndexBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.0 {
            Repr::Fjall(slice) => slice,
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

/// The keyspaces a layered index merges. The memory, proposal and embedding keyspaces are not here:
/// they are per-repo-group or per-worktree state and never layered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Part {
    Meta,
    SymbolsByPath,
    SymbolsByName,
    CallsByPath,
    CallsByCallee,
    ImportsByModule,
    ImportsByPath,
    ImplementationsByTrait,
    ImplementationsByPath,
    RefsByDef,
    RefsByPath,
    ImportTargets,
    ImportersByProbe,
    CodeBm25Postings,
    CodeBm25ByPath,
    GrepBloom,
}

impl Part {
    /// Every layered keyspace, in declaration order. Checked against [`KEYSPACE_MEMTABLE_BYTES`] in
    /// `view_tests`, so a keyspace added to either list alone fails there.
    #[cfg(test)]
    pub(super) const ALL: [Part; 16] = [
        Part::Meta,
        Part::SymbolsByPath,
        Part::SymbolsByName,
        Part::CallsByPath,
        Part::CallsByCallee,
        Part::ImportsByModule,
        Part::ImportsByPath,
        Part::ImplementationsByTrait,
        Part::ImplementationsByPath,
        Part::RefsByDef,
        Part::RefsByPath,
        Part::ImportTargets,
        Part::ImportersByProbe,
        Part::CodeBm25Postings,
        Part::CodeBm25ByPath,
        Part::GrepBloom,
    ];

    /// The keyspace's on-disk name.
    #[cfg(test)]
    pub(super) const fn name(self) -> &'static str {
        match self {
            Part::Meta => "meta",
            Part::SymbolsByPath => "symbols_by_path",
            Part::SymbolsByName => "symbols_by_name",
            Part::CallsByPath => "calls_by_path",
            Part::CallsByCallee => "calls_by_callee",
            Part::ImportsByModule => "imports_by_module",
            Part::ImportsByPath => "imports_by_path",
            Part::ImplementationsByTrait => "implementations_by_trait",
            Part::ImplementationsByPath => "implementations_by_path",
            Part::RefsByDef => "refs_by_def",
            Part::RefsByPath => "refs_by_path",
            Part::ImportTargets => "import_targets",
            Part::ImportersByProbe => "importers_by_probe",
            Part::CodeBm25Postings => "code_bm25_postings",
            Part::CodeBm25ByPath => "code_bm25_by_path",
            Part::GrepBloom => "grep_bloom",
        }
    }
}

impl IndexDb {
    fn keyspace(&self, part: Part) -> &Keyspace {
        match part {
            Part::Meta => &self.meta,
            Part::SymbolsByPath => &self.symbols_by_path,
            Part::SymbolsByName => &self.symbols_by_name,
            Part::CallsByPath => &self.calls_by_path,
            Part::CallsByCallee => &self.calls_by_callee,
            Part::ImportsByModule => &self.imports_by_module,
            Part::ImportsByPath => &self.imports_by_path,
            Part::ImplementationsByTrait => &self.implementations_by_trait,
            Part::ImplementationsByPath => &self.implementations_by_path,
            Part::RefsByDef => &self.refs_by_def,
            Part::RefsByPath => &self.refs_by_path,
            Part::ImportTargets => &self.import_targets,
            Part::ImportersByProbe => &self.importers_by_probe,
            Part::CodeBm25Postings => &self.code_bm25_postings,
            Part::CodeBm25ByPath => &self.code_bm25_by_path,
            Part::GrepBloom => &self.grep_bloom,
        }
    }

    /// Raw keyspace access for tests that assert on stored rows or drive the name dictionaries.
    #[cfg(test)]
    pub(crate) fn keyspace_for_test(&self, part: Part) -> &Keyspace {
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

/// `Some(&dyn IndexView)` for an optional concrete view, for the call sites that take an optional one.
pub(crate) fn as_dyn<V: IndexView>(view: &Option<V>) -> Option<&dyn IndexView> {
    view.as_ref().map(|v| v as &dyn IndexView)
}

pub(super) fn kv(guard: fjall::Guard) -> KvResult {
    let (key, value) = guard.into_inner().map_err(IndexError::from)?;
    Ok((IndexBytes::from_fjall(key), IndexBytes::from_fjall(value)))
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

    /// Keys of `calls_by_callee` after `lower`, in key order, whose callee name may contain `needle`
    /// (callers still filter by name). Served from the callee-name dictionary when it is built.
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

    /// True when the index holds no per-file symbol entries. Cheap: peeks at the first key of
    /// `symbols_by_path` rather than counting.
    ///
    /// Used by the MCP startup auto-scan to detect a present-but-empty index (e.g. a
    /// `views/<view>/index.fjall/` wiped out-of-band while the msgpack `index.msgpack` survived). The
    /// in-RAM map cache then looks populated while the index-backed tools would silently return
    /// nothing, so a rescan is warranted.
    fn symbols_index_is_empty(&self) -> bool {
        self.iter(Part::SymbolsByPath).next().is_none()
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
        let mut out = Vec::new();
        for row in self.prefix(Part::SymbolsByName, &prefix) {
            if out.len() >= cap {
                break;
            }
            if let Ok((k, _)) = row
                && let Some(symbol) = keys::parse_symbol_by_name(&k)
            {
                out.push(symbol);
            }
        }
        out
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

/// The unlayered view: one [`IndexDb`], read as is. `T` is `&IndexDb` to borrow it or `IndexDb` to
/// own a handle (clones share the same keyspaces).
pub struct StandaloneView<T = IndexDb>(pub(crate) T);

impl<T: Borrow<IndexDb>> StandaloneView<T> {
    fn db(&self) -> &IndexDb {
        self.0.borrow()
    }
}

impl<T: Borrow<IndexDb> + Send + Sync> IndexView for StandaloneView<T> {
    fn get(&self, part: Part, key: &[u8]) -> Result<Option<IndexBytes>, IndexError> {
        Ok(self.db().keyspace(part).get(key)?.map(IndexBytes::from_fjall))
    }

    fn prefix<'a>(&'a self, part: Part, prefix: &[u8]) -> KvIter<'a> {
        Box::new(self.db().keyspace(part).prefix(prefix).map(kv))
    }

    fn range<'a>(&'a self, part: Part, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> KvIter<'a> {
        Box::new(self.db().keyspace(part).range::<&[u8], _>((lower, upper)).map(kv))
    }

    fn iter<'a>(&'a self, part: Part) -> KvIter<'a> {
        Box::new(self.db().keyspace(part).iter().map(kv))
    }

    fn callee_keys_matching<'a>(
        &'a self,
        needle: &str,
        cursor_after: Option<&[u8]>,
        lower: Bound<Vec<u8>>,
    ) -> KvIter<'a> {
        let db = self.db();
        name_dict::name_ordered_keys(&db.calls_by_callee, &db.callee_names, needle, cursor_after, lower)
    }

    fn trait_keys_matching<'a>(
        &'a self,
        needle: &str,
        cursor_after: Option<&[u8]>,
        lower: Bound<Vec<u8>>,
    ) -> KvIter<'a> {
        let db = self.db();
        name_dict::name_ordered_keys(
            &db.implementations_by_trait,
            &db.trait_names,
            needle,
            cursor_after,
            lower,
        )
    }
}

#[cfg(test)]
pub(crate) use recording::{ReadLog, RecordingView};

/// A view that records which reads reach it, so a test can prove a code path is routed through
/// [`IndexView`] and a later layered implementation has nothing to bypass.
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
