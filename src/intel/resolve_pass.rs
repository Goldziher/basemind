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
//!
//! ## Why the pass is chunked
//!
//! [`FileResolvedRefs::intra`] carries one edge per resolved identifier use, which makes it by far
//! the largest per-file structure this pass touches — and materialising every file's at once was a
//! whole-corpus peak on top of the primary scan's. It is needed for exactly two things, and both
//! are **per-file**: `upsert_resolved_file` staging, and the [`import_bound_edges`] filter that
//! projects out the handful of import-bound edges the cross-file stitch consumes. So the pass runs
//! through [`crate::chunk_drive`]: compute a chunk in parallel, stage it, project it into
//! `xfile::FileFacts` — a type with no field capable of holding `intra` — and drop the chunk's
//! `FileResolvedRefs` before the next chunk is computed. What survives a chunk is the join's
//! import/export facts, never the corpus's resolved edges, and that is enforced by the types
//! rather than by an assertion.

use std::path::Path;

use rayon::prelude::*;

use crate::chunk_drive::{ChunkCut, drive_chunks};
use crate::index::IndexDb;
use crate::intel::model::FileResolvedRefs;
use crate::intel::stage_budget::BudgetedWriter;
use crate::lang;
use crate::path::RelPath;
use crate::scanner_lanes::contain_panic;
use crate::store::Store;

/// Files staged into one Fjall write batch before committing. Mirrors the primary scan's
/// `INDEX_COMMIT_BATCH`: each commit takes Fjall's single write lock, so batching caps the
/// commit count (and thus lock contention) while keeping staged work bounded in memory. Kept
/// local to this module rather than shared with `scanner.rs` — it is an independent tuning knob
/// for the resolve pass. It is a *count* bound only; the byte bound that makes it a memory bound
/// lives in [`BudgetedWriter`].
const INDEX_COMMIT_BATCH: usize = 256;

/// Files whose [`FileResolvedRefs`] may be live at once. Sized well above any plausible thread
/// count so the parallel compute phase still saturates the pool, and far below any repo's file
/// count.
const RESOLVE_CHUNK_FILES: usize = 1024;

/// Source bytes one chunk may cover, whatever the file count says. Resolved edges scale with the
/// number of identifiers in a file, so source size is the pre-compute proxy for how much a chunk
/// will materialise: a handful of machine-generated files can out-weigh a thousand hand-written
/// ones, and a pure file count would wave them through.
const RESOLVE_CHUNK_SOURCE_BYTES: u64 = 16 * 1024 * 1024;

/// The bound on one compute chunk. A named function so a test can pin the constants against the
/// cut they are supposed to produce.
fn resolve_chunk_cut() -> ChunkCut {
    ChunkCut::new(RESOLVE_CHUNK_FILES, RESOLVE_CHUNK_SOURCE_BYTES)
}

/// A minimal per-file snapshot taken from the primary index so the compute phase holds no borrow
/// of `store.index`.
struct FileSnapshot {
    rel: String,
    hash_hex: String,
    language: String,
    /// Source size — the chunk driver's weight, and nothing else.
    size_bytes: u64,
}

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
        .map(|(rel, entry)| FileSnapshot {
            rel: rel.to_str_lossy().into_owned(),
            hash_hex: entry.hash_hex.clone(),
            language: entry.language.clone(),
            size_bytes: entry.size_bytes,
        })
        .collect();

    #[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
    {
        let mut cross_file: ahash::AHashMap<String, crate::intel::xfile::FileFacts> = ahash::AHashMap::new();
        resolve_in_chunks(root, store, index_db, &files, precise, |facts| {
            harvest_cross_file_facts(facts, &mut cross_file);
        });
        crate::intel::xfile::stitch_cross_file_edges(root, store, index_db, &cross_file);
    }
    #[cfg(not(any(feature = "code-intel-js", feature = "code-intel-stack")))]
    resolve_in_chunks(root, store, index_db, &files, precise, drop);
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
            Some(FileSnapshot {
                rel: rel.clone(),
                hash_hex: entry.hash_hex.clone(),
                language: entry.language.clone(),
                size_bytes: entry.size_bytes,
            })
        })
        .collect();
    resolve_in_chunks(root, store, index_db, &changed_snapshot, precise, drop);

    #[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
    xfile_incremental::restitch_affected(root, store, index_db, changed, precise);
}

