//! The scan's side of the document retrieval store (ADR-0029 rules 2 and 11): bring the store in
//! line with the view's document map.
//!
//! Each document of the map has a `doc_key`, computed from the content hash and extraction digest
//! its entry records. The scan asks the store which keys are absent, reads blobs only for those,
//! and writes their rows; rows of keys no document of the view names are deleted. A wiped or
//! deleted store is therefore rebuilt by the next scan from the cached blobs, with no embedding.

use ahash::{AHashMap, AHashSet};

use crate::config::Config;
use crate::extract::doc::{DocBlobs, FileMapDoc};
use crate::lance::docs::{DocStore, doc_key, mime_group, store_dir};
use crate::lance::docs_schema::{DocChunkRow, DocRows, SECTION_SEPARATOR};
use crate::scanner_filter::Filters;
use crate::store::Store;

/// Documents per store write; a write holds their rows in memory.
const WRITE_DOCS: usize = crate::lance::BATCH_MAX_DOCS;
/// Chunk rows per store write.
const WRITE_ROWS: usize = crate::lance::BATCH_MAX_ROWS;

/// The view's retrieval store for the configured preset. With `create` false, `None` when no store
/// was ever written for it: removing rows must not create one.
pub(crate) fn open_doc_store(store: &Store, config: &Config, create: bool) -> Result<Option<DocStore>, String> {
    let preset = &config.documents.embedding_preset;
    let dim = super::preset_dim(preset).map_err(|error| error.to_string())?;
    let dir = store_dir(&store.basemind_dir, preset, dim);
    let opened = if create {
        DocStore::shared(&dir, dim, preset).map(Some)
    } else {
        DocStore::shared_existing(&dir, dim, preset)
    };
    opened.map_err(|error| format!("{error:#}"))
}

/// One live document: the blob its rows are built from, its paths, and whether any of them wants
/// vectors.
struct LiveDoc {
    blob: String,
    paths: Vec<String>,
    want_vectors: bool,
    /// The entry says the blob holds vectors of the preset its scan asked for.
    embedded: bool,
}

fn live_docs(store: &Store, config: &Config, filters: &Filters) -> AHashMap<String, LiveDoc> {
    let cfg = &config.documents;
    let mut live: AHashMap<String, LiveDoc> = AHashMap::new();
    for (path, entry) in &store.index.doc_files {
        let path = path.to_str_lossy().into_owned();
        let want = cfg.embed && filters.doc_embed_allowed(&path);
        let doc = live
            .entry(doc_key(&entry.hash_hex, &entry.config_digest))
            .or_insert_with(|| LiveDoc {
                blob: entry.hash_hex.clone(),
                paths: Vec::new(),
                want_vectors: false,
                embedded: false,
            });
        doc.want_vectors |= want;
        doc.embedded |= entry.embedded && entry.embedding_preset == cfg.embedding_preset;
        doc.paths.push(path);
    }
    live
}

/// The rows of one document. Over `max_chunks` chunks only the document row is written (no chunk
/// row, as before: such a document is tracked but not searched). A chunk carries its vector when
/// `with_vectors` and the vector is whole.
pub(crate) fn build_doc_rows(doc: FileMapDoc, key: String, with_vectors: bool, max_chunks: usize) -> DocRows {
    let title = doc
        .metadata
        .iter()
        .find(|(name, value)| {
            (name.eq_ignore_ascii_case("title") || name.eq_ignore_ascii_case("subject")) && !value.trim().is_empty()
        })
        .map(|(_, value)| value.trim().to_string());
    let mut keywords: Vec<String> = doc.keywords.iter().map(|k| k.text.trim().to_lowercase()).collect();
    keywords.sort_unstable();
    keywords.dedup();
    let dim = usize::from(doc.embedding_dim);
    let over_cap = doc.chunks.len() > max_chunks;
    let entities = &doc.entities;
    let chunks = if over_cap {
        Vec::new()
    } else {
        doc.chunks
            .into_iter()
            .enumerate()
            .map(|(ord, chunk)| {
                let mut entity_cats: Vec<String> = entities
                    .iter()
                    .filter(|e| e.start < chunk.byte_end && e.end > chunk.byte_start)
                    .map(|e| e.category.to_lowercase())
                    .collect();
                entity_cats.sort_unstable();
                entity_cats.dedup();
                let embedding = (with_vectors && dim > 0 && chunk.embedding.len() == dim).then_some(chunk.embedding);
                DocChunkRow {
                    chunk_ord: u32::try_from(ord).unwrap_or(u32::MAX),
                    chunk_kind: if chunk.kind.is_empty() {
                        "unknown".to_string()
                    } else {
                        chunk.kind
                    },
                    page_start: chunk.page_start,
                    page_end: chunk.page_end,
                    section: (!chunk.section.is_empty()).then(|| chunk.section.join(SECTION_SEPARATOR)),
                    line_start: chunk.line_start,
                    line_end: chunk.line_end,
                    byte_start: chunk.byte_start,
                    byte_end: chunk.byte_end,
                    entity_cats,
                    text: chunk.text,
                    embedding,
                }
            })
            .collect()
    };
    DocRows {
        doc_key: key,
        mime_group: mime_group(&doc.mime_type).to_string(),
        mime_type: doc.mime_type,
        title,
        keywords,
        chunks,
    }
}

