use std::path::PathBuf;

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
    let mut by_path: Vec<(PathBuf, Vec<crate::extract::Import>)> = Vec::with_capacity(store.index.files.len());
    for (rel, entry) in &store.index.files {
        let l1 = match store.read_l1_by_hex(&entry.hash_hex)? {
            Some(m) => m,
            None => continue,
        };
        by_path.push((rel.to_path_buf(), l1.imports));
    }
    let paths = crate::extract::l3::dependents_of(module, &by_path);
    Ok(paths.into_iter().map(|p| RelPath::from(p.as_path())).collect())
}

/// Scope/import-resolved references to the definition at `(def_path, def_start)` — the resolved
/// backing for `find_references`. Returns each binding `(use_path, use_start)`; empty when the
/// store has no writable index or the definition has no resolved uses (the caller falls back to
/// the name-based scan).
pub fn resolved_references(store: &Store, def_path: &RelPath, def_start: u32) -> Vec<(RelPath, u32)> {
    match store.index_db.as_ref() {
        Some(index) => index.references_to(def_path, def_start),
        None => Vec::new(),
    }
}

/// The definition the use at `(use_path, use_start)` resolves to — backs `goto_definition`.
/// `None` when the position isn't a resolved reference (or the store has no index).
pub fn definition_of(store: &Store, use_path: &RelPath, use_start: u32) -> Option<(RelPath, u32)> {
    store
        .index_db
        .as_ref()
        .and_then(|index| index.definition_of(use_path, use_start))
}
