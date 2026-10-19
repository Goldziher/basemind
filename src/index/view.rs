//! Read-only view over the fjall index.
//!
//! Every read of the index goes through [`IndexView`] so a later phase can answer from a shared base
//! index plus a per-worktree overlay without touching a call site (ADR-0016, phase A3). Writers keep
//! using [`IndexWriter`](super::writer::IndexWriter) on the concrete [`IndexDb`].
//!
//! The trait has two layers. The generic layer (`get` / `prefix` / `range` / `iter`, addressed by
//! [`Part`]) is what a merging implementation has to provide; the named accessors are the lookups the
//! read path performs, each delegating 1:1 to the matching [`IndexDb`] method in [`StandaloneView`].

use std::borrow::Borrow;
use std::ops::Bound;
use std::path::Path;

use fjall::{Keyspace, Slice};

use super::grep_bloom::{GrepEntry, Needle};
use super::{IndexDb, IndexError, name_dict};
use crate::extract::SymbolKind;
use crate::path::RelPath;

/// One `(key, value)` row, or the error that interrupted the walk.
pub type KvResult = Result<(Slice, Slice), IndexError>;

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
    /// Every layered keyspace, in declaration order.
    pub const ALL: [Part; 16] = [
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
    pub const fn name(self) -> &'static str {
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
pub fn as_dyn<V: IndexView>(view: &Option<V>) -> Option<&dyn IndexView> {
    view.as_ref().map(|v| v as &dyn IndexView)
}

pub(crate) fn kv(guard: fjall::Guard) -> KvResult {
    guard.into_inner().map_err(IndexError::from)
}

/// Read access to the index. See the module docs.
pub trait IndexView: Send + Sync {
    /// Point lookup of `key` in `part`.
    fn get(&self, part: Part, key: &[u8]) -> Result<Option<Slice>, IndexError>;

    /// Every row of `part` whose key starts with `prefix`, in key order.
    fn prefix<'a>(&'a self, part: Part, prefix: &[u8]) -> KvIter<'a>;

    /// Rows of `part` between the bounds, in key order.
    fn range<'a>(&'a self, part: Part, lower: Bound<Vec<u8>>, upper: Bound<Vec<u8>>) -> KvIter<'a>;

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

    /// True when the index holds no per-file symbol entries.
    fn symbols_index_is_empty(&self) -> bool;

    /// Resolved uses of the definition at `(def_path, def_start)`.
    fn references_to(&self, def_path: &RelPath, def_start: u32) -> Vec<(RelPath, u32)>;

    /// The definition the use at `(use_path, use_start)` resolves to.
    fn definition_of(&self, use_path: &RelPath, use_start: u32) -> Option<(RelPath, u32)>;

    /// The bindings of `importer`: `(local_start, def_path, def_start)`.
    fn import_targets_of(&self, importer: &RelPath) -> Vec<(u32, RelPath, u32)>;

    /// Importers that probed a path `path` could satisfy.
    fn importers_probing(&self, path: &str) -> Vec<RelPath>;

    /// Whether any importer probed a path `path` could satisfy.
    fn is_probed(&self, path: &str) -> bool;

    /// Whether the probe keyspaces cover every importer.
    fn import_probes_complete(&self) -> bool;

    /// Up to `cap` symbols named `name` (prefix match), as `(name, kind, path, start_byte)`.
    fn symbols_by_name_lookup(&self, name: &str, cap: usize) -> Vec<(String, SymbolKind, RelPath, u32)>;

    /// BM25 corpus statistics `(doc_count, total_len)`; `None` before the first scan.
    fn bm25_stats(&self) -> Option<(u64, u64)>;

    /// The stored trigram bloom row for the content `hash_hex`.
    fn grep_bloom_row(&self, hash_hex: &str) -> Option<Slice> {
        self.get(Part::GrepBloom, hash_hex.as_bytes()).ok().flatten()
    }

    /// For each of `entries`, whether a grep for `needle` can skip the file.
    fn grep_skip_verdicts(&self, root: &Path, entries: &[GrepEntry], needle: &Needle) -> Vec<bool> {
        super::grep_bloom::skip_verdicts(|hash| self.grep_bloom_row(hash), root, entries, needle)
    }
}

/// The unlayered view: one [`IndexDb`], read as is. `T` is `&IndexDb` to borrow it or `IndexDb` to
/// own a handle (clones share the same keyspaces).
pub struct StandaloneView<T = IndexDb>(pub T);

