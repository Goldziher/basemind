//! Post-scan resolution pass: caching + secondary-index staging for per-file resolution facts,
//! plus the cross-file JS/TS join. Lifted out of `src/scanner.rs` (which owns the primary scan) so
//! that module stays under the line cap and the pass sits beside the rest of the `intel` tier.
//!
//! Two entry points share the same per-file compute/stage helpers:
//!
//! - [`resolve_pass`] — **wholesale**, run after a full `scan`. Every indexed file's intra facts
//!   are (re)staged and every importer is (re)stitched. A full scan already touches every file, so
//!   there is nothing to scope down.
//! - [`resolve_pass_incremental`] — **scoped**, run after `scan_paths` (the watcher path). Only the
//!   changed files' intra facts are restaged, and only the importers whose cross-file resolution
//!   could actually change — the changed files themselves plus every file that *imports* a changed
//!   file — are re-stitched. Every other file's `refs_by_def` / `refs_by_path` entries are left
//!   untouched. This turns a 1-file watcher event from O(entire repo) Fjall churn into O(changed +
//!   their importers).
//!
//! ## Reverse-import invariant (the correctness crux of the incremental path)
//!
//! A file's cross-file edges depend on OTHER files: if a dependency's export moves, the unchanged
//! importer must be re-stitched to the new export site. The wholesale pass gets this for free by
//! re-stitching everything. The incremental pass reconstructs the affected importer set explicitly:
//! it loads every indexed JS/TS file's persisted import list (from the `.rref` blobs) and resolves
//! each import specifier with the same [`oxc_resolver`] configuration `xfile` uses, so an importer
//! whose resolved target is a changed file is pulled into the affected set. Those affected importers
//! (changed OR unchanged) get their per-file slate cleared via `upsert_resolved_file` *before* the
//! stitch, so the re-stitch replaces — never accumulates — their cross-file edges.

use std::path::Path;

use rayon::prelude::*;

use crate::index::IndexDb;
use crate::intel::model::FileResolvedRefs;
use crate::lang;
use crate::path::RelPath;
use crate::scanner_lanes::contain_panic;
use crate::store::Store;

/// Files staged into one Fjall write batch before committing. Mirrors the primary scan's
/// `INDEX_COMMIT_BATCH`: each commit takes Fjall's single write lock, so batching caps the
/// commit count (and thus lock contention) while keeping staged work bounded in memory. Kept
/// local to this module rather than shared with `scanner.rs` — it is an independent tuning knob
/// for the resolve pass.
const INDEX_COMMIT_BATCH: usize = 256;

/// A minimal per-file snapshot `(rel, content-hash-hex, language)` taken from the primary index so
/// the compute phase holds no borrow of `store.index`.
type FileSnapshot = (String, String, String);

/// Wholesale resolve pass: (re)stage every indexed file's intra facts and (re)stitch every
/// importer. Best-effort — any failure is logged and the scan still succeeds. No-op in a read-only
/// (no writable index) session.
pub(crate) fn resolve_pass(root: &Path, store: &Store, precise: bool) {
    let Some(index_db) = store.index_db.as_ref() else {
        return;
    };
    let files: Vec<FileSnapshot> = store
        .index
        .files
        .iter()
        .map(|(rel, entry)| {
            (
                rel.to_str_lossy().into_owned(),
                entry.hash_hex.clone(),
                entry.language.clone(),
            )
        })
        .collect();

    let facts = compute_facts(root, store, &files, precise);
    stage_facts(index_db, &facts);

    #[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
    {
        let facts_map = harvest_cross_file_facts(facts);
        crate::intel::xfile::stitch_cross_file_edges(root, store, index_db, &facts_map);
    }
    #[cfg(not(any(feature = "code-intel-js", feature = "code-intel-stack")))]
    let _ = facts;
}

