//! The LanceDB side of the scan: [`LanceVectors`] implements the core
//! [`VectorStore`](crate::scan_retrieval::VectorStore) seam, so the scan writes, deletes and maintains
//! vector rows without naming LanceDB or the embedding presets (ADR-0023 rule 1).

#![cfg(any(feature = "documents", feature = "code-search"))]

use std::path::Path;

use crate::config::Config;
use crate::embeddings::SharedEmbedder;
#[cfg(feature = "code-search")]
use crate::lance::{CodeRow, PendingBatch};
use crate::scan_retrieval::{RetrievalError, VectorStore};
#[cfg(feature = "code-search")]
use crate::scanner_code::PendingCodeBatch;
use crate::store::Store;

/// The scan's vector store: the per-view LanceDB store a [`Store`] keeps open.
pub(crate) struct LanceVectors;

impl VectorStore for LanceVectors {
    fn open(&self, store: &mut Store, config: &Config) -> Result<(), RetrievalError> {
        let preset = &config.documents.embedding_preset;
        SharedEmbedder::load(preset, 0, config.resources.embed_batch_size)
            .and_then(|embedder| store.lance_or_open(embedder.dim(), preset).map(|_| ()))
            .map_err(|error| RetrievalError(format!("{error:#}")))
    }

    fn store_id_at(&self, dir: &Path) -> Option<String> {
        crate::lance::store_id(dir)
    }

    fn open_store_id(&self, store: &Store) -> Option<String> {
        store
            .lance
            .as_ref()
            .and_then(|lance| crate::lance::store_id(lance.dir()))
    }

    fn close(&self, store: &mut Store) {
        store.lance = None;
    }

    fn maintain(&self, store: &Store) {
        if let Some(lance) = store.lance.as_ref()
            && let Err(error) = lance.maintain(crate::lance::SCAN_RETENTION)
        {
            tracing::warn!(?error, "lance post-scan maintenance failed; old table versions kept");
        }
    }

    fn publish_snapshot(&self, store: &Store, config: &Config, scope: &str) -> Result<(), RetrievalError> {
        if !crate::lance::shared_snapshot::enabled() {
            return Ok(());
        }
        crate::lance::shared_snapshot::publish_scan(store, config, scope)
            .map_err(|error| RetrievalError(format!("{error:#}")))
    }

    #[cfg(feature = "code-search")]
    fn write_code_rows(&self, store: &mut Store, scope: &str, batches: Vec<PendingCodeBatch>, model: &str) -> usize {
        flush_code_batches(store, scope, batches, model)
    }

    #[cfg(feature = "code-search")]
    fn delete_code_rows(&self, store: &mut Store, config: &Config, scope: &str, paths: &[String]) -> bool {
        delete_code_rows(store, config, scope, paths)
    }
}

/// Assemble the LanceDB rows for a file's chunks + embeddings (parallel arrays), **consuming** both
/// so the chunk text and embedding vectors move into the rows instead of being cloned. Called at
/// flush time with the owned sidecar blob; `scope` is stamped directly (no second pass needed).
#[cfg(feature = "code-search")]
fn build_rows_owned(
    rel: &str,
    scope: &str,
    chunks: Vec<crate::chunk::CodeChunk>,
    embeddings: Vec<Vec<f32>>,
) -> Vec<CodeRow> {
    chunks
        .into_iter()
        .zip(embeddings)
        .map(|(c, emb)| CodeRow {
            scope: scope.to_string(),
            path: rel.to_string(),
            chunk_id: c.chunk_id,
            symbol: c.symbol.unwrap_or_default(),
            kind: c.kind.unwrap_or_default(),
            lang: c.lang,
            line_start: c.line_start,
            line_end: c.line_end,
            byte_start: c.byte_start,
            byte_end: c.byte_end,
            text: c.text,
            embedding: emb,
        })
        .collect()
}

