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
//!    A target that only forwards the name (a barrel's re-import, `export { x } from` or
//!    `export * from`) is followed to the file that defines it.
//! 4. Emit a cross-file edge — **def** = `(target, export.name_start)`, **use** =
//!    `(importer, import.local_start)` — into `refs_by_def` + `refs_by_path`.
//!
//! Besides the edges, each importer's stitch records what it depended on: the `import_targets` row
//! per resolved import binding, and in `importers_by_probe` every path the resolver consulted (hit
//! targets, re-export hop files, failed candidates, `tsconfig.json` / `package.json`). That reverse
//! map is what lets the incremental pass re-stitch exactly the importers a changed path can affect.
//! It also records the importer's file-level edges ([`FileEdges`]): every indexed file an import
//! resolves to at the first hop, whether or not the name binds an export there (a barrel forwards
//! it), plus the imports that resolved to none. Each edge carries the [`EdgeKinds`] of the imports
//! behind it: a type-only import, or a JS side-effect import, namespace import or `export … from`
//! re-export (a `module_only` import, which joins no export), is flagged as such. A Python `from`
//! import whose name binds no export of the package it names (`from . import views`) names a
//! submodule instead, and that submodule's file is an edge too.
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
use crate::index::keys::{EdgeKinds, FileEdge, FileEdges, MAX_UNRESOLVED_SPECIFIERS};
use crate::intel::model::{ExportEdge, ImportEdge, ReexportEdge, ResolvedEdge};
use crate::intel::resolver::{ProbeSet, SpecifierResolver};
use crate::intel::stage_budget::BudgetedWriter;
use crate::store::Store;

/// Import/export facts for one file, harvested during the resolve pass's per-file loop so the
/// join needs no second blob read. Only files that import or export something are kept.
pub struct FileFacts {
    pub imports: Vec<ImportEdge>,
    pub exports: Vec<ExportEdge>,
    /// Names this file forwards from other modules — the hops a barrel file adds to an import's
    /// resolution.
    pub reexports: Vec<ReexportEdge>,
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