/// Incremental resolve pass for the watcher: only `changed` files' intra facts are restaged, and
/// only the affected importer set is re-stitched. `changed` is the set of repo-relative paths the
/// watcher re-indexed this event (removed files are handled by the caller's remove-mirror).
pub(crate) fn resolve_pass_incremental(root: &Path, store: &Store, changed: &[String], precise: bool) {
    let Some(index_db) = store.index_db.as_ref() else {
        return;
    };

    let changed_snapshot: Vec<FileSnapshot> = changed
        .iter()
        .filter_map(|rel| {
            let entry = store.lookup(rel.as_str())?;
            Some((rel.clone(), entry.hash_hex.clone(), entry.language.clone()))
        })
        .collect();
    let changed_facts = compute_facts(root, store, &changed_snapshot, precise);
    stage_facts(index_db, &changed_facts);

    #[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
    xfile_incremental::restitch_affected(root, store, index_db, changed, precise);
    #[cfg(not(any(feature = "code-intel-js", feature = "code-intel-stack")))]
    let _ = changed_facts;
}

/// Parallel-compute the resolution facts for `files`, respecting the blob cache. Returns the
/// `(rel, facts)` pairs to stage; files whose language can't be interned or whose bytes can't be
/// read are dropped (mirrors the original serial pass's `continue`). Blob WRITES for cache-miss
/// recomputes happen inside this parallel phase — the store is content-addressed, so distinct files
/// write distinct paths and `write_bytes_atomic` makes duplicate-content writes idempotent. Only
/// the small [`FileResolvedRefs`] is retained per file; source bytes are dropped immediately.
fn compute_facts(root: &Path, store: &Store, files: &[FileSnapshot], precise: bool) -> Vec<(String, FileResolvedRefs)> {
    files
        .par_iter()
        .filter_map(|(rel_str, hash_hex, language)| {
            let refs = match store.read_resolved_by_hex(hash_hex) {
                Ok(Some(cached)) => cached,
                _ => {
                    let lang = lang::intern(language)?;
                    let abs = root.join(rel_str);
                    let bytes = std::fs::read(&abs).ok()?;
                    // Per-file panic containment: the precise engines are third-party (the
                    // `stack-graphs` partial-path stitcher has panicked with an out-of-bounds index
                    // and a failed cyclic test on real inputs). One pathological file must cost only
                    // its own resolved edges, not the other tens of thousands of files in the pass.
                    let computed = match contain_panic(|| crate::intel::resolve::resolve_file(lang, &abs, &bytes, precise))
                    {
                        Ok(computed) => computed,
                        Err(reason) => {
                            tracing::warn!(
                                path = rel_str,
                                lang,
                                reason,
                                "resolve pass: resolver panicked on this file — skipping it; its navigation stays name-only"
                            );
                            return None;
                        }
                    };
                    if !computed.is_empty() {
                        let _ = store.write_resolved_hex(hash_hex, &computed);
                    }
                    computed
                }
            };
            Some((rel_str.clone(), refs))
        })
        .collect()
}

/// Drain the computed facts into the single `IndexWriter` SERIALLY, committing in
/// `INDEX_COMMIT_BATCH` chunks. Fjall staging is the shared bottleneck, so it stays single-threaded
/// (the parallel win is the compute phase above). A file with empty facts still gets
/// `remove_resolved_file` so a prior scan's edges are cleared.
fn stage_facts(index_db: &IndexDb, facts: &[(String, FileResolvedRefs)]) {
    let mut writer = index_db.writer();
    let mut staged = 0usize;
    for (rel_str, refs) in facts {
        let rel = RelPath::from(rel_str.as_str());
        let staged_res = if refs.is_empty() {
            writer.remove_resolved_file(&rel)
        } else {
            writer.upsert_resolved_file(&rel, refs)
        };
        if let Err(error) = staged_res {
            tracing::warn!(path = %rel, %error, "resolve pass: failed to stage resolved edges — skipping file");
        }
        staged += 1;
        if staged >= INDEX_COMMIT_BATCH {
            if let Err(error) = writer.commit() {
                tracing::warn!(%error, "resolve pass: index commit failed — resolved navigation may be stale");
            }
            writer = index_db.writer();
            staged = 0;
        }
    }
    if let Err(error) = writer.commit() {
        tracing::warn!(%error, "resolve pass: index commit failed — resolved navigation may be stale");
    }
}