/// Delete the `code_chunks` rows of `paths`, whether they left the scan set or stopped being
/// embed-eligible. Best-effort: never opens (i.e. never *creates*) the vector store when it does not
/// already exist, and logs — never propagates — a delete failure.
///
/// Returns `false` only when rows that exist could not be deleted (unknown preset, store open or
/// delete error); "nothing to delete" and "no vector store" are both `true`.
#[cfg(feature = "code-search")]
fn delete_code_rows(store: &mut Store, config: &Config, scope: &str, paths: &[String]) -> bool {
    let stale = paths;
    if stale.is_empty() {
        return true;
    }
    if store.lance.is_none() && !store.lance_dir_exists() {
        return true;
    }
    let model = &config.documents.embedding_preset;
    let dim = match SharedEmbedder::load(model, 0, config.resources.embed_batch_size) {
        Ok(embedder) => embedder.dim(),
        Err(error) => {
            tracing::warn!(?error, preset = %model, "code-chunk stale purge: unknown embedding preset; skipping");
            return false;
        }
    };
    let lance = match store.lance_or_open(dim, model) {
        Ok(lance) => lance.clone(),
        Err(error) => {
            tracing::warn!(?error, "code-chunk stale purge: open LanceStore failed; skipping");
            return false;
        }
    };
    if let Err(error) = lance.delete_code_chunks_for_paths(scope, stale) {
        tracing::warn!(
            paths = stale.len(),
            ?error,
            "code-chunk purge failed; search_code may return a removed or no-longer-embedded path"
        );
        return false;
    }
    true
}

/// Stream every pending code batch into the `code_chunks` LanceDB table, one file at a time. Opens
/// the store lazily — if no batch carries vectors (embeddings off) no LanceDB connection is made.
///
/// For each embedded file this re-reads the file's already-persisted `.chunk.msgpack` sidecar,
/// rebuilds its rows, writes them, and drops them before the next file — so peak embed-write RAM is
/// one file's rows plus one Arrow batch, independent of corpus size. (The sidecar is guaranteed on
/// disk here: every `embedding_dim > 0` batch either wrote it in `chunk_and_embed` or reused an
/// existing one, both before this post-barrier lane runs.)
///
/// Errors are logged and skipped per file so one malformed embedding never aborts the scan. Returns
/// the number of files for which rows were written.
#[cfg(feature = "code-search")]
fn flush_code_batches(store: &mut Store, scope: &str, batches: Vec<PendingCodeBatch>, embedding_model: &str) -> usize {
    let Some(dim) = batches.iter().find(|b| b.embedding_dim > 0).map(|b| b.embedding_dim) else {
        return 0;
    };
    match SharedEmbedder::load(
        embedding_model,
        0,
        crate::config::ResourcesConfig::default().embed_batch_size,
    ) {
        Ok(embedder) if embedder.dim() != dim => {
            tracing::error!(
                preset = %embedding_model,
                expected = embedder.dim(),
                actual = dim,
                "preset/runtime dim mismatch — refusing to write code_chunks batch"
            );
            return 0;
        }
        Ok(_) => {}
        Err(error) => {
            tracing::error!(?error, preset = %embedding_model, "unknown embedding preset — refusing to write code_chunks batch");
            return 0;
        }
    }

    let lance = match store.lance_or_open(dim, embedding_model) {
        Ok(s) => s.clone(),
        Err(error) => {
            tracing::error!(?error, "open LanceStore for code_chunks batch failed");
            return 0;
        }
    };

    let mut inserted = 0usize;
    let mut pending: PendingBatch<CodeRow> = PendingBatch::default();
    // Paths this call leaves owing nothing: rows written, or no rows to write. A path whose sidecar
    // could not be read or whose write failed stays owed for the next scan.
    let mut settled: Vec<String> = Vec::new();
    let write = |pending: &mut PendingBatch<CodeRow>, inserted: &mut usize, settled: &mut Vec<String>| {
        let items = pending.take();
        let files = items.len();
        let paths: Vec<String> = items.iter().map(|(_, path, _)| path.clone()).collect();
        match lance.replace_code_chunks_many(items) {
            Ok(()) => {
                *inserted += files;
                settled.extend(paths);
            }
            Err(error) => {
                tracing::warn!(
                    files,
                    ?error,
                    "lance replace_code_chunks_many failed; the files stay queued for the next scan"
                );
            }
        }
    };
    for batch in batches {
        if batch.embedding_dim == 0 {
            continue;
        }
        let blob = match store.read_chunks_by_hex(&batch.blob_hash) {
            Ok(Some(blob)) if blob.embedding_dim == dim && blob.embeddings.len() == blob.chunks.len() => blob,
            Ok(Some(_)) => {
                settled.push(batch.rel_path);
                continue;
            }
            // No sidecar, no rows to build: the file's next chunking queues it again.
            Ok(None) => {
                tracing::warn!(rel = %batch.rel_path, "chunk sidecar missing at flush; no vector rows to write");
                settled.push(batch.rel_path);
                continue;
            }
            Err(error) => {
                tracing::warn!(rel = %batch.rel_path, ?error, "re-read chunk sidecar failed; skipping vector rows");
                continue;
            }
        };
        if blob.chunks.is_empty() {
            settled.push(batch.rel_path);
            continue;
        }
        let rows = build_rows_owned(&batch.rel_path, scope, blob.chunks, blob.embeddings);
        pending.push(scope, &batch.rel_path, rows);
        if pending.is_full() {
            write(&mut pending, &mut inserted, &mut settled);
        }
    }
    if !pending.is_empty() {
        write(&mut pending, &mut inserted, &mut settled);
    }
    store.pending_vectors.settle_code(&settled);
    inserted
}