/// Compute → stage → project, one bounded chunk at a time.
///
/// This is the pass's whole memory story: [`drive_chunks`] owns each chunk's `FileResolvedRefs`,
/// hands them to the serial staging step, offers them to `harvest` for projection, and drops them
/// before the next chunk is computed. `harvest` receives them by value precisely so it cannot keep
/// them — anything it wants to retain has to be copied into a structure of its own choosing, and
/// the one caller that retains anything chooses a type with no `intra` field.
fn resolve_in_chunks(
    root: &Path,
    store: &Store,
    index_db: &IndexDb,
    files: &[FileSnapshot],
    precise: bool,
    mut harvest: impl FnMut(Vec<(String, FileResolvedRefs)>),
) {
    drive_chunks(
        files,
        resolve_chunk_cut(),
        |snapshot| snapshot.size_bytes,
        |chunk| compute_facts(root, store, chunk, precise),
        |facts| {
            stage_facts(index_db, &facts);
            harvest(facts);
        },
    );
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
        .filter_map(|snapshot| {
            let rel_str = snapshot.rel.as_str();
            let refs = match store.read_resolved_by_hex(&snapshot.hash_hex) {
                Ok(Some(cached)) => cached,
                _ => {
                    let lang = lang::intern(&snapshot.language)?;
                    let abs = root.join(rel_str);
                    let bytes = std::fs::read(&abs).ok()?;
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
                        let _ = store.write_resolved_hex(&snapshot.hash_hex, &computed);
                    }
                    computed
                }
            };
            Some((rel_str.to_string(), refs))
        })
        .collect()
}