    /// `key`'s facts, or `None` when it neither imports, exports nor re-exports anything (or is
    /// unindexed).
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
        if refs.imports.is_empty() && refs.exports.is_empty() && refs.reexports.is_empty() {
            return None;
        }
        Some(FileFacts {
            imports: refs.imports,
            exports: refs.exports,
            reexports: refs.reexports,
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
/// following re-export chains: a file that does not directly export `wanted` forwards it when a
/// named re-export provides it (`export { a as wanted } from`), when it IMPORTS it (a package
/// `__init__.py` re-export, a TS `import { x } …; export { x }` barrel), or when a star re-export
/// may (`export * from`, every name but `default`). Forwards are tried in that order, so a named
/// re-export beats a local import of the same name (the import is not what the file exports), star
/// re-exports in source order, and the first that reaches a definition wins. Returns `(defining_file, name_start)`
/// — the file that actually defines the export and the byte the join keys on — or why there is none
/// ([`Unbound`]), within [`MAX_REEXPORT_HOPS`] files.
/// Cycle-guarded on `(file, name)` with the fewest hops it was reached at: a barrel that re-exports
/// itself, directly or around a loop, terminates, while a pair first reached down a long chain is
/// searched again when a shorter path reaches it, so the hop limit cut nothing the short path
/// allows.
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
) -> Result<(crate::path::RelPath, u32), Unbound> {
    let mut search = ExportSearch {
        root,
        store,
        catalog,
        resolver,
        probes,
        visited: AHashMap::new(),
    };
    search.find(target_rel, wanted, 0)
}

/// One [`resolve_export_transitively`] walk: the depth-first search over a file's forwards, with
/// the `(file, name)` pairs already visited and the fewest hops each was reached at.
struct ExportSearch<'s, 'a> {
    root: &'s Path,
    store: &'s Store,
    catalog: &'s FactsCatalog<'a>,
    resolver: &'s SpecifierResolver,
    probes: &'s mut ProbeSet,
    visited: AHashMap<(String, String), usize>,
}

type Found = Result<(crate::path::RelPath, u32), Unbound>;

/// The reason a search reports: the first that says more than [`Unbound::Absent`].
fn first_reason(first: Unbound, next: Unbound) -> Unbound {
    if first == Unbound::Absent { next } else { first }
}

impl ExportSearch<'_, '_> {
    /// The definition of `wanted` as seen from `current_rel`, reached `hops` files into the chain.
    fn find(&mut self, current_rel: crate::path::RelPath, wanted: &str, hops: usize) -> Found {
        if hops >= MAX_REEXPORT_HOPS {
            return Err(Unbound::Absent);
        }
        let current_key = current_rel.as_str().ok_or(Unbound::Absent)?.to_string();
        let pair = (current_key.clone(), wanted.to_string());
        if self.visited.get(&pair).is_some_and(|&seen| seen <= hops) {
            return Err(Unbound::Package(current_rel, wanted.to_string()));
        }
        self.visited.insert(pair, hops);
        if let Some(name_start) = self.catalog.export_start(&current_key, wanted) {
            return Ok((current_rel, name_start));
        }
        let facts = self.catalog.facts(&current_key).ok_or(Unbound::Absent)?;
        let named = self.follow_reexports(&current_key, &facts, wanted, hops, true);
        if let Some(named) = named {
            // A named re-export of `wanted` shadows the imports and star re-exports even when its
            // module is outside the index: ECMAScript does not consult a star export once a name matches.
            return named;
        }
        let mut failure = Unbound::Absent;
        let import = facts
            .imports
            .iter()
            .find(|import| !import.is_type && !import.module_only && import.local == wanted);
        if let Some(import) = import {
            if import.imported.is_none() && self.resolver.default_export_name().is_none() {
                return Err(Unbound::Module);
            }
            let next_wanted = import.imported.as_deref().unwrap_or(wanted);
            match self.follow(&current_key, import, next_wanted, hops) {
                Ok(found) => return Ok(found),
                Err(reason) => failure = reason,
            }
        }
        let star = self.follow_reexports(&current_key, &facts, wanted, hops, false);
        match star {
            Some(Ok(found)) => Ok(found),
            Some(Err(reason)) => Err(first_reason(failure, reason)),
            None if import.is_none() => Err(Unbound::Package(current_rel, wanted.to_string())),
            None => Err(failure),
        }
    }

    /// Follow the runtime re-exports of `facts` that forward `wanted`: the named ones when `named`,
    /// else the star ones, in source order. The first that reaches a definition wins; `None` when
    /// none forwards `wanted`, else the reason the last one failed.
    fn follow_reexports(
        &mut self,
        current_key: &str,
        facts: &FileFacts,
        wanted: &str,
        hops: usize,
        named: bool,
    ) -> Option<Found> {
        let mut outcome: Option<Found> = None;
        for reexport in facts
            .reexports
            .iter()
            .filter(|reexport| !reexport.is_type && reexport.named.is_some() == named)
        {
            let Some(next_wanted) = reexport.forwards(wanted) else {
                continue;
            };
            match self.follow(current_key, &reexport.module_request(), next_wanted, hops) {
                Ok(found) => return Some(Ok(found)),
                Err(reason) => {
                    let kept = outcome.take().and_then(Result::err).unwrap_or(Unbound::Absent);
                    outcome = Some(Err(first_reason(kept, reason)));
                }
            }
        }
        outcome
    }

    /// Resolve `request` from `from_key` and continue the search for `wanted` in the indexed file it
    /// names.
    fn follow(&mut self, from_key: &str, request: &ImportEdge, wanted: &str, hops: usize) -> Found {
        let next_rel = self
            .resolver
            .resolve(self.root, from_key, request, self.probes)
            .ok_or(Unbound::Absent)?;
        self.store.lookup(&next_rel).ok_or(Unbound::Absent)?;
        self.find(next_rel, wanted, hops + 1)
    }
}

/// Why [`resolve_export_transitively`] found no definition for a name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Unbound {
    /// No file on the chain binds the name to something followable: it is neither exported nor
    /// imported there, its import leads to no indexed file, or the chain came back to a `(file,
    /// name)` it had followed, as `from . import sub` in a package does for its own submodule. The
    /// name may still be a submodule of the package.
    Absent,
    /// The chain ended at this file, which neither exports nor imports the name, or whose import of
    /// it came back to the file itself. The name may still be a submodule of this package, which is
    /// where the fallback looks, not in the package the import began at.
    Package(crate::path::RelPath, String),
    /// A file on the chain binds the name to a module (`import other.thing as sub`): it names that
    /// module, never a submodule of the package of the same name.
    Module,
}

/// The indexed file `import`'s name names when it is read as a submodule of the module the import
/// names, for a name that binds no export there (see [`SpecifierResolver::resolve_submodule`]).
/// `package` is the indexed file the module resolved to, if any.
fn indexed_submodule(
    root: &Path,
    store: &Store,
    resolver: &SpecifierResolver,
    importer_key: &str,
    package: Option<&crate::path::RelPath>,
    import: &ImportEdge,
    probes: &mut ProbeSet,
) -> Option<crate::path::RelPath> {
    if import.is_type || import.module_only {
        return None;
    }
    resolver
        .resolve_submodule(root, importer_key, package, import, probes)
        .filter(|submodule| store.lookup(submodule).is_some())
}

/// Stitch cross-file resolved edges for every importer in `importers` into the index, and record
/// each importer's `import_targets` and `importers_by_probe` rows. Returns whether every batch
/// committed.
///
/// Runs after the resolve pass's per-file upserts have committed, so each importer's previous-scan
/// edges and probe rows are already purged (see the idempotency note on `upsert_cross_file_edge`).
/// Best-effort: resolver misses and commit failures are logged and swallowed so the scan still
/// succeeds with the name-based fallback intact. Returns false when a commit failed, so the caller
/// can decline to record the resolve pass as complete.
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
        if importer_facts.imports.is_empty() {
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
        let mut file_edges = FileEdgesBuilder::default();

        for import in &importer_facts.imports {
            let resolved = resolver.resolve(root, importer_key, import, &mut probes);
            let target_rel = resolved.clone().filter(|target| store.lookup(target).is_some());
            let Some(target_rel) = target_rel else {
                // The module's file decides whether it has submodules, indexed or not. ~keep
                let package = resolved.as_ref();
                match indexed_submodule(root, store, resolver, importer_key, package, import, &mut probes) {
                    Some(submodule) => file_edges.hit(submodule, import),
                    None => {
                        tracing::debug!(
                            importer = %importer_rel,
                            specifier = %import.specifier,
                            "cross-file stitch: specifier did not resolve to an indexed file — skipping"
                        );
                        file_edges.miss(import);
                    }
                }
                continue;
            };
            // A file is never its own import target: `from . import a` in `pkg/__init__.py` resolves the
            // package to the importer itself. It records no file edge, but the name still binds to the
            // package's own exports first and falls back to a submodule when none does.
            if target_rel.as_str() != Some(importer_key.as_str()) {
                file_edges.hit(target_rel.clone(), import);
            }
            if import.is_type || import.module_only {
                continue;
            }

            let Some(wanted) = import.imported.as_deref().or_else(|| resolver.default_export_name()) else {
                continue;
            };
            let bound =
                resolve_export_transitively(root, store, &catalog, resolver, target_rel.clone(), wanted, &mut probes);
            let (def_rel, name_start) = match bound {
                Ok(bound) => bound,
                Err(Unbound::Module) => continue,
                Err(Unbound::Absent) => continue,
                Err(Unbound::Package(package, name)) => {
                    let terminal = ImportEdge {
                        imported: Some(name),
                        ..import.clone()
                    };
                    if let Some(submodule) = indexed_submodule(
                        root,
                        store,
                        resolver,
                        importer_key,
                        Some(&package),
                        &terminal,
                        &mut probes,
                    ) {
                        file_edges.hit(submodule, import);
                    }
                    continue;
                }
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
        batch.writer().upsert_file_edges(&importer_rel, &file_edges.finish());
        batch.item_staged();
    }

    let committed = batch.finish();
    if committed {
        tracing::debug!(edges, "cross-file stitch: staged cross-file resolved edges");
    }
    committed
}

/// Collects one importer's [`FileEdges`] while its imports resolve. Shared with the file-level
/// module stitch ([`crate::intel::module_resolver`]), whose resolvers classify a miss themselves.
#[derive(Default)]
pub(crate) struct FileEdgesBuilder {
    targets: AHashMap<crate::path::RelPath, TargetImports>,
    external: u32,
    unresolved: u32,
    unresolved_specifiers: Vec<String>,
}

/// The imports of one target seen so far: their kinds and the anchor [`FileEdge::local_start`]
/// takes, the first import of the most real kind.
struct TargetImports {
    kinds: EdgeKinds,
    anchor: u32,
    anchor_rank: u8,
}

/// Which import anchors an edge: a runtime one before a module declaration before a type-only one.
fn anchor_rank(kind: EdgeKinds) -> u8 {
    if kind.runtime() {
        0
    } else if kind.contains(EdgeKinds::MODULE_DECL) {
        1
    } else {
        2
    }
}

impl FileEdgesBuilder {
    /// `import` resolved to the indexed file `target`.
    fn hit(&mut self, target: crate::path::RelPath, import: &ImportEdge) {
        let kind = if import.is_type {
            EdgeKinds::TYPE_ONLY
        } else if import.module_only {
            EdgeKinds::MODULE_ONLY
        } else {
            EdgeKinds::IMPORT
        };
        self.hit_at(target, import.local_start, kind);
    }

    /// An import of `kind` at byte `local_start` resolved to the indexed file `target`.
    pub(crate) fn hit_at(&mut self, target: crate::path::RelPath, local_start: u32, kind: EdgeKinds) {
        let rank = anchor_rank(kind);
        let entry = self.targets.entry(target).or_insert(TargetImports {
            kinds: kind,
            anchor: local_start,
            anchor_rank: rank,
        });
        entry.kinds = entry.kinds.union(kind);
        if (rank, local_start) < (entry.anchor_rank, entry.anchor) {
            entry.anchor = local_start;
            entry.anchor_rank = rank;
        }
    }

    /// `import` resolved to no indexed file. A relative specifier (`./x`, `../x`, `.x`, `/x`) names a
    /// file in the repository and is unresolved; any other is taken for an external module.
    fn miss(&mut self, import: &ImportEdge) {
        if import.specifier.starts_with(['.', '/']) {
            self.unresolved(&import.specifier);
        } else {
            self.external();
        }
    }

    /// An import named a module outside the repository.
    pub(crate) fn external(&mut self) {
        self.external += 1;
    }

    /// An import named a module of the repository that no indexed file matches.
    pub(crate) fn unresolved(&mut self, specifier: &str) {
        self.unresolved += 1;
        if self.unresolved_specifiers.len() < MAX_UNRESOLVED_SPECIFIERS
            && !self.unresolved_specifiers.iter().any(|known| known == specifier)
        {
            self.unresolved_specifiers.push(specifier.to_string());
        }
    }

    pub(crate) fn finish(self) -> FileEdges {
        let mut edges: Vec<FileEdge> = self
            .targets
            .into_iter()
            .map(|(target, imports)| FileEdge {
                target,
                local_start: imports.anchor,
                kinds: imports.kinds,
            })
            .collect();
        edges.sort_unstable_by(|a, b| a.target.cmp(&b.target));
        FileEdges {
            edges,
            external: self.external,
            unresolved: self.unresolved,
            unresolved_specifiers: self.unresolved_specifiers,
        }
    }
}
