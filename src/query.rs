use thiserror::Error;

use crate::extract::{FileMapL1, FileMapL2, Symbol, SymbolKind};
use crate::path::RelPath;
use crate::store::{Store, StoreError};

#[derive(Debug, Error)]
pub enum QueryError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("file not indexed: {0}")]
    NotIndexed(String),
    #[error("blob missing for indexed file (likely .basemind/blobs/ was cleaned): {0}")]
    BlobMissing(String),
}

#[derive(Debug, Clone)]
pub struct SymbolHit {
    pub path: RelPath,
    pub symbol: Symbol,
}

/// Read an L1 map for the given relative path from the store.
pub fn file_outline(store: &Store, rel: impl AsRef<[u8]>) -> Result<FileMapL1, QueryError> {
    let rel_bytes = rel.as_ref();
    let entry = store
        .lookup(rel_bytes)
        .ok_or_else(|| QueryError::NotIndexed(String::from_utf8_lossy(rel_bytes).into_owned()))?;
    let l1 = store
        .read_l1_by_hex(&entry.hash_hex)?
        .ok_or_else(|| QueryError::BlobMissing(String::from_utf8_lossy(rel_bytes).into_owned()))?;
    Ok(l1)
}

/// Read or compute the L2 map for the given relative path.
///
/// If the L2 blob exists for the file's current content hash it is returned as-is.
/// Otherwise, this function reads the source from disk, runs extract_l2, writes the
/// blob, and returns it. "Becomes live on request."
pub fn file_outline_l2(store: &Store, rel: impl AsRef<[u8]>, root: &std::path::Path) -> Result<FileMapL2, QueryError> {
    let rel_bytes = rel.as_ref();
    let rel_display = String::from_utf8_lossy(rel_bytes).into_owned();
    let entry = store
        .lookup(rel_bytes)
        .ok_or_else(|| QueryError::NotIndexed(rel_display.clone()))?;
    if let Some(l2) = store.read_l2_by_hex(&entry.hash_hex)? {
        return Ok(l2);
    }
    // Live escalation: read source, extract, persist. The L1 outline already lives in the
    // combined frame; read it back so the rewrite carries both tiers (the frame is one blob).
    let l1 = store
        .read_l1_by_hex(&entry.hash_hex)?
        .ok_or_else(|| QueryError::BlobMissing(rel_display.clone()))?;
    let rel_path = RelPath::from(rel_bytes);
    let abs = root.join(rel_path.to_path_buf());
    let bytes = std::fs::read(&abs).map_err(|source| {
        QueryError::Store(StoreError::Io {
            path: abs.clone(),
            source,
        })
    })?;
    let lang = crate::lang::intern(&entry.language)
        .ok_or_else(|| QueryError::NotIndexed(format!("unknown language {}", entry.language)))?;
    let l2 = crate::extract::l2::extract_l2(lang, &bytes).map_err(|e| {
        QueryError::Store(StoreError::Io {
            path: abs,
            source: std::io::Error::other(format!("{e}")),
        })
    })?;
    store.write_filemap_hex(&entry.hash_hex, &l1, Some(&l2))?;
    Ok(l2)
}

/// Find all symbols across indexed files whose name matches `needle` (case-sensitive substring),
/// optionally filtered by kind.
///
/// Returns an empty `Vec` immediately when `needle` is empty — an empty substring matches
/// every symbol, which is never what callers want and is very expensive on large repos.
pub fn search_symbols(store: &Store, needle: &str, kind: Option<SymbolKind>) -> Result<Vec<SymbolHit>, QueryError> {
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let finder = memchr::memmem::Finder::new(needle.as_bytes());
    let mut out = Vec::new();
    for (rel, entry) in &store.index.files {
        let l1 = match store.read_l1_by_hex(&entry.hash_hex)? {
            Some(m) => m,
            None => continue,
        };
        for sym in l1.symbols {
            if finder.find(sym.name.as_bytes()).is_none() {
                continue;
            }
            if let Some(k) = kind
                && sym.kind != k
            {
                continue;
            }
            out.push(SymbolHit {
                path: rel.clone(),
                symbol: sym,
            });
        }
    }
    Ok(out)
}

/// Heuristic L3: read every L1, collect imports, return paths whose imports mention `module`.
pub fn dependents_of(store: &Store, module: &str) -> Result<Vec<RelPath>, QueryError> {
    // `RelPath: AsRef<Path>` so `l3::dependents_of` accepts `Vec<(RelPath, Vec<Import>)>`
    // without any `PathBuf` allocation per file. Only the matching results (typically small)
    // are then converted from `PathBuf` back to `RelPath` on the output side.
    let mut by_path: Vec<(RelPath, Vec<crate::extract::Import>)> = Vec::with_capacity(store.index.files.len());
    for (rel, entry) in &store.index.files {
        let l1 = match store.read_l1_by_hex(&entry.hash_hex)? {
            Some(m) => m,
            None => continue,
        };
        by_path.push((rel.clone(), l1.imports));
    }
    let paths = crate::extract::l3::dependents_of(module, &by_path);
    Ok(paths.into_iter().map(|p| RelPath::from(p.as_path())).collect())
}