/// Drain one chunk's computed facts into the `IndexWriter` SERIALLY, committing on
/// `INDEX_COMMIT_BATCH` files or the byte bounds [`BudgetedWriter`] enforces. Fjall staging is the
/// shared bottleneck, so it stays single-threaded (the parallel win is the compute phase above). A
/// file with empty facts still gets `remove_resolved_file` so a prior scan's edges are cleared.
fn stage_facts(index_db: &IndexDb, facts: &[(String, FileResolvedRefs)]) {
    let mut batch = BudgetedWriter::new(index_db, INDEX_COMMIT_BATCH, "resolve pass");
    for (rel_str, refs) in facts {
        let rel = RelPath::from(rel_str.as_str());
        let staged_res = if refs.is_empty() {
            batch.writer().remove_resolved_file(&rel)
        } else {
            batch.writer().upsert_resolved_file(&rel, refs)
        };
        if let Err(error) = staged_res {
            tracing::warn!(path = %rel, %error, "resolve pass: failed to stage resolved edges — skipping file");
        }
        batch.item_staged();
    }
    batch.finish();
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

/// Project one chunk's facts into the map the cross-file stitch consumes, then let the chunk's
/// [`FileResolvedRefs`] die. Only files that import or export something are kept (the join ignores
/// the rest). The stitch itself picks a per-language resolver, so this harvest is
/// language-agnostic.
///
/// This is the projection the whole chunking rests on: `FileFacts` has no `intra` field, so what
/// accumulates across chunks *cannot* be the O(corpus) edge set — only the import/export lists and
/// the import-bound slice of `intra`, which is the part the join actually needs.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
fn harvest_cross_file_facts(
    facts: Vec<(String, FileResolvedRefs)>,
    out: &mut ahash::AHashMap<String, crate::intel::xfile::FileFacts>,
) {
    for (rel, refs) in facts {
        if refs.imports.is_empty() && refs.exports.is_empty() {
            continue;
        }
        let import_uses = import_bound_edges(&refs);
        out.insert(
            rel,
            crate::intel::xfile::FileFacts {
                imports: refs.imports,
                exports: refs.exports,
                import_uses,
            },
        );
    }
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

    use super::{FileSnapshot, resolve_in_chunks};
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
                Some(FileSnapshot {
                    rel: k.clone(),
                    hash_hex: entry.hash_hex.clone(),
                    language: entry.language.clone(),
                    size_bytes: entry.size_bytes,
                })
            })
            .collect();
        resolve_in_chunks(root, store, index_db, &unchanged_affected, precise, drop);

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

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(rel: &str, size_bytes: u64) -> FileSnapshot {
        FileSnapshot {
            rel: rel.to_string(),
            hash_hex: "0".repeat(64),
            language: "typescript".to_string(),
            size_bytes,
        }
    }

    /// The pass's own constants — not just the driver — must cut on bytes. A handful of
    /// machine-generated files carries more resolved edges than a thousand hand-written ones, so a
    /// cut that only counted files would materialise all of them at once.
    #[test]
    fn the_pass_cut_is_byte_aware_and_not_only_count_aware() {
        let heavy: Vec<FileSnapshot> = (0..8)
            .map(|i| snapshot(&format!("src/generated_{i}.ts"), 8 * 1024 * 1024))
            .collect();

        let mut chunk_sizes = Vec::new();
        drive_chunks(
            &heavy,
            resolve_chunk_cut(),
            |s| s.size_bytes,
            |chunk| {
                chunk_sizes.push(chunk.len());
                Vec::<()>::new()
            },
            |_| {},
        );

        assert!(
            chunk_sizes.len() > 1,
            "eight 8 MiB files must not ride in one chunk under a {RESOLVE_CHUNK_SOURCE_BYTES}-byte budget"
        );
        assert!(
            heavy.len() < RESOLVE_CHUNK_FILES,
            "the file-count bound must not be what cut these chunks"
        );
        assert_eq!(chunk_sizes.iter().sum::<usize>(), heavy.len());
    }

    /// A repo of ordinary files still rides the cheap count bound — the byte budget must not turn
    /// every scan into a chunk-per-file commit storm.
    #[test]
    fn ordinary_files_still_ride_the_count_bound() {
        let light: Vec<FileSnapshot> = (0..RESOLVE_CHUNK_FILES)
            .map(|i| snapshot(&format!("src/hand_written_{i}.ts"), 4 * 1024))
            .collect();

        let mut chunk_sizes = Vec::new();
        drive_chunks(
            &light,
            resolve_chunk_cut(),
            |s| s.size_bytes,
            |chunk| {
                chunk_sizes.push(chunk.len());
                Vec::<()>::new()
            },
            |_| {},
        );

        assert_eq!(chunk_sizes, vec![RESOLVE_CHUNK_FILES]);
    }

    /// The projection that lets a chunk's `FileResolvedRefs` die: what survives into `FileFacts`
    /// must be the import-bound *slice* of `intra`, never the whole vector. `FileFacts` has no
    /// field that could hold the rest, and this pins the one field that shares its element type.
    #[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
    #[test]
    fn the_harvest_retains_only_import_bound_edges() {
        use crate::intel::model::{ImportEdge, ResolvedEdge};

        let mut refs = FileResolvedRefs::new("typescript");
        refs.imports = vec![ImportEdge {
            local: "useThing".to_string(),
            specifier: "./thing".to_string(),
            imported: Some("useThing".to_string()),
            is_type: false,
            local_start: 7,
        }];
        refs.intra = (0..256)
            .map(|i| ResolvedEdge {
                use_start: 1000 + i * 16,
                use_end: 1008 + i * 16,
                // Only every 128th edge binds to the import; the rest are ordinary in-file edges.
                def_start: if i.is_multiple_of(128) { 7 } else { 500 },
                def_end: 20,
            })
            .collect();

        let mut harvested = ahash::AHashMap::new();
        harvest_cross_file_facts(vec![("src/app.ts".to_string(), refs)], &mut harvested);

        let facts = harvested.get("src/app.ts").expect("importer must be harvested");
        assert_eq!(
            facts.import_uses.len(),
            2,
            "only the edges bound to an import may survive the chunk"
        );
        assert!(facts.import_uses.iter().all(|e| e.def_start == 7));
    }

    /// The pass's half of the release invariant, over MANY chunks rather than one: what the harvest
    /// accumulates must scale with the corpus's *imports*, never with its resolved edges.
    ///
    /// The guarantee is primarily a TYPE guarantee — `FileFacts` has no field that could hold
    /// `intra`, so an accumulator of them cannot be O(corpus edges) however many chunks it sees.
    /// The exhaustive destructuring at the end is what asserts that: giving `FileFacts` a field for
    /// the full edge set stops this test compiling. The counts above it pin the runtime half, which
    /// a filter regression (harvesting every edge instead of the import-bound slice) would break
    /// without changing any type.
    #[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
    #[test]
    fn what_survives_the_chunks_scales_with_imports_not_with_resolved_edges() {
        use crate::intel::model::{ImportEdge, ResolvedEdge};

        const CHUNKS: usize = 8;
        const FILES_PER_CHUNK: usize = 16;
        const EDGES_PER_FILE: u32 = 4096;

        let file_facts = |rel: String| {
            let mut refs = FileResolvedRefs::new("typescript");
            refs.imports = vec![ImportEdge {
                local: "useThing".to_string(),
                specifier: "./thing".to_string(),
                imported: Some("useThing".to_string()),
                is_type: false,
                local_start: 7,
            }];
            refs.intra = (0..EDGES_PER_FILE)
                .map(|i| ResolvedEdge {
                    use_start: 1000 + i * 16,
                    use_end: 1008 + i * 16,
                    def_start: if i == 0 { 7 } else { 500 },
                    def_end: 20,
                })
                .collect();
            (rel, refs)
        };

        let mut retained: ahash::AHashMap<String, crate::intel::xfile::FileFacts> = ahash::AHashMap::new();
        for chunk in 0..CHUNKS {
            let computed: Vec<(String, FileResolvedRefs)> = (0..FILES_PER_CHUNK)
                .map(|f| file_facts(format!("src/c{chunk}/f{f}.ts")))
                .collect();
            harvest_cross_file_facts(computed, &mut retained);
        }

        let files = CHUNKS * FILES_PER_CHUNK;
        let computed_edges = files * EDGES_PER_FILE as usize;
        let survived: usize = retained.values().map(|f| f.import_uses.len()).sum();
        assert_eq!(retained.len(), files, "every importer must be harvested");
        assert_eq!(
            survived, files,
            "exactly the one import-bound edge per file may outlive its chunk"
        );
        assert!(
            survived * 1000 < computed_edges,
            "{survived} edges survived {computed_edges} computed — the projection stopped being one"
        );

        let (_rel, facts) = retained.into_iter().next().expect("at least one importer");
        let crate::intel::xfile::FileFacts {
            imports,
            exports,
            import_uses,
        } = facts;
        assert_eq!(imports.len(), 1);
        assert!(exports.is_empty());
        assert_eq!(import_uses.len(), 1);
    }
}
