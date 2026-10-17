//! Cross-file resolution stitch (feature `code-intel-js` or `code-intel-stack`).
//!
//! The per-file resolve pass caches each file's *intra*-file resolved edges plus its import and
//! export lists (see [`crate::intel::model::FileResolvedRefs`]). This module runs once at the end
//! of that pass and computes the piece those per-file facts deliberately omit: the **cross-file**
//! edge that links an importer's local binding to the exported definition it actually refers to.
//!
//! The join is:
//!
//! 1. For each importer file, pick the [`SpecifierResolver`] for its language and resolve every
//!    runtime (`is_type == false`) [`ImportEdge`]'s module `specifier` to a repo-relative target
//!    file (JS/TS: Node/tsconfig resolution via oxc; Python/Java: package/source-root path
//!    arithmetic). An importer whose language has no compiled-in resolver is skipped.
//! 2. Require that target to be an indexed file.
//! 3. In the target file's export list, find the [`ExportEdge`] whose `name` matches the import's
//!    `imported` name (or `"default"` for default / namespace imports, whose `imported` is `None`).
//! 4. Emit a cross-file edge — **def** = `(target, export.name_start)`, **use** =
//!    `(importer, import.local_start)` — into `refs_by_def` + `refs_by_path`.
//!
//! Besides the edges, each importer's stitch records what it depended on: the `import_targets` row
//! per resolved import binding, and in `importers_by_probe` every path the resolver consulted (hit
//! targets, re-export hop files, failed candidates, `tsconfig.json` / `package.json`). That reverse
//! map is what lets the incremental pass re-stitch exactly the importers a changed path can affect.
//!
//! Best-effort: any resolver miss or unindexed target is logged at debug and skipped; a commit
//! failure warns but never aborts the scan. Idempotency across re-scans is documented on
//! [`crate::index::writer::IndexWriter::upsert_cross_file_edge`].

use std::cell::RefCell;
use std::ops::Deref;
use std::path::Path;
use std::rc::Rc;

use ahash::AHashMap;

use crate::index::IndexDb;
use crate::intel::model::{ExportEdge, ImportEdge, ResolvedEdge};
use crate::intel::resolver::{ProbeSet, SpecifierResolver};
use crate::intel::stage_budget::BudgetedWriter;
use crate::store::Store;

/// Import/export facts for one file, harvested during the resolve pass's per-file loop so the
/// join needs no second blob read. Only files that import or export something are kept.
pub struct FileFacts {
    pub imports: Vec<ImportEdge>,
    pub exports: Vec<ExportEdge>,
    /// This file's in-file use sites of an imported name — intra edges whose `def_start` is one of
    /// the file's import bindings (`local_start`). The stitch redirects each of these across the
    /// import boundary so a `foo()` call resolves to the *exported* definition, not merely the
    /// `import` statement. Pre-filtered to import-bound edges at harvest time to bound memory.
    pub import_uses: Vec<ResolvedEdge>,
}

/// Commit the cross-file edge batch in bounded chunks, matching the primary scan's
/// `INDEX_COMMIT_BATCH`: caps peak memory and periodically releases Fjall's write lock so a
/// concurrent MCP reader isn't blocked for the whole stitch. A count of edges is not itself a
/// memory bound — [`BudgetedWriter`] adds the byte bounds, including the process-wide staged-byte
/// ceiling this staging was previously invisible to.
const COMMIT_BATCH: usize = 256;

/// How far the join follows a re-export chain before giving up. A name imported through a package
/// `__init__.py` / a TS barrel that itself re-imports it from the defining module is one hop; real
/// chains are shallow, so a small bound keeps a pathological or cyclic chain from spinning while
/// covering every practical re-export (django's `from django.db.models import QuerySet`, etc.).
const MAX_REEXPORT_HOPS: usize = 8;

/// Where the stitch finds a file's import/export facts: the importers handed in (`resident`), and
/// otherwise the file's persisted `.rref` blob, read at most once. The blob path is what keeps the
/// incremental pass O(affected): a re-export hop file that is not itself being re-stitched is loaded
/// on demand instead of the whole corpus being loaded up front.
struct FactsCatalog<'a> {
    store: &'a Store,
    resident: &'a AHashMap<String, FileFacts>,
    loaded: RefCell<AHashMap<String, Option<Rc<FileFacts>>>>,
    export_maps: RefCell<AHashMap<String, Rc<AHashMap<String, u32>>>>,
}

