//! Cross-file JS/TS resolution stitch (feature `code-intel-js`).
//!
//! The per-file resolve pass caches each file's *intra*-file resolved edges plus its import and
//! export lists (see [`crate::intel::model::FileResolvedRefs`]). This module runs once at the end
//! of that pass and computes the piece those per-file facts deliberately omit: the **cross-file**
//! edge that links an importer's local binding to the exported definition it actually refers to.
//!
//! The join is:
//!
//! 1. For each importer file, resolve every runtime (`is_type == false`) [`ImportEdge`]'s module
//!    `specifier` with [`oxc_resolver`] (Node/tsconfig-style: extension + index + `package.json`
//!    resolution) relative to the importer's directory → an absolute on-disk path.
//! 2. Convert that path to a repo-relative [`RelPath`] and require it to be an indexed file.
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
use oxc_resolver::{ResolveOptions, Resolver};

use crate::index::IndexDb;
use crate::intel::model::{ExportEdge, ImportEdge};
use crate::path::RelPath;
use crate::store::Store;

/// Import/export facts for one file, harvested during the resolve pass's per-file loop so the
/// join needs no second blob read. Only files that import or export something are kept.
pub struct FileFacts {
    pub imports: Vec<ImportEdge>,
    pub exports: Vec<ExportEdge>,
}

/// The ES-module name a default import (`import x from ...`) binds to — its target's `default`
/// export. Namespace imports (`import * as ns`) are dropped upstream in the oxc analysis (they bind
/// a whole-module object with no single export site), so only genuine default imports — whose
/// `imported` is `None` — reach this fallback.
const DEFAULT_EXPORT_NAME: &str = "default";

/// Commit the cross-file edge batch in bounded chunks, matching the primary scan's
/// `INDEX_COMMIT_BATCH`: caps peak memory and periodically releases Fjall's write lock so a
/// concurrent MCP reader isn't blocked for the whole stitch.
const COMMIT_BATCH: usize = 256;

/// JS/TS module-resolution extensions, TS-first so a bare `./util` specifier binds to `util.ts`
/// before `util.js` (matching `tsc`'s module resolution precedence).
const RESOLVE_EXTENSIONS: &[&str] = &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];

/// Build the resolver once per stitch. Configured for TS-aware Node resolution: TS extensions win,
/// a `./util.js` specifier maps back to `util.ts` (TS's rewritten-extension convention), and the
/// standard import/require conditions are enabled for `package.json` `exports` maps.
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
        // Do NOT canonicalize symlinks: we map the resolved path back to a repo-relative key via
        // `strip_prefix(root)`, and canonicalization would rewrite `root` (e.g. macOS `/var` →
        // `/private/var`, or any symlinked checkout) out from under that prefix, dropping the edge.
        symlinks: false,
        ..ResolveOptions::default()
    })
}

/// Convert an absolute resolved path back to a repo-relative [`RelPath`] (forward-slashed to match
/// the scanner's key convention). Returns `None` for paths outside `root` or non-UTF-8 paths.
fn to_repo_relative(root: &Path, target_abs: &Path) -> Option<RelPath> {
    let rel = target_abs.strip_prefix(root).ok()?;
    let normalized = rel.to_str()?.replace('\\', "/");
    Some(RelPath::from(normalized.as_str()))
}

/// Stitch cross-file resolved edges for every importer in `facts` into the index.
///
/// Runs after the resolve pass's per-file upserts have committed, so each importer's previous-scan
/// edges are already purged (see the idempotency note on `upsert_cross_file_edge`). Best-effort:
/// never returns an error — resolver misses and commit failures are logged and swallowed so the
/// scan still succeeds with the name-based fallback intact.
pub fn stitch_cross_file_edges(root: &Path, store: &Store, index_db: &IndexDb, facts: &AHashMap<String, FileFacts>) {
    if facts.is_empty() {
        return;
    }
    // Pre-index every target file's exports by name once, so the per-import lookup is O(1) instead
    // of a linear scan over a barrel file's hundreds of re-exports.
    let export_maps: AHashMap<&str, AHashMap<&str, u32>> = facts
        .iter()
        .filter(|(_, f)| !f.exports.is_empty())
        .map(|(key, f)| {
            let by_name: AHashMap<&str, u32> = f.exports.iter().map(|e| (e.name.as_str(), e.name_start)).collect();
            (key.as_str(), by_name)
        })
        .collect();

    let resolver = build_resolver();
    let mut writer = index_db.writer();
    let mut edges = 0usize;

    for (importer_key, importer_facts) in facts {
        if importer_facts.imports.is_empty() {
            continue;
        }
        let importer_abs = root.join(importer_key);
        let Some(importer_dir) = importer_abs.parent() else {
            continue;
        };
        let importer_rel = RelPath::from(importer_key.as_str());

        for import in &importer_facts.imports {
            // Type-only imports are erased at runtime — no runtime reference edge.
            if import.is_type {
                continue;
            }
            let target_abs = match resolver.resolve(importer_dir, &import.specifier) {
                Ok(resolution) => resolution.full_path(),
                Err(error) => {
                    tracing::debug!(
                        importer = %importer_rel,
                        specifier = %import.specifier,
                        %error,
                        "cross-file stitch: specifier did not resolve — skipping"
                    );
                    continue;
                }
            };
            let Some(target_rel) = to_repo_relative(root, &target_abs) else {
                continue; // resolved outside the repo (e.g. a symlinked node_modules) — not indexed
            };
            // The target must be an indexed file; unindexed targets have no export facts to bind.
            if store.lookup(&target_rel).is_none() {
                continue;
            }
            let Some(target_key) = target_rel.as_str() else {
                continue;
            };
            let Some(export_map) = export_maps.get(target_key) else {
                continue; // target exports nothing (nothing to bind to)
            };

            let wanted = import.imported.as_deref().unwrap_or(DEFAULT_EXPORT_NAME);
            if let Some(&name_start) = export_map.get(wanted) {
                match writer.upsert_cross_file_edge(&target_rel, name_start, &importer_rel, import.local_start) {
                    Ok(()) => {
                        edges += 1;
                        if edges % COMMIT_BATCH == 0 {
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

    if let Err(error) = writer.commit() {
        tracing::warn!(%error, "cross-file stitch: index commit failed — cross-file navigation may be stale");
        return;
    }
    tracing::debug!(edges, "cross-file stitch: staged cross-file resolved edges");
}