/// Bring the store in line with the view's document map: write the rows of every live document
/// the store lacks (or holds without the vectors it now has), settle their paths in the owed set,
/// delete the rows no live document names, and bring the indexes up to date. Returns the number
/// of documents written. Failures are logged; what was not written stays owed.
///
/// The documents at `scanned` (this scan's) are always written when absent; at most `budget` other
/// absent documents are, so an incremental or cancelled pass does not read every blob a wiped
/// store lacks. The rest are recorded as owed for a later pass.
pub(crate) fn sync_document_rows(
    store: &mut Store,
    config: &Config,
    filters: &Filters,
    scanned: &AHashSet<String>,
    budget: usize,
) -> usize {
    let live = live_docs(store, config, filters);
    let doc_store = match open_doc_store(store, config, !live.is_empty()) {
        Ok(Some(doc_store)) => doc_store,
        Ok(None) => return 0,
        Err(error) => {
            tracing::warn!(%error, "document store did not open; document rows stay owed");
            return 0;
        }
    };
    let keys: Vec<String> = live.keys().cloned().collect();
    let present = match doc_store.present(&keys) {
        Ok(present) => present,
        Err(error) => {
            tracing::warn!(?error, "listing the documents the store holds failed; rows stay owed");
            return 0;
        }
    };
    let absent: Vec<&String> = keys
        .iter()
        .filter(|key| match present.get(*key) {
            None => true,
            Some(&has_vectors) => !has_vectors && live[*key].want_vectors && live[*key].embedded,
        })
        .collect();
    let (mut todo, others): (Vec<&String>, Vec<&String>) = absent
        .into_iter()
        .partition(|key| live[*key].paths.iter().any(|path| scanned.contains(path)));
    let (within, beyond) = others.split_at(others.len().min(budget));
    todo.extend_from_slice(within);
    for key in beyond {
        for path in &live[*key].paths {
            store.pending_vectors.owe_doc(path);
        }
    }
    let preset = config.documents.embedding_preset.clone();
    let max_chunks = config.documents.max_chunks_per_document;
    let mut written = 0usize;
    let mut settled: Vec<String> = keys
        .iter()
        .filter(|key| present.contains_key(*key))
        .flat_map(|key| live[key].paths.iter().cloned())
        .collect();
    let mut blobless: Vec<String> = Vec::new();
    let mut pending: Vec<DocRows> = Vec::new();
    let mut pending_paths: Vec<String> = Vec::new();
    let mut rows = 0usize;
    let mut flush = |pending: &mut Vec<DocRows>, pending_paths: &mut Vec<String>, rows: &mut usize| {
        if pending.is_empty() {
            return;
        }
        match doc_store.write(pending) {
            Ok(()) => {
                written += pending.len();
                settled.append(pending_paths);
            }
            Err(error) => {
                tracing::warn!(
                    docs = pending.len(),
                    ?error,
                    "writing document rows failed; they stay owed"
                );
                pending_paths.clear();
            }
        }
        pending.clear();
        *rows = 0;
    };
    for key in todo {
        let doc = &live[key];
        match store.read_doc_by_hex(&doc.blob) {
            Ok(Some(blob)) => {
                let with_vectors = doc.want_vectors && super::doc_has_vectors_of(&blob, &preset);
                let built = build_doc_rows(blob, key.clone(), with_vectors, max_chunks);
                rows += built.chunks.len();
                pending.push(built);
                pending_paths.extend(doc.paths.iter().cloned());
                if pending.len() >= WRITE_DOCS || rows >= WRITE_ROWS {
                    flush(&mut pending, &mut pending_paths, &mut rows);
                }
            }
            // No blob: the entry's next extraction writes one; nothing to build rows from now, so
            // nothing is owed until then.
            Ok(None) => {
                tracing::debug!(
                    key,
                    "no blob for a live document; its rows wait for its next extraction"
                );
                blobless.extend(doc.paths.iter().cloned());
            }
            Err(error) => tracing::warn!(key, ?error, "reading a document blob failed; its rows stay owed"),
        }
    }
    flush(&mut pending, &mut pending_paths, &mut rows);
    drop(flush);
    settled.append(&mut blobless);
    // An owed path the view no longer tracks has no rows to come.
    let tracked: AHashSet<&str> = live
        .values()
        .flat_map(|doc| doc.paths.iter().map(String::as_str))
        .collect();
    settled.extend(
        store
            .pending_vectors
            .docs()
            .iter()
            .map(|rel| rel.to_str_lossy().into_owned())
            .filter(|rel| !tracked.contains(rel.as_str())),
    );
    drop(tracked);
    store.pending_vectors.settle_docs(&settled);
    // The view's store holds this view's rows only, so a key the view does not name is dead. When
    // the store moves to the repository group (ADR-0028) this call moves to the group sweep, which
    // passes the live keys of every view.
    let live_keys: AHashSet<String> = live.into_keys().collect();
    if let Err(error) = doc_store.delete_dead(&live_keys) {
        tracing::warn!(
            ?error,
            "deleting dead document rows failed; they are filtered out at query time"
        );
    }
    if let Err(error) = doc_store.maintain_indexes() {
        tracing::warn!(?error, "document index upkeep failed; searches scan the unindexed rows");
    }
    written
}