/// A borrowed-or-shared view of one file's [`FileFacts`].
enum Facts<'a> {
    Resident(&'a FileFacts),
    Loaded(Rc<FileFacts>),
}

impl Deref for Facts<'_> {
    type Target = FileFacts;

    fn deref(&self) -> &FileFacts {
        match self {
            Self::Resident(facts) => facts,
            Self::Loaded(facts) => facts,
        }
    }
}

impl<'a> FactsCatalog<'a> {
    fn new(store: &'a Store, resident: &'a AHashMap<String, FileFacts>) -> Self {
        Self {
            store,
            resident,
            loaded: RefCell::default(),
            export_maps: RefCell::default(),
        }
    }

    /// `key`'s facts, or `None` when it neither imports nor exports anything (or is unindexed).
    fn facts(&self, key: &str) -> Option<Facts<'a>> {
        if let Some(facts) = self.resident.get(key) {
            return Some(Facts::Resident(facts));
        }
        self.loaded
            .borrow_mut()
            .entry(key.to_string())
            .or_insert_with(|| self.load(key).map(Rc::new))
            .clone()
            .map(Facts::Loaded)
    }

    fn load(&self, key: &str) -> Option<FileFacts> {
        let entry = self.store.lookup(key)?;
        let refs = self.store.read_resolved_by_hex(&entry.hash_hex).ok()??;
        if refs.imports.is_empty() && refs.exports.is_empty() {
            return None;
        }
        Some(FileFacts {
            imports: refs.imports,
            exports: refs.exports,
            import_uses: Vec::new(),
        })
    }

    /// The `name_start` of `key`'s export called `name`.
    fn export_start(&self, key: &str, name: &str) -> Option<u32> {
        let cached = self.export_maps.borrow().get(key).cloned();
        let map = match cached {
            Some(map) => map,
            None => {
                let facts = self.facts(key)?;
                let map: Rc<AHashMap<String, u32>> =
                    Rc::new(facts.exports.iter().map(|e| (e.name.clone(), e.name_start)).collect());
                self.export_maps.borrow_mut().insert(key.to_string(), Rc::clone(&map));
                map
            }
        };
        map.get(name).copied()
    }
}

/// Resolve `wanted` to its ultimate exported definition starting from `target_rel`, transparently
/// following re-export chains: a file that does not directly export `wanted` but IMPORTS it (a
/// package `__init__.py` re-export, a TS barrel) forwards to that import's target, and so on.
/// Returns `(defining_file, name_start)` — the file that actually defines the export and the byte
/// the join keys on — or `None` when the name is neither a direct export nor a followable re-export
/// within [`MAX_REEXPORT_HOPS`]. Cycle-guarded on `(file, name)`.
///
/// Re-exports stay within one language (a Python import resolves to a Python file), so the importer's
/// `resolver` is valid for every hop; each hop resolves relative to the current file in the chain.
/// Every path a hop consults lands in the importer's `probes`: a change to any file in the chain
/// must re-stitch the importer at the head of it.
fn resolve_export_transitively(
    root: &Path,
    store: &Store,
    catalog: &FactsCatalog<'_>,
    resolver: &SpecifierResolver,
    target_rel: crate::path::RelPath,
    wanted: &str,
    probes: &mut ProbeSet,
) -> Option<(crate::path::RelPath, u32)> {
    let mut current_rel = target_rel;
    let mut wanted = wanted.to_string();
    let mut visited: ahash::AHashSet<(String, String)> = ahash::AHashSet::new();
    for _ in 0..MAX_REEXPORT_HOPS {
        let current_key = current_rel.as_str()?.to_string();
        if !visited.insert((current_key.clone(), wanted.clone())) {
            return None;
        }
        if let Some(name_start) = catalog.export_start(&current_key, &wanted) {
            return Some((current_rel, name_start));
        }
        let facts = catalog.facts(&current_key)?;
        let reexport = facts
            .imports
            .iter()
            .find(|import| !import.is_type && import.local == wanted)?;
        let next_rel = resolver.resolve(root, &current_key, reexport, probes)?;
        store.lookup(&next_rel)?;
        wanted = reexport.imported.clone().unwrap_or(wanted);
        current_rel = next_rel;
    }
    None
}

