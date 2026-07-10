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
pub(crate) fn resolve_pass(root: &Path, store: &Store) {
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

    let facts = compute_facts(root, store, &files);
    stage_facts(index_db, &facts);

    #[cfg(feature = "code-intel-js")]
    {
        let facts_map = harvest_cross_file_facts(facts);
        crate::intel::xfile::stitch_cross_file_edges(root, store, index_db, &facts_map);
    }
    #[cfg(not(feature = "code-intel-js"))]
    let _ = facts;
}

/// Incremental resolve pass for the watcher: only `changed` files' intra facts are restaged, and
/// only the affected importer set is re-stitched. `changed` is the set of repo-relative paths the
/// watcher re-indexed this event (removed files are handled by the caller's remove-mirror).
pub(crate) fn resolve_pass_incremental(root: &Path, store: &Store, changed: &[String]) {
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
    let changed_facts = compute_facts(root, store, &changed_snapshot);
    stage_facts(index_db, &changed_facts);

    #[cfg(feature = "code-intel-js")]
    xfile_incremental::restitch_affected(root, store, index_db, changed);
    #[cfg(not(feature = "code-intel-js"))]
    let _ = changed_facts;
}

/// Parallel-compute the resolution facts for `files`, respecting the blob cache. Returns the
/// `(rel, facts)` pairs to stage; files whose language can't be interned or whose bytes can't be
/// read are dropped (mirrors the original serial pass's `continue`). Blob WRITES for cache-miss
/// recomputes happen inside this parallel phase — the store is content-addressed, so distinct files
/// write distinct paths and `write_bytes_atomic` makes duplicate-content writes idempotent. Only
/// the small [`FileResolvedRefs`] is retained per file; source bytes are dropped immediately.
fn compute_facts(root: &Path, store: &Store, files: &[FileSnapshot]) -> Vec<(String, FileResolvedRefs)> {
    files
        .par_iter()
        .filter_map(|(rel_str, hash_hex, language)| {
            let refs = match store.read_resolved_by_hex(hash_hex) {
                Ok(Some(cached)) => cached,
                _ => {
                    let lang = lang::intern(language)?;
                    let abs = root.join(rel_str);
                    let bytes = std::fs::read(&abs).ok()?;
                    let computed = crate::intel::resolve::resolve_file(lang, &abs, &bytes);
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

/// Move the JS/TS import/export lists out of the wholesale facts into the map the cross-file stitch
/// consumes. Only files that import or export something are kept (the join ignores the rest).
#[cfg(feature = "code-intel-js")]
fn harvest_cross_file_facts(
    facts: Vec<(String, FileResolvedRefs)>,
) -> ahash::AHashMap<String, crate::intel::xfile::FileFacts> {
    facts
        .into_iter()
        .filter(|(_, refs)| !refs.imports.is_empty() || !refs.exports.is_empty())
        .map(|(rel, refs)| {
            (
                rel,
                crate::intel::xfile::FileFacts {
                    imports: refs.imports,
                    exports: refs.exports,
                },
            )
        })
        .collect()
}

/// Incremental cross-file re-stitch (JS/TS, feature `code-intel-js`).
///
/// Kept in a submodule so the oxc-resolver mirror of `xfile`'s configuration and the affected-set
/// machinery are self-contained and easy to keep in sync.
#[cfg(feature = "code-intel-js")]
mod xfile_incremental {
    use std::path::Path;

    use ahash::{AHashMap, AHashSet};
    use oxc_resolver::{ResolveOptions, Resolver};
    use rayon::prelude::*;

    use super::{FileSnapshot, compute_facts, stage_facts};
    use crate::index::IndexDb;
    use crate::intel::xfile::{FileFacts, stitch_cross_file_edges};
    use crate::store::Store;

    /// JS/TS pack names oxc handles (JSX lives under the `javascript` grammar). Only these files
    /// carry import/export facts, so the reverse-import scan is restricted to them.
    fn is_js_ts(language: &str) -> bool {
        matches!(language, "javascript" | "typescript" | "tsx")
    }

    /// JS/TS module-resolution extensions, TS-first — mirrors `xfile::RESOLVE_EXTENSIONS`. Duplicated
    /// here because `xfile`'s resolver builder is private and this slice may not edit `xfile`; keep
    /// the two in sync if either changes.
    const RESOLVE_EXTENSIONS: &[&str] = &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];

    /// Build the TS-aware Node resolver. Mirrors `xfile::build_resolver` (see the note on
    /// [`RESOLVE_EXTENSIONS`]). `symlinks: false` keeps `strip_prefix(root)` valid.
    fn build_resolver() -> Resolver {
        let ext_alias = |from: &str, to: &[&str]| (from.to_string(), to.iter().map(|s| (*s).to_string()).collect());
        Resolver::new(ResolveOptions {
            extensions: RESOLVE_EXTENSIONS.iter().map(|e| (*e).to_string()).collect(),
            extension_alias: vec![
                ext_alias(".js", &[".ts", ".tsx", ".js", ".jsx"]),
                ext_alias(".mjs", &[".mts", ".mjs"]),
                ext_alias(".cjs", &[".cts", ".cjs"]),
            ],
            condition_names: vec![
                "node".to_string(),
                "import".to_string(),
                "require".to_string(),
                "default".to_string(),
            ],
            symlinks: false,
            ..ResolveOptions::default()
        })
    }

    /// Repo-relative key for an absolute resolved path (forward-slashed). `None` for paths outside
    /// `root` or non-UTF-8 paths. Mirrors `xfile::to_repo_relative`.
    fn to_repo_relative(root: &Path, target_abs: &Path) -> Option<String> {
        let rel = target_abs.strip_prefix(root).ok()?;
        Some(rel.to_str()?.replace('\\', "/"))
    }

    /// Resolve `importer`'s runtime imports to repo-relative target keys, pushing each onto `out`.
    fn resolve_targets(root: &Path, resolver: &Resolver, importer: &str, facts: &FileFacts, out: &mut Vec<String>) {
        let importer_abs = root.join(importer);
        let Some(importer_dir) = importer_abs.parent() else {
            return;
        };
        for import in &facts.imports {
            if import.is_type {
                continue;
            }
            if let Ok(resolution) = resolver.resolve(importer_dir, &import.specifier)
                && let Some(target) = to_repo_relative(root, &resolution.full_path())
            {
                out.push(target);
            }
        }
    }

    /// Re-stitch only the importers whose cross-file resolution could have changed after `changed`
    /// was re-indexed: the changed JS/TS files themselves plus every file that imports one.
    pub(super) fn restitch_affected(root: &Path, store: &Store, index_db: &IndexDb, changed: &[String]) {
        let changed_set: AHashSet<&str> = changed
            .iter()
            .filter(|rel| store.lookup(rel.as_str()).is_some_and(|e| is_js_ts(&e.language)))
            .map(String::as_str)
            .collect();
        if changed_set.is_empty() {
            return;
        }

        let js_files: Vec<(String, String)> = store
            .index
            .files
            .iter()
            .filter(|(_, e)| is_js_ts(&e.language))
            .map(|(rel, e)| (rel.to_str_lossy().into_owned(), e.hash_hex.clone()))
            .collect();
        let js_facts: AHashMap<String, FileFacts> = js_files
            .par_iter()
            .filter_map(|(rel, hash)| {
                let refs = store.read_resolved_by_hex(hash).ok()??;
                (!refs.imports.is_empty() || !refs.exports.is_empty()).then(|| {
                    (
                        rel.clone(),
                        FileFacts {
                            imports: refs.imports,
                            exports: refs.exports,
                        },
                    )
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .collect();

        let resolver = build_resolver();

        let importers_of_changed: Vec<String> = js_facts
            .par_iter()
            .filter_map(|(importer, facts)| {
                if facts.imports.is_empty() {
                    return None;
                }
                let mut targets = Vec::new();
                resolve_targets(root, &resolver, importer, facts, &mut targets);
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
        let ua_facts = compute_facts(root, store, &unchanged_affected);
        stage_facts(index_db, &ua_facts);

        let mut stitch_facts: AHashMap<String, FileFacts> = AHashMap::with_capacity(affected.len());
        for key in &affected {
            if let Some(facts) = js_facts.get(key) {
                stitch_facts.insert(
                    key.clone(),
                    FileFacts {
                        imports: facts.imports.clone(),
                        exports: facts.exports.clone(),
                    },
                );
            }
        }
        let mut provider_targets: Vec<String> = Vec::new();
        for key in &affected {
            if let Some(facts) = js_facts.get(key) {
                resolve_targets(root, &resolver, key, facts, &mut provider_targets);
            }
        }
        for target in provider_targets {
            if stitch_facts.contains_key(&target) {
                continue;
            }
            if let Some(facts) = js_facts.get(&target) {
                stitch_facts.insert(
                    target,
                    FileFacts {
                        imports: Vec::new(),
                        exports: facts.exports.clone(),
                    },
                );
            }
        }

        stitch_cross_file_edges(root, store, index_db, &stitch_facts);
    }
}
