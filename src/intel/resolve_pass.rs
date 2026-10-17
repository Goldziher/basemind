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
//! re-stitching everything. The incremental pass must find the affected importers without reading
//! the corpus, so the stitch persists, per importer, every path the resolver consulted
//! (`importers_by_probe`: hit targets, re-export hop files, failed candidates, `tsconfig.json` /
//! `package.json`). The affected set is then the changed, added and deleted paths themselves plus a
//! prefix scan of that reverse map per path — exact (a re-export chain or a shadowing file is a
//! probe like any other) and O(affected). Affected importers (changed OR unchanged) get their
//! per-file slate cleared via `upsert_resolved_file` *before* the stitch, so the re-stitch
//! replaces — never accumulates — their cross-file edges and probe rows.
//!
//! An index whose probe rows may be incomplete (created before the keyspaces existed, or a
//! wholesale pass that never finished) lacks the `import_probe_ver` stamp; the incremental pass then
//! runs the wholesale one once.
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

use crate::backpressure::FootprintGate;
use crate::chunk_drive::{ChunkCut, drive_chunks_governed};
use crate::config::MaxFootprint;
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
/// importer. Best-effort — a resolver failure is logged and skipped. No-op in a read-only (no
/// writable index) session.
///
/// Returns whether every index commit succeeded: only then may the caller record the pass as
/// complete. A fatal commit failure (disk full, poisoned handle) also latches on `index_db`, which
/// the scan polls to abort.
pub(crate) fn resolve_pass(root: &Path, store: &Store, precise: bool, footprint: MaxFootprint) -> bool {
    let Some(index_db) = store.index_db.as_ref() else {
        return true;
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
    let committed = {
        // The probe rows are rewritten importer by importer; until the stitch commits they describe a
        // mixture of old and new state, so the stamp that vouches for them is withdrawn first.
        if let Err(error) = index_db.set_import_probes_complete(false) {
            tracing::warn!(%error, "resolve pass: could not withdraw the import-probe stamp");
        }
        let mut cross_file: ahash::AHashMap<String, crate::intel::xfile::FileFacts> = ahash::AHashMap::new();
        let staged = resolve_in_chunks(root, store, index_db, &files, precise, footprint, |facts| {
            harvest_cross_file_facts(facts, &mut cross_file);
        });
        let stitched = crate::intel::xfile::stitch_cross_file_edges(root, store, index_db, &cross_file);
        if staged
            && stitched
            && let Err(error) = index_db.set_import_probes_complete(true)
        {
            tracing::warn!(%error, "resolve pass: could not stamp the import-probe index complete");
        }
        staged && stitched
    };
    #[cfg(not(any(feature = "code-intel-js", feature = "code-intel-stack")))]
    let committed = resolve_in_chunks(root, store, index_db, &files, precise, footprint, drop);
    committed
}

/// What a watcher event touched. `changed` are the re-indexed files, `removed` the files that left
/// the index, `other` paths that are not indexed (a `tsconfig.json`, a `package.json`) but that a
/// resolver may have read or probed for. All are repo-relative.
#[cfg_attr(not(any(feature = "code-intel-js", feature = "code-intel-stack")), allow(dead_code))]
pub(crate) struct PathDelta<'a> {
    pub changed: &'a [String],
    pub removed: &'a [String],
    pub other: &'a [String],
}

/// The paths of `candidates` that some importer's resolution consulted — the only non-indexed
/// paths whose change can matter. A cheap point lookup per path.
pub(crate) fn probed_only(index_db: &IndexDb, candidates: Vec<String>) -> Vec<String> {
    candidates.into_iter().filter(|rel| index_db.is_probed(rel)).collect()
}

/// Incremental resolve pass for the watcher: only `delta.changed` files' intra facts are restaged,
/// and only the affected importer set is re-stitched (see the module docs).
pub(crate) fn resolve_pass_incremental(
    root: &Path,
    store: &Store,
    delta: &PathDelta<'_>,
    precise: bool,
    footprint: MaxFootprint,
) {
    let Some(index_db) = store.index_db.as_ref() else {
        return;
    };
    #[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
    if !index_db.import_probes_complete() {
        tracing::info!("resolve pass: import-probe index not built yet — running the wholesale pass once");
        resolve_pass(root, store, precise, footprint);
        return;
    }

    let changed_snapshot = snapshot_of(store, delta.changed.iter());
    #[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
    {
        let mut importers: ahash::AHashMap<String, crate::intel::xfile::FileFacts> = ahash::AHashMap::new();
        resolve_in_chunks(root, store, index_db, &changed_snapshot, precise, footprint, |facts| {
            harvest_cross_file_facts(facts, &mut importers);
        });
        let affected = xfile_incremental::affected_importers(index_db, store, delta);
        let affected_snapshot = snapshot_of(store, affected.iter());
        resolve_in_chunks(root, store, index_db, &affected_snapshot, precise, footprint, |facts| {
            harvest_cross_file_facts(facts, &mut importers);
        });
        crate::intel::xfile::stitch_cross_file_edges(root, store, index_db, &importers);
    }
    #[cfg(not(any(feature = "code-intel-js", feature = "code-intel-stack")))]
    resolve_in_chunks(root, store, index_db, &changed_snapshot, precise, footprint, drop);
}