/// This file's intra edges whose definition is one of its own import bindings — the in-file use
/// sites of an imported name. Pre-filtered here (not in the stitch) so `FileFacts` carries only the
/// import-relevant slice of what can be a large `intra` vector.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
fn import_bound_edges(refs: &FileResolvedRefs) -> Vec<crate::intel::model::ResolvedEdge> {
    if refs.imports.is_empty() || refs.intra.is_empty() {
        return Vec::new();
    }
    let import_starts: ahash::AHashSet<u32> = refs.imports.iter().map(|i| i.local_start).collect();
    refs.intra
        .iter()
        .filter(|e| import_starts.contains(&e.def_start))
        .cloned()
        .collect()
}

/// Move the import/export lists out of the wholesale facts into the map the cross-file stitch
/// consumes. Only files that import or export something are kept (the join ignores the rest). The
/// stitch itself picks a per-language resolver, so this harvest is language-agnostic.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
fn harvest_cross_file_facts(
    facts: Vec<(String, FileResolvedRefs)>,
) -> ahash::AHashMap<String, crate::intel::xfile::FileFacts> {
    facts
        .into_iter()
        .filter(|(_, refs)| !refs.imports.is_empty() || !refs.exports.is_empty())
        .map(|(rel, refs)| {
            let import_uses = import_bound_edges(&refs);
            (
                rel,
                crate::intel::xfile::FileFacts {
                    imports: refs.imports,
                    exports: refs.exports,
                    import_uses,
                },
            )
        })
        .collect()
}

/// Incremental cross-file re-stitch (feature `code-intel-js` or `code-intel-stack`).
///
/// Kept in a submodule so the affected-set machinery is self-contained. Resolution goes through the
/// shared per-language [`crate::intel::resolver::SpecifierResolver`], so it covers every
/// resolver-capable language (JS/TS, Python, Java) rather than only JS/TS.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
mod xfile_incremental {
    use std::path::Path;

    use ahash::{AHashMap, AHashSet};
    use rayon::prelude::*;

    use super::{FileSnapshot, compute_facts, stage_facts};
    use crate::index::IndexDb;
    use crate::intel::resolver::SpecifierResolver;
    use crate::intel::xfile::{FileFacts, stitch_cross_file_edges};
    use crate::store::Store;

    /// True if `language` has a compiled-in specifier resolver — i.e. its files can carry stitchable
    /// import/export facts. Files in other languages never enter the affected set. This is asked once
    /// per indexed file, so it must not construct a resolver (the JS one wraps an `oxc_resolver`).
    fn has_resolver(language: &str) -> bool {
        SpecifierResolver::supports(language)
    }

    /// A file's import/export facts plus the language that selects its resolver.
    struct FileEntry {
        language: String,
        facts: FileFacts,
    }

    /// Resolvers keyed by language, built once and shared across every importer. Building the JS
    /// variant constructs an `oxc_resolver` (and its tsconfig cache), so it must never happen
    /// per-file — this map is the reuse point for both the parallel and the serial passes below.
    type ResolverCache = AHashMap<String, Option<SpecifierResolver>>;

    /// Build one resolver per distinct language present in `entries`.
    fn build_resolvers(entries: &AHashMap<String, FileEntry>) -> ResolverCache {
        let mut cache = ResolverCache::new();
        for entry in entries.values() {
            if !cache.contains_key(&entry.language) {
                cache.insert(entry.language.clone(), SpecifierResolver::for_language(&entry.language));
            }
        }
        cache
    }

    /// Resolve `importer`'s runtime imports (using the resolver for its language) to repo-relative
    /// target keys, pushing each onto `out`. A language with no resolver contributes nothing.
    fn resolve_targets(
        root: &Path,
        importer: &str,
        entry: &FileEntry,
        resolvers: &ResolverCache,
        out: &mut Vec<String>,
    ) {
        let Some(Some(resolver)) = resolvers.get(&entry.language) else {
            return;
        };
        for import in &entry.facts.imports {
            if import.is_type {
                continue;
            }
            if let Some(target) = resolver.resolve(root, importer, import)
                && let Some(key) = target.as_str()
            {
                out.push(key.to_string());
            }
        }
    }