/// Stitch cross-file resolved edges for every importer in `importers` into the index, and record
/// each importer's `import_targets` and `importers_by_probe` rows. Returns whether every batch
/// committed.
///
/// Runs after the resolve pass's per-file upserts have committed, so each importer's previous-scan
/// edges and probe rows are already purged (see the idempotency note on `upsert_cross_file_edge`).
/// Best-effort: resolver misses and commit failures are logged and swallowed so the scan still
/// succeeds with the name-based fallback intact.
///
/// `importers` holds the facts of the files being (re)stitched; a file reached only as a re-export
/// hop is loaded from its blob on demand. Each importer's language (from the primary index)
/// selects a [`SpecifierResolver`]; importers in a language with no compiled-in resolver are
/// skipped. Resolvers are built once per language and cached for the duration of the stitch.
pub fn stitch_cross_file_edges(
    root: &Path,
    store: &Store,
    index_db: &IndexDb,
    importers: &AHashMap<String, FileFacts>,
) -> bool {
    if importers.is_empty() {
        return true;
    }
    let catalog = FactsCatalog::new(store, importers);
    let mut resolvers: AHashMap<String, Option<SpecifierResolver>> = AHashMap::new();
    let mut batch = BudgetedWriter::new(index_db, COMMIT_BATCH, "cross-file stitch");
    let mut edges = 0usize;

    let mut importer_keys: Vec<&String> = importers.keys().collect();
    importer_keys.sort_unstable();
    for importer_key in importer_keys {
        let importer_facts = &importers[importer_key];
        if importer_facts.imports.iter().all(|import| import.is_type) {
            continue;
        }
        let Some(language) = store.lookup(importer_key.as_str()).map(|e| e.language.clone()) else {
            continue;
        };
        let resolver = resolvers
            .entry(language)
            .or_insert_with_key(|lang| SpecifierResolver::for_language(lang));
        let Some(resolver) = resolver.as_ref() else {
            continue;
        };
        resolver.begin_importer();
        let importer_rel = crate::path::RelPath::from(importer_key.as_str());
        let mut probes = ProbeSet::default();
        let mut targets: Vec<(u32, crate::path::RelPath, u32)> = Vec::new();

        for import in &importer_facts.imports {
            if import.is_type {
                continue;
            }
            let Some(target_rel) = resolver.resolve(root, importer_key, import, &mut probes) else {
                tracing::debug!(
                    importer = %importer_rel,
                    specifier = %import.specifier,
                    "cross-file stitch: specifier did not resolve — skipping"
                );
                continue;
            };
            if store.lookup(&target_rel).is_none() {
                continue;
            }

            let Some(wanted) = import.imported.as_deref().or_else(|| resolver.default_export_name()) else {
                continue;
            };
            let Some((def_rel, name_start)) =
                resolve_export_transitively(root, store, &catalog, resolver, target_rel, wanted, &mut probes)
            else {
                continue;
            };
            targets.push((import.local_start, def_rel.clone(), name_start));
            let mut use_starts: Vec<u32> = vec![import.local_start];
            for edge in &importer_facts.import_uses {
                if edge.def_start == import.local_start && !use_starts.contains(&edge.use_start) {
                    use_starts.push(edge.use_start);
                }
            }
            for use_start in use_starts {
                let staged = batch
                    .writer()
                    .upsert_cross_file_edge(&def_rel, name_start, &importer_rel, use_start);
                match staged {
                    Ok(()) => {
                        edges += 1;
                        batch.item_staged();
                    }
                    Err(error) => tracing::warn!(
                        importer = %importer_rel,
                        target = %def_rel,
                        %error,
                        "cross-file stitch: failed to stage edge — skipping"
                    ),
                }
            }
        }

        if !probes.is_empty() || !targets.is_empty() {
            batch
                .writer()
                .upsert_import_probes(&importer_rel, &targets, &probes.hashes());
            batch.item_staged();
        }
    }

    let committed = batch.finish();
    if committed {
        tracing::debug!(edges, "cross-file stitch: staged cross-file resolved edges");
    }
    committed
}