impl<T: Borrow<IndexDb>> StandaloneView<T> {
    fn db(&self) -> &IndexDb {
        self.0.borrow()
    }
}

impl<T: Borrow<IndexDb> + Send + Sync> IndexView for StandaloneView<T> {
    fn get(&self, part: Part, key: &[u8]) -> Result<Option<Slice>, IndexError> {
        Ok(self.db().keyspace(part).get(key)?)
    }

    fn prefix<'a>(&'a self, part: Part, prefix: &[u8]) -> KvIter<'a> {
        Box::new(self.db().keyspace(part).prefix(prefix).map(kv))
    }

    fn range<'a>(&'a self, part: Part, lower: Bound<Vec<u8>>, upper: Bound<Vec<u8>>) -> KvIter<'a> {
        Box::new(self.db().keyspace(part).range::<Vec<u8>, _>((lower, upper)).map(kv))
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

    fn symbols_index_is_empty(&self) -> bool {
        self.db().symbols_index_is_empty()
    }

    fn references_to(&self, def_path: &RelPath, def_start: u32) -> Vec<(RelPath, u32)> {
        self.db().references_to(def_path, def_start)
    }

    fn definition_of(&self, use_path: &RelPath, use_start: u32) -> Option<(RelPath, u32)> {
        self.db().definition_of(use_path, use_start)
    }

    fn import_targets_of(&self, importer: &RelPath) -> Vec<(u32, RelPath, u32)> {
        self.db().import_targets_of(importer)
    }

    fn importers_probing(&self, path: &str) -> Vec<RelPath> {
        self.db().importers_probing(path)
    }

    fn is_probed(&self, path: &str) -> bool {
        self.db().is_probed(path)
    }

    fn import_probes_complete(&self) -> bool {
        self.db().import_probes_complete()
    }

    fn symbols_by_name_lookup(&self, name: &str, cap: usize) -> Vec<(String, SymbolKind, RelPath, u32)> {
        self.db().symbols_by_name_lookup(name, cap)
    }

    fn bm25_stats(&self) -> Option<(u64, u64)> {
        self.db().bm25_stats()
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

        pub(crate) fn saw(&self, entry: &str) -> bool {
            self.0.lock().expect("read log").iter().any(|e| e == entry)
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
        fn get(&self, part: Part, key: &[u8]) -> Result<Option<Slice>, IndexError> {
            self.log.record("get", Some(part));
            self.inner.get(part, key)
        }

        fn prefix<'a>(&'a self, part: Part, prefix: &[u8]) -> KvIter<'a> {
            self.log.record("prefix", Some(part));
            self.inner.prefix(part, prefix)
        }

        fn range<'a>(&'a self, part: Part, lower: Bound<Vec<u8>>, upper: Bound<Vec<u8>>) -> KvIter<'a> {
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

        fn symbols_index_is_empty(&self) -> bool {
            self.log.record("symbols_index_is_empty", None);
            self.inner.symbols_index_is_empty()
        }

        fn references_to(&self, def_path: &RelPath, def_start: u32) -> Vec<(RelPath, u32)> {
            self.log.record("references_to", None);
            self.inner.references_to(def_path, def_start)
        }

        fn definition_of(&self, use_path: &RelPath, use_start: u32) -> Option<(RelPath, u32)> {
            self.log.record("definition_of", None);
            self.inner.definition_of(use_path, use_start)
        }

        fn import_targets_of(&self, importer: &RelPath) -> Vec<(u32, RelPath, u32)> {
            self.log.record("import_targets_of", None);
            self.inner.import_targets_of(importer)
        }

        fn importers_probing(&self, path: &str) -> Vec<RelPath> {
            self.log.record("importers_probing", None);
            self.inner.importers_probing(path)
        }

        fn is_probed(&self, path: &str) -> bool {
            self.log.record("is_probed", None);
            self.inner.is_probed(path)
        }

        fn import_probes_complete(&self) -> bool {
            self.log.record("import_probes_complete", None);
            self.inner.import_probes_complete()
        }

        fn symbols_by_name_lookup(&self, name: &str, cap: usize) -> Vec<(String, SymbolKind, RelPath, u32)> {
            self.log.record("symbols_by_name_lookup", None);
            self.inner.symbols_by_name_lookup(name, cap)
        }

        fn bm25_stats(&self) -> Option<(u64, u64)> {
            self.log.record("bm25_stats", None);
            self.inner.bm25_stats()
        }
    }
}
