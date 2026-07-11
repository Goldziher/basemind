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
//! Best-effort: any resolver miss or unindexed target is logged at debug and skipped; a commit
//! failure warns but never aborts the scan. Idempotency across re-scans is documented on
//! [`crate::index::writer::IndexWriter::upsert_cross_file_edge`].

use std::path::Path;

use ahash::AHashMap;

use crate::index::IndexDb;
use crate::intel::model::{ExportEdge, ImportEdge, ResolvedEdge};
use crate::intel::resolver::SpecifierResolver;
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
/// concurrent MCP reader isn't blocked for the whole stitch.
const COMMIT_BATCH: usize = 256;

/// Stitch cross-file resolved edges for every importer in `facts` into the index.
///
/// Runs after the resolve pass's per-file upserts have committed, so each importer's previous-scan
/// edges are already purged (see the idempotency note on `upsert_cross_file_edge`). Best-effort:
/// never returns an error — resolver misses and commit failures are logged and swallowed so the
/// scan still succeeds with the name-based fallback intact.
///
/// Each importer's language (from the primary index) selects a [`SpecifierResolver`]; importers in
/// a language with no compiled-in resolver are skipped. Resolvers are built once per language and
/// cached for the duration of the stitch.
pub fn stitch_cross_file_edges(root: &Path, store: &Store, index_db: &IndexDb, facts: &AHashMap<String, FileFacts>) {
    if facts.is_empty() {
        return;
    }
    let export_maps: AHashMap<&str, AHashMap<&str, u32>> = facts
        .iter()
        .filter(|(_, f)| !f.exports.is_empty())
        .map(|(key, f)| {
            let by_name: AHashMap<&str, u32> = f.exports.iter().map(|e| (e.name.as_str(), e.name_start)).collect();
            (key.as_str(), by_name)
        })
        .collect();

    // Resolvers are cheap to reuse but the oxc variant is not cheap to build; cache one per
    // language for the whole stitch. `None` records a language with no compiled-in resolver.
    let mut resolvers: AHashMap<String, Option<SpecifierResolver>> = AHashMap::new();
    let mut writer = index_db.writer();
    let mut edges = 0usize;

    for (importer_key, importer_facts) in facts {
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
        let importer_rel = crate::path::RelPath::from(importer_key.as_str());

        for import in &importer_facts.imports {
            if import.is_type {
                continue;
            }
            let Some(target_rel) = resolver.resolve(root, importer_key, import) else {
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
            let Some(target_key) = target_rel.as_str() else {
                continue;
            };
            let Some(export_map) = export_maps.get(target_key) else {
                continue;
            };

            // A specific imported name (`from m import f`) joins against that export; an import with
            // no named symbol falls back to the resolver's default-export convention (JS `default`),
            // and `None` there means "no single export to bind" (bare Python/Java module import) — so
            // the join is skipped rather than mis-binding to an unrelated `default` symbol.
            let Some(wanted) = import.imported.as_deref().or_else(|| resolver.default_export_name()) else {
                continue;
            };
            if let Some(&name_start) = export_map.get(wanted) {
                // Emit a cross-file edge for the import binding site itself AND for every in-file use
                // of the imported name (calls/references whose intra `def_start` is this binding), so
                // `find_callers` / `goto_definition` resolve the real call sites across the boundary,
                // not just the `import` statement.
                let mut use_starts: Vec<u32> = vec![import.local_start];
                for edge in &importer_facts.import_uses {
                    if edge.def_start == import.local_start && !use_starts.contains(&edge.use_start) {
                        use_starts.push(edge.use_start);
                    }
                }
                for use_start in use_starts {
                    match writer.upsert_cross_file_edge(&target_rel, name_start, &importer_rel, use_start) {
                        Ok(()) => {
                            edges += 1;
                            if edges.is_multiple_of(COMMIT_BATCH) {
                                if let Err(error) = writer.commit() {
                                    tracing::warn!(%error, "cross-file stitch: batch commit failed — navigation may be stale");
                                }
                                writer = index_db.writer();
                            }
                        }
                        Err(error) => tracing::warn!(
                            importer = %importer_rel,
                            target = %target_rel,
                            %error,
                            "cross-file stitch: failed to stage edge — skipping"
                        ),
                    }
                }
            }
        }
    }

    if let Err(error) = writer.commit() {
        tracing::warn!(%error, "cross-file stitch: index commit failed — cross-file navigation may be stale");
        return;
    }
    tracing::debug!(edges, "cross-file stitch: staged cross-file resolved edges");
}