/// Scope/import-resolved references to the definition at `(def_path, def_start)` — the resolved
/// backing for `find_references` / `find_callers`. Returns each binding `(use_path, use_start)`;
/// empty when the definition has no resolved uses.
///
/// When the Fjall index is open it answers from `refs_by_def` (intra **and** cross-file edges).
/// When it isn't — the documented read-only multi-session case, where a second `serve` lost the
/// single-holder Fjall lock — it falls back to the concurrently-readable `.rref` blobs. Intra-file
/// resolved edges are same-file, so only `def_path`'s own blob can hold references to a definition
/// in `def_path`: a single blob read, no full scan. Cross-file resolved refs live only in Fjall
/// and are therefore unavailable in the fallback (same caveat as `goto_definition`'s cross-file
/// hop).
pub fn resolved_references(store: &Store, def_path: &RelPath, def_start: u32) -> Vec<(RelPath, u32)> {
    match store.index_db.as_ref() {
        Some(index) => index.references_to(def_path, def_start),
        None => resolved_references_from_blob(store, def_path, def_start),
    }
}

/// Blob fallback for [`resolved_references`]: read `def_path`'s `.rref` blob and return every
/// intra edge whose definition endpoint is exactly `def_start`. A read error / missing blob
/// degrades to an empty result (the caller then falls back to the name-based scan), never an error.
fn resolved_references_from_blob(store: &Store, def_path: &RelPath, def_start: u32) -> Vec<(RelPath, u32)> {
    let Some(entry) = store.lookup(def_path) else {
        return Vec::new();
    };
    let refs = match store.read_resolved_by_hex(&entry.hash_hex) {
        Ok(Some(refs)) => refs,
        Ok(None) => return Vec::new(),
        Err(error) => {
            tracing::debug!(path = %def_path, %error, "resolved_references: resolution blob unreadable — no intra refs");
            return Vec::new();
        }
    };
    refs.intra
        .iter()
        .filter(|edge| edge.def_start == def_start)
        .map(|edge| (def_path.clone(), edge.use_start))
        .collect()
}

/// The definition the use at `(use_path, use_start)` resolves to — backs `goto_definition`.
/// `None` when the position isn't a resolved reference. Mirrors [`resolved_references`]: reads the
/// Fjall `refs_by_path` partition when the index is open, else the file's `.rref` blob (intra-file
/// bindings only; cross-file targets need the index open).
pub fn definition_of(store: &Store, use_path: &RelPath, use_start: u32) -> Option<(RelPath, u32)> {
    match store.index_db.as_ref() {
        Some(index) => index.definition_of(use_path, use_start),
        None => definition_of_from_blob(store, use_path, use_start),
    }
}

/// Blob fallback for [`definition_of`]: read `use_path`'s `.rref` blob and return the definition
/// endpoint of the intra edge whose use starts exactly at `use_start`.
fn definition_of_from_blob(store: &Store, use_path: &RelPath, use_start: u32) -> Option<(RelPath, u32)> {
    let entry = store.lookup(use_path)?;
    let refs = store.read_resolved_by_hex(&entry.hash_hex).ok().flatten()?;
    refs.intra
        .iter()
        .find(|edge| edge.use_start == use_start)
        .map(|edge| (use_path.clone(), edge.def_start))
}

// The blob-fallback parity tests need real resolved edges, which today only the oxc JS/TS engine
// produces intra-file. Feature-gated so a default build (locals-only) simply skips them.
#[cfg(all(test, feature = "code-intel-js"))]
mod tests {
    use super::*;
    use crate::config::ConfigV1;
    use crate::scanner::{ScanSource, scan};
    use crate::store::{Store, VIEW_WORKING};

    /// Scan a one-file TS fixture whose two `foo()` calls resolve to the local `function foo`,
    /// returning the opened store and its path handle.
    fn scan_foo_fixture(root: &std::path::Path) -> Store {
        std::fs::write(
            root.join("u.ts"),
            b"export function foo() { return 1; }\nfoo();\nfoo();\n",
        )
        .expect("u.ts");
        let mut store = Store::open(root, VIEW_WORKING).expect("open");
        scan(
            root,
            &mut store,
            &ConfigV1::with_defaults(),
            ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Inline,
        )
        .expect("scan");
        store
    }

    #[test]
    fn resolved_references_blob_fallback_matches_index_for_intra_refs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scan_foo_fixture(dir.path());
        let path = RelPath::from("u.ts".as_bytes());
        let entry = store.lookup(&path).expect("indexed");
        let refs = store
            .read_resolved_by_hex(&entry.hash_hex)
            .expect("read blob")
            .expect("resolution facts present");
        let def_start = refs
            .intra
            .first()
            .map(|e| e.def_start)
            .expect("at least one intra edge");

        let via_blob = resolved_references_from_blob(&store, &path, def_start);
        let via_index = store
            .index_db
            .as_ref()
            .expect("index open")
            .references_to(&path, def_start);
        assert!(!via_blob.is_empty(), "blob fallback must find the intra uses");
        assert_eq!(
            via_blob.len(),
            via_index.len(),
            "blob fallback and Fjall index must agree for single-file (all-intra) refs"
        );
        assert!(
            via_blob.iter().all(|(p, _)| *p == path),
            "single-file fixture → all uses in u.ts"
        );
    }

    #[test]
    fn definition_of_blob_fallback_returns_intra_definition() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scan_foo_fixture(dir.path());
        let path = RelPath::from("u.ts".as_bytes());
        let entry = store.lookup(&path).expect("indexed");
        let refs = store
            .read_resolved_by_hex(&entry.hash_hex)
            .expect("read blob")
            .expect("resolution facts present");
        let edge = refs.intra.first().cloned().expect("at least one intra edge");

        let def = definition_of_from_blob(&store, &path, edge.use_start);
        assert_eq!(
            def,
            Some((path.clone(), edge.def_start)),
            "blob fallback resolves the use back to its in-file definition"
        );
    }
}