    /// Re-stitch only the importers whose cross-file resolution could have changed after `changed`
    /// was re-indexed: the changed resolver-capable files themselves plus every file that imports
    /// one.
    pub(super) fn restitch_affected(root: &Path, store: &Store, index_db: &IndexDb, changed: &[String], precise: bool) {
        let changed_set: AHashSet<&str> = changed
            .iter()
            .filter(|rel| store.lookup(rel.as_str()).is_some_and(|e| has_resolver(&e.language)))
            .map(String::as_str)
            .collect();
        if changed_set.is_empty() {
            return;
        }

        let candidate_files: Vec<(String, String, String)> = store
            .index
            .files
            .iter()
            .filter(|(_, e)| has_resolver(&e.language))
            .map(|(rel, e)| (rel.to_str_lossy().into_owned(), e.hash_hex.clone(), e.language.clone()))
            .collect();
        let entries: AHashMap<String, FileEntry> = candidate_files
            .par_iter()
            .filter_map(|(rel, hash, language)| {
                let refs = store.read_resolved_by_hex(hash).ok()??;
                if refs.imports.is_empty() && refs.exports.is_empty() {
                    return None;
                }
                let import_uses = super::import_bound_edges(&refs);
                Some((
                    rel.clone(),
                    FileEntry {
                        language: language.clone(),
                        facts: FileFacts {
                            imports: refs.imports,
                            exports: refs.exports,
                            import_uses,
                        },
                    },
                ))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .collect();

        let resolvers = build_resolvers(&entries);
        let importers_of_changed: Vec<String> = entries
            .par_iter()
            .filter_map(|(importer, entry)| {
                if entry.facts.imports.is_empty() {
                    return None;
                }
                let mut targets = Vec::new();
                resolve_targets(root, importer, entry, &resolvers, &mut targets);
                targets
                    .iter()
                    .any(|t| changed_set.contains(t.as_str()))
                    .then(|| importer.clone())
            })
            .collect();

        let mut affected: AHashSet<String> = changed_set.iter().map(|s| (*s).to_string()).collect();
        affected.extend(importers_of_changed);

        let unchanged_affected: Vec<FileSnapshot> = affected
            .iter()
            .filter(|k| !changed_set.contains(k.as_str()))
            .filter_map(|k| {
                let entry = store.lookup(k.as_str())?;
                Some((k.clone(), entry.hash_hex.clone(), entry.language.clone()))
            })
            .collect();
        let ua_facts = compute_facts(root, store, &unchanged_affected, precise);
        stage_facts(index_db, &ua_facts);

        let mut stitch_facts: AHashMap<String, FileFacts> = AHashMap::with_capacity(affected.len());
        for key in &affected {
            if let Some(entry) = entries.get(key) {
                stitch_facts.insert(
                    key.clone(),
                    FileFacts {
                        imports: entry.facts.imports.clone(),
                        exports: entry.facts.exports.clone(),
                        import_uses: entry.facts.import_uses.clone(),
                    },
                );
            }
        }
        let mut provider_targets: Vec<String> = Vec::new();
        for key in &affected {
            if let Some(entry) = entries.get(key) {
                resolve_targets(root, key, entry, &resolvers, &mut provider_targets);
            }
        }
        for target in provider_targets {
            if stitch_facts.contains_key(&target) {
                continue;
            }
            if let Some(entry) = entries.get(&target) {
                stitch_facts.insert(
                    target,
                    FileFacts {
                        imports: Vec::new(),
                        exports: entry.facts.exports.clone(),
                        import_uses: Vec::new(),
                    },
                );
            }
        }

        stitch_cross_file_edges(root, store, index_db, &stitch_facts);
    }
}