/// Snapshots of the indexed files among `rels`.
fn snapshot_of<'a>(store: &Store, rels: impl Iterator<Item = &'a String>) -> Vec<FileSnapshot> {
    rels.filter_map(|rel| {
        let entry = store.lookup(rel.as_str())?;
        Some(FileSnapshot {
            rel: rel.clone(),
            hash_hex: entry.hash_hex.clone(),
            language: entry.language.clone(),
            size_bytes: entry.size_bytes,
        })
    })
    .collect()
}

/// Compute → stage → project, one bounded chunk at a time.
///
/// This is the pass's whole memory story: [`drive_chunks_governed`] owns each chunk's `FileResolvedRefs`,
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
    footprint: MaxFootprint,
    mut harvest: impl FnMut(Vec<(String, FileResolvedRefs)>),
) -> bool {
    // The admission point the primary scan's drive loop has and this pass lacked: before each chunk
    // is computed, park (bounded) while the process is over its footprint ceiling, so the previous
    // chunk's arenas and the allocator's freed pages can settle. Advisory by design -- the gate
    // admits after its max wait to guarantee progress -- so the hard bound on one file's memory
    // remains the per-file budgets in `intel::stackgraph`.
    let gate = FootprintGate::new(footprint);
    let mut all_committed = true;
    drive_chunks_governed(
        &mut (),
        files,
        || {
            // A fatal commit failure (disk full, poisoned handle) dooms every later chunk's commit;
            // stop computing facts that can only be thrown away.
            if index_db.fatal_fault().is_some() {
                return None;
            }
            gate.admit();
            Some(resolve_chunk_cut())
        },
        |(), snapshot| snapshot.size_bytes,
        |(), chunk| compute_facts(root, store, chunk, precise),
        |(), facts| {
            all_committed &= stage_facts(index_db, &facts);
            harvest(facts);
        },
    );
    all_committed && index_db.fatal_fault().is_none()
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
///
/// Returns false when any commit failed, so the pass can refuse to be recorded as complete.
fn stage_facts(index_db: &IndexDb, facts: &[(String, FileResolvedRefs)]) -> bool {
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
    batch.finish()
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

/// Affected-set computation for the incremental re-stitch (feature `code-intel-js` or
/// `code-intel-stack`). Resolution itself goes through the shared per-language
/// [`crate::intel::resolver::SpecifierResolver`] inside the stitch, so it covers every
/// resolver-capable language (JS/TS, Python, Java).
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
mod xfile_incremental {
    use ahash::AHashSet;

    use super::PathDelta;
    use crate::index::IndexDb;
    use crate::intel::resolver::SpecifierResolver;
    use crate::store::Store;

    /// Indexed, resolver-capable importers that are NOT in `delta.changed` (those are re-stitched
    /// anyway) but consulted a touched path: a changed, added or deleted file, or a non-indexed
    /// config file. One prefix scan of `importers_by_probe` per touched path.
    pub(super) fn affected_importers(index_db: &IndexDb, store: &Store, delta: &PathDelta<'_>) -> Vec<String> {
        let changed: AHashSet<&str> = delta.changed.iter().map(String::as_str).collect();
        let mut affected: AHashSet<String> = AHashSet::new();
        for touched in delta.changed.iter().chain(delta.removed).chain(delta.other) {
            for importer in index_db.importers_probing(touched) {
                let Some(key) = importer.as_str() else {
                    continue;
                };
                if !changed.contains(key)
                    && store
                        .lookup(key)
                        .is_some_and(|entry| SpecifierResolver::supports(&entry.language))
                {
                    affected.insert(key.to_string());
                }
            }
        }
        affected.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk_drive::drive_chunks;

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