/// Delete the rows of documents the view no longer names. No store is created for it.
pub(crate) fn remove_dead_document_rows(store: &Store, config: &Config, filters: &Filters) {
    let live = live_docs(store, config, filters);
    match open_doc_store(store, config, false) {
        Ok(Some(doc_store)) => {
            let live_keys: AHashSet<String> = live.into_keys().collect();
            if let Err(error) = doc_store.delete_dead(&live_keys) {
                tracing::warn!(
                    ?error,
                    "deleting removed documents' rows failed; they are filtered out at query time"
                );
            }
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, "document store did not open; removed documents' rows stay"),
    }
}

/// Rewrite without vectors the rows of documents no path of which may be embedded any more. They
/// stay searchable by the lexical lane. `false` when rows that carry vectors could not be rewritten.
pub(crate) fn strip_unwanted_vectors(store: &Store, config: &Config, filters: &Filters) -> bool {
    let live = live_docs(store, config, filters);
    let unwanted: Vec<String> = live
        .iter()
        .filter(|(_, doc)| !doc.want_vectors)
        .map(|(key, _)| key.clone())
        .collect();
    if unwanted.is_empty() {
        return true;
    }
    let doc_store = match open_doc_store(store, config, false) {
        Ok(Some(doc_store)) => doc_store,
        Ok(None) => return true,
        Err(error) => {
            tracing::warn!(%error, "document store did not open; excluded documents keep their vectors");
            return false;
        }
    };
    let present = match doc_store.present(&unwanted) {
        Ok(present) => present,
        Err(error) => {
            tracing::warn!(?error, "listing documents with vectors failed");
            return false;
        }
    };
    let max_chunks = config.documents.max_chunks_per_document;
    let mut rewrite = Vec::new();
    for key in unwanted.iter().filter(|key| present.get(*key) == Some(&true)) {
        if let Ok(Some(blob)) = store.read_doc_by_hex(&live[key].blob) {
            rewrite.push(build_doc_rows(blob, key.clone(), false, max_chunks));
        }
    }
    let mut applied = true;
    for part in rewrite.chunks(WRITE_DOCS) {
        if let Err(error) = doc_store.write(part) {
            tracing::warn!(?error, "rewriting rows without vectors failed");
            applied = false;
        }
    }
    applied
}

#[cfg(test)]
#[path = "scanner_docs_store_tests.rs"]
mod tests;