#[cfg(all(test, feature = "code-search"))]
mod tests {
    use crate::lance::CodeRow;
    use crate::scanner_code::purge_unembedded_code;
    use crate::scanner_filter::Filters;
    use crate::store::Store;

    fn seeded_code_lance(store: &mut Store, paths: &[&str]) -> crate::lance::LanceStore {
        let lance = store.lance_or_open(768, "balanced").expect("open lance").clone();
        for path in paths {
            let row = CodeRow {
                scope: "repo:s".to_string(),
                path: (*path).to_string(),
                chunk_id: format!("{path}:0"),
                symbol: "sym".to_string(),
                kind: "function".to_string(),
                lang: "rust".to_string(),
                line_start: 1,
                line_end: 1,
                byte_start: 0,
                byte_end: 4,
                text: "body".to_string(),
                embedding: vec![0.25; 768],
            };
            lance.replace_code_chunks("repo:s", path, vec![row]).expect("seed row");
            store.upsert(
                *path,
                crate::store::FileEntry {
                    hash_hex: "h".to_string(),
                    language: "rust".to_string(),
                    size_bytes: 1,
                    mtime: 0,
                    extract_epoch: crate::extract::EXTRACT_EPOCH,
                },
            );
        }
        lance
    }

    fn code_rows(lance: &crate::lance::LanceStore) -> Vec<String> {
        let mut paths: Vec<String> = lance
            .search_code_chunks("repo:s", vec![0.25; 768], 50)
            .expect("search")
            .into_iter()
            .map(|hit| hit.path)
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn embed_policy_purge_deletes_vectors_of_newly_ineligible_files_only() {
        crate::store::init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(tmp.path(), crate::store::VIEW_WORKING).unwrap();
        let lance = seeded_code_lance(&mut store, &["src/a.rs", "src/generated/b.rs", "lib/c.rs"]);
        assert_eq!(code_rows(&lance).len(), 3);

        let mut config = crate::config::default_for_root(std::path::Path::new("."));
        config.code_search.embed = true;
        config.code_search.embed_exclude = vec!["**/generated/**".to_string()];
        let filters = Filters::build(&config, Vec::new()).unwrap();
        purge_unembedded_code(&mut store, &config, &filters, "repo:s");
        assert_eq!(code_rows(&lance), vec!["lib/c.rs".to_string(), "src/a.rs".to_string()]);

        config.code_search.embed_include = vec!["src/**".to_string()];
        let filters = Filters::build(&config, Vec::new()).unwrap();
        purge_unembedded_code(&mut store, &config, &filters, "repo:s");
        assert_eq!(
            code_rows(&lance),
            vec!["src/a.rs".to_string()],
            "embed_include narrows retroactively"
        );

        config.code_search.embed = false;
        let filters = Filters::build(&config, Vec::new()).unwrap();
        purge_unembedded_code(&mut store, &config, &filters, "repo:s");
        assert!(code_rows(&lance).is_empty(), "embed = false removes every vector row");
        assert!(store.lookup("src/a.rs").is_some(), "files stay indexed");
    }

    #[test]
    fn a_purge_that_cannot_delete_reports_failure_and_leaves_the_rows() {
        crate::store::init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(tmp.path(), crate::store::VIEW_WORKING).unwrap();
        let lance = seeded_code_lance(&mut store, &["src/a.rs"]);

        let mut config = crate::config::default_for_root(std::path::Path::new("."));
        config.code_search.embed = false;
        let filters = Filters::build(&config, Vec::new()).unwrap();

        config.documents.embedding_preset = "no-such-preset".to_string();
        assert!(
            !purge_unembedded_code(&mut store, &config, &filters, "repo:s"),
            "an unknown preset means the delete never ran: not applied"
        );
        assert_eq!(code_rows(&lance), vec!["src/a.rs".to_string()], "nothing was deleted");

        config.documents.embedding_preset = "balanced".to_string();
        assert!(purge_unembedded_code(&mut store, &config, &filters, "repo:s"));
        assert!(code_rows(&lance).is_empty(), "the retry removes the row");
    }
}
