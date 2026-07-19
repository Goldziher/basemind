//! The per-file scan pipeline: everything that happens to ONE candidate path, plus the rayon pool
//! the pipeline runs on.
//!
//! `process_file` is the unit of work `scanner::scan` / `scan_paths` fan out over: detect the
//! language, short-circuit an unchanged file (mtime+size, then content hash), read the bytes from
//! the active [`ScanSource`], classify (binary / non-UTF-8 / too large), extract or reuse the L1+L2
//! blobs, stage the Fjall index upsert, and (feature-gated) chunk / embed. The reader helpers
//! ([`read_working_tree`] / [`read_via_git`]), the classifiers ([`looks_binary`]), the
//! unchanged-file gates, and the [`WorkerIndexBatch`] accumulator are all in service of that one
//! pipeline and change with it.
//!
//! Carved out of `scanner.rs` (which was over the 1000-line module cap): what stays there is the
//! *orchestration* — candidate enumeration, the stale-entry purge, `apply_outcomes`, and the
//! post-barrier lanes — which changes for a different reason than the per-file work does.
//! `scanner.rs` re-exports [`looks_binary`], so its callers are unaffected.

use std::path::Path;
use std::time::SystemTime;

use rayon::prelude::*;

use crate::config::Config;
use crate::extract::{self, ExtractError, FileMapL1, FileMapL2};
use crate::git::GitError;
use crate::hashing;
use crate::index::{IndexDb, writer::IndexWriter};
use crate::lang;
use crate::path::RelPath;
use crate::scanner::{EmbedMode, FileResult, FileStatus, ScanSource};
#[cfg(feature = "documents")]
use crate::scanner_docs::{extract_and_persist_doc, should_extract_document};
use crate::scanner_filter::Filters;
use crate::store::{FileEntry, Store};

/// Number of files whose index entries are accumulated into one Fjall write batch before
/// committing. Each `IndexWriter::commit` takes Fjall's single write lock, so committing
/// per file made every rayon worker serialize on that lock (a flamegraph attributed ~14%
/// of scan wall-time to `__psynch_mutexwait` here). Batching `N` files per commit cuts the
/// commit count — and thus the lock-contention — by ~`N`× while keeping each worker's
/// staged work bounded in memory. The per-file read-before-write atomicity is preserved:
/// every file still stages its own deletes+inserts; only the *flush boundary* moved.
const INDEX_COMMIT_BATCH: usize = 256;

/// Per-rayon-worker accumulator: buffers each file's index upsert into a shared Fjall write
/// batch and commits once `INDEX_COMMIT_BATCH` files have been staged (and once more at the
/// end of the worker's slice). Also carries the worker's `FileResult`s so the parallel fold
/// produces both the scan outcomes and the committed index in one pass.
///
/// Borrows `&IndexDb` (cheap `Arc`-backed handle) for the worker's lifetime. When the store
/// has no index (`index_db == None`, read-only mode) staging is a no-op.
struct WorkerIndexBatch<'a> {
    index: Option<&'a IndexDb>,
    writer: Option<IndexWriter>,
    staged: usize,
    results: Vec<FileResult>,
}

impl<'a> WorkerIndexBatch<'a> {
    fn new(store: &'a Store) -> Self {
        Self {
            index: store.index_db.as_ref(),
            writer: None,
            staged: 0,
            results: Vec::new(),
        }
    }

    /// Stage one file's symbols / calls / imports into the current batch, committing first
    /// if the batch is already full. Returns `false` only when the upsert itself failed
    /// (caller logs); a `None` index is a successful no-op.
    fn stage(&mut self, rel: &RelPath, l1: &FileMapL1, l2: Option<&FileMapL2>) -> bool {
        let Some(index) = self.index else {
            return true;
        };
        let writer = self.writer.get_or_insert_with(|| index.writer());
        if writer.upsert_file(rel, l1, l2).is_err() {
            return false;
        }
        self.staged += 1;
        if self.staged >= INDEX_COMMIT_BATCH {
            self.commit();
        }
        true
    }

    /// Stage one file's BM25 keyword postings into the current batch, reusing the same
    /// [`IndexWriter`] as [`Self::stage`] so the symbol upsert and the keyword postings ride the
    /// same per-file commit. Does not touch the file counter (the file was already counted by
    /// `stage`) and never force-commits. A `None` index is a successful no-op.
    #[cfg(feature = "code-search")]
    fn stage_bm25(&mut self, rel: &RelPath, postings: &[crate::search::bm25::ChunkPosting]) {
        let Some(index) = self.index else {
            return;
        };
        let writer = self.writer.get_or_insert_with(|| index.writer());
        if writer.upsert_bm25_file(rel, postings).is_err() {
            tracing::warn!(rel = %rel, "bm25 upsert failed; keyword search may be incomplete");
        }
    }

    /// Flush the staged batch under Fjall's write lock and reset the counter.
    fn commit(&mut self) {
        if let Some(writer) = self.writer.take()
            && writer.commit().is_err()
        {
            tracing::warn!("index batch commit failed; reference search may be incomplete");
        }
        self.staged = 0;
    }

    /// Commit the trailing partial batch and hand back the worker's results.
    fn finish(mut self) -> Vec<FileResult> {
        self.commit();
        self.results
    }
}

/// Per-worker stack size for the scanner's rayon pool. Tree-sitter parse trees and the recursive
/// msgpack (de)serialization of large extraction blobs can recurse deeper than rayon's small default
/// worker stack (~2 MiB), overflowing it on pathological inputs (deeply-nested or machine-generated
/// files) — observed as a hard `stack overflow` abort on a full fresh scan of a large real repo. A
/// full scan of that repo was measured to complete under a 128 MiB stack, so 256 MiB carries a 2×
/// margin. The stack is reserved lazily (VA only, not committed until touched), so this large ceiling
/// costs nothing on the common shallow path.
const SCANNER_STACK_SIZE: usize = 256 * 1024 * 1024;

/// Process-wide rayon pool the scanner runs its per-file `par_iter` on: sized like the default global
/// pool but with a much larger per-worker stack (see [`SCANNER_STACK_SIZE`]). Lazily built once.
///
/// `scan_threads` caps the pool from `[resources].scan_threads`: `0` (auto) keeps rayon's default
/// (one worker per logical CPU); a non-zero value pins the worker count so the scan cannot saturate
/// every core on a shared machine. The pool is built on first call and its size is then fixed for the
/// process — the first caller's `scan_threads` wins, mirroring [`crate::embeddings::embed_pool`].
pub(crate) fn scanner_pool(scan_threads: usize) -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let mut builder = rayon::ThreadPoolBuilder::new()
            .stack_size(SCANNER_STACK_SIZE)
            .thread_name(|i| format!("bm-scan-{i}"));
        if scan_threads > 0 {
            builder = builder.num_threads(scan_threads);
        }
        builder.build().expect("build scanner rayon pool")
    })
}

/// Drive the per-file pipeline across `candidates` on the rayon pool, batching index commits
/// per worker. Order of the returned `FileResult`s is unspecified (the parallel fold
/// concatenates per-worker slices) — every consumer keys by `path`, never by position.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_candidates(
    candidates: &[String],
    root: &Path,
    filters: &Filters,
    store: &Store,
    source: &ScanSource<'_>,
    config: &Config,
    scope: &str,
    embed: EmbedMode,
) -> Vec<FileResult> {
    scanner_pool(config.resources.scan_threads).install(|| {
        candidates
            .par_iter()
            .fold(
                || WorkerIndexBatch::new(store),
                |mut batch, rel| {
                    let result = process_file(root, rel, filters, store, source, config, scope, &mut batch, embed);
                    batch.results.push(result);
                    batch
                },
            )
            .map(WorkerIndexBatch::finish)
            .reduce(Vec::new, |mut a, mut b| {
                a.append(&mut b);
                a
            })
    })
}

/// True when the extraction blobs required to classify a file as `Unchanged` are present on disk for
/// `hash_hex`: the combined-filemap blob always, plus the code-chunk sidecar when code-search
/// chunking is enabled (so toggling the feature on re-chunks rather than being skipped as unchanged).
fn extraction_sidecars_present(store: &Store, config: &Config, hash_hex: &str) -> bool {
    #[cfg(not(feature = "code-search"))]
    let _ = config;
    if !store.blob_path_fm_hex(hash_hex).exists() {
        return false;
    }
    #[cfg(feature = "code-search")]
    if crate::scanner_code::should_chunk(config) && !store.blob_path_chunk_hex(hash_hex).exists() {
        return false;
    }
    true
}

/// True when the on-disk chunk sidecar already satisfies THIS scan's embedding requirement, so an
/// otherwise-unchanged file may be short-circuited as `Unchanged`.
///
/// This is distinct from [`extraction_sidecars_present`] (blob *presence*, which governs l1/l2/chunk
/// reuse): a chunk-only sidecar is present but carries no vectors. A `Deferred` pass never embeds, so
/// any present sidecar satisfies it; an `Inline` pass over an embed-eligible file requires embeddings
/// for the current preset to already exist, else the file must be re-processed to fill them. The
/// daemon writes chunk-only sidecars in `Deferred`, which a later `Inline` pass upgrades in place —
/// without this gate the Inline pass would short-circuit past `chunk_and_embed` and never embed.
#[cfg(feature = "code-search")]
fn embed_state_satisfied(store: &Store, config: &Config, rel: &str, hash_hex: &str, mode: EmbedMode) -> bool {
    if !matches!(mode, EmbedMode::Inline) {
        return true;
    }
    let cfg = &config.code_search;
    if !cfg.embed || crate::scanner_filter::embed_excluded(rel, &cfg.embed_exclude) {
        return true;
    }
    // A schema/state peek is enough here — this is a stat-only fast path, so avoid the full
    // chunk-text decode `read_chunks_by_hex` would pay for on every unchanged, embed-eligible file.
    match store.peek_chunk_state(hash_hex) {
        // A file with no chunks has nothing to embed; anything with vectors for the active preset is
        // already satisfied. A non-empty, unembedded (dim-0) sidecar must be re-processed.
        Ok(Some(peek)) => {
            peek.chunks.is_empty()
                || (peek.embedding_dim > 0
                    && peek.embedding_model == config.documents.embedding_preset
                    && peek.embeddings.len() == peek.chunks.len())
        }
        _ => false,
    }
}

/// Without `code-search` there is no embedding tier, so every scan's embed requirement is trivially
/// satisfied.
#[cfg(not(feature = "code-search"))]
fn embed_state_satisfied(_store: &Store, _config: &Config, _rel: &str, _hash_hex: &str, _mode: EmbedMode) -> bool {
    true
}

/// Process a single relative path. Returns a `FileResult`; if the file is being
/// updated, the new `FileEntry` is attached via `FileResult::upsert` so the caller
/// can apply it to the store from the single-threaded apply loop.
#[allow(clippy::too_many_arguments)]
fn process_file(
    root: &Path,
    rel: &str,
    filters: &Filters,
    store: &Store,
    source: &ScanSource<'_>,
    config: &Config,
    scope: &str,
    index_batch: &mut WorkerIndexBatch<'_>,
    embed: EmbedMode,
) -> FileResult {
    #[cfg(not(feature = "documents"))]
    {
        let _ = (config, scope);
    }
    #[cfg(not(any(feature = "documents", feature = "code-search")))]
    {
        let _ = embed;
    }
    let lang = match lang::detect(Path::new(rel)) {
        Some(l) => l,
        None => {
            #[cfg(feature = "documents")]
            {
                if matches!(source, ScanSource::WorkingTree) {
                    return process_doc(root, rel, filters, store, config, scope, embed);
                }
            }
            return FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang);
        }
    };

    if matches!(source, ScanSource::WorkingTree)
        && let Some(existing) = store.lookup(rel)
        && existing.mtime != 0
        && let Ok(meta) = std::fs::metadata(root.join(rel))
    {
        let mtime = mtime_nanos(&meta);
        if meta.len() == existing.size_bytes
            && mtime == existing.mtime
            && extraction_sidecars_present(store, config, &existing.hash_hex)
            && embed_state_satisfied(store, config, rel, &existing.hash_hex, embed)
        {
            return FileResult::bare(rel.to_string(), FileStatus::Unchanged);
        }
    }

    let (bytes, size_bytes, mtime) = match source {
        ScanSource::WorkingTree => match read_working_tree(root, rel, filters) {
            Ok(triple) => triple,
            Err(status) => {
                return FileResult::bare(rel.to_string(), status);
            }
        },
        ScanSource::Staged(repo) => match read_via_git(filters, repo.read_blob_staged(rel)) {
            Ok(triple) => triple,
            Err(status) => {
                return FileResult::bare(rel.to_string(), status);
            }
        },
        ScanSource::Rev { repo, sha } => match read_via_git(filters, repo.read_blob_at_rev(sha, rel)) {
            Ok(triple) => triple,
            Err(status) => {
                return FileResult::bare(rel.to_string(), status);
            }
        },
    };

    if looks_binary(&bytes) {
        return FileResult::bare(rel.to_string(), FileStatus::SkippedBinary);
    }

    if std::str::from_utf8(&bytes).is_err() {
        return FileResult::bare(rel.to_string(), FileStatus::SkippedNonUtf8);
    }

    let hash = hashing::hash_bytes(&bytes);
    let hex_buf = hashing::hex_buf(&hash);
    let hash_hex_str = hashing::hex_str(&hex_buf);

    let sidecars_present = extraction_sidecars_present(store, config, hash_hex_str);
    if let Some(existing) = store.lookup(rel)
        && existing.hash_hex == hash_hex_str
        && sidecars_present
        && embed_state_satisfied(store, config, rel, hash_hex_str, embed)
    {
        return FileResult::bare(rel.to_string(), FileStatus::Unchanged);
    }

    let want_l2 = filters.eager_l2 && store.index_db.is_some();

    let reused_pair: Option<(FileMapL1, Option<FileMapL2>)> = if sidecars_present {
        match store.read_l1_by_hex(hash_hex_str) {
            Ok(Some(l1)) => {
                let l2 = if want_l2 {
                    store.read_l2_by_hex(hash_hex_str).unwrap_or(None)
                } else {
                    None
                };
                if want_l2 && l2.is_none() { None } else { Some((l1, l2)) }
            }
            _ => None,
        }
    } else {
        None
    };
    let reused = reused_pair.is_some();

    let (l1, l2_opt): (FileMapL1, Option<FileMapL2>) = match reused_pair {
        Some(pair) => pair,
        None => match extract::extract_l1_l2(lang, &bytes, want_l2) {
            Ok(pair) => pair,
            Err(ExtractError::ParseTimeout(_)) => {
                return FileResult::bare(rel.to_string(), FileStatus::ParseTimedOut);
            }
            Err(source) => {
                return FileResult::bare(
                    rel.to_string(),
                    FileStatus::ExtractFailed {
                        msg: format_extract_err(&source),
                    },
                );
            }
        },
    };

    let l2: Option<FileMapL2> = l2_opt;
    if !reused && let Err(e) = store.write_filemap_hex(hash_hex_str, &l1, l2.as_ref()) {
        return FileResult::bare(rel.to_string(), FileStatus::ExtractFailed { msg: e.to_string() });
    }

    let rel_path = RelPath::from(rel);
    if !index_batch.stage(&rel_path, &l1, l2.as_ref()) {
        tracing::warn!(rel, "index upsert failed; reference search may be incomplete");
    }

    #[cfg(feature = "code-search")]
    let code_batch = if crate::scanner_code::should_chunk(config) {
        match crate::scanner_code::chunk_and_embed(store, rel, &bytes, &l1, l2.as_ref(), hash_hex_str, config, embed) {
            Ok(batch) => batch,
            Err(error) => {
                tracing::debug!(
                    rel,
                    ?error,
                    "code chunk/embed failed; skipping code-search for this file"
                );
                None
            }
        }
    } else {
        None
    };

    // Stage BM25 postings into Fjall here in the worker, then clear them: they are consumed at this
    // point, and the descriptor that follows into `FileResult` is accumulated corpus-wide, so it
    // must stay metadata-only (see `PendingCodeBatch`).
    #[cfg(feature = "code-search")]
    let code_batch = code_batch.map(|mut batch| {
        index_batch.stage_bm25(&rel_path, &batch.bm25);
        batch.bm25 = Vec::new();
        batch
    });

    let entry = FileEntry {
        hash_hex: hash_hex_str.to_string(),
        language: lang.to_string(),
        size_bytes,
        mtime,
    };
    FileResult {
        path: rel.to_string(),
        status: FileStatus::Updated {
            had_errors: l1.had_errors,
            error_count: l1.error_count,
            reused,
        },
        upsert: Some(entry),
        #[cfg(feature = "documents")]
        doc_batch: None,
        #[cfg(feature = "documents")]
        doc_upsert: None,
        #[cfg(feature = "code-search")]
        code_batch,
    }
}

/// Document-tier branch: file had no tree-sitter language; check `[documents]` config and
/// route through xberg. Always returns a `FileResult` — falls back to `SkippedNoLang`
/// when documents are disabled or the MIME type is filtered out.
#[cfg(feature = "documents")]
fn process_doc(
    root: &Path,
    rel: &str,
    filters: &Filters,
    store: &Store,
    config: &Config,
    scope: &str,
    embed: EmbedMode,
) -> FileResult {
    let abs = root.join(rel);
    let effective_scope = crate::scanner_docs::doc_scope_for(rel, scope, config);
    let scope = effective_scope.as_ref();
    let Some(mime_type) = should_extract_document(&abs, &config.documents) else {
        return FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang);
    };
    let (bytes, size_bytes, mtime) = match read_working_tree(root, rel, filters) {
        Ok(triple) => triple,
        Err(status) => return FileResult::bare(rel.to_string(), status),
    };
    let hash = hashing::hash_bytes(&bytes);
    let hex_buf = hashing::hex_buf(&hash);
    let hash_hex = hashing::hex_str(&hex_buf);

    if let Some(existing) = store.lookup_doc(rel)
        && existing.hash_hex == hash_hex
        && existing.embedding_preset == config.documents.embedding_preset
        && store.blob_path_doc_hex(hash_hex).exists()
    {
        return FileResult::bare(rel.to_string(), FileStatus::Unchanged);
    }

    let doc_entry = crate::store::DocEntry {
        hash_hex: hash_hex.to_string(),
        embedding_preset: config.documents.embedding_preset.clone(),
        size_bytes,
        mtime,
    };
    match extract_and_persist_doc(
        store,
        rel,
        &abs,
        &hash,
        &mime_type,
        &config.documents,
        &config.llm,
        &config.resources,
        scope,
        embed,
    ) {
        Ok(Some(batch)) => {
            let status = FileStatus::DocIndexed {
                chunk_count: batch.chunk_count,
                embedding_dim: batch.embedding_dim,
            };
            let doc_upsert = match embed {
                EmbedMode::Inline => Some(doc_entry),
                EmbedMode::Deferred => None,
            };
            FileResult {
                path: rel.to_string(),
                status,
                upsert: None,
                doc_batch: Some(batch),
                doc_upsert,
                #[cfg(feature = "code-search")]
                code_batch: None,
            }
        }
        Ok(None) => FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang),
        Err(error) => {
            let msg = format!("document extract: {error:#}");
            if is_unsupported_format_error(&msg) {
                tracing::debug!(path = rel, reason = %msg, "skipping file: not an extractable document");
                FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang)
            } else {
                tracing::debug!(path = rel, error = %msg, "document extraction failed");
                FileResult::bare(rel.to_string(), FileStatus::ExtractFailed { msg })
            }
        }
    }
}

/// True when a document-extraction error means the file's format simply has no xberg
/// extractor (xberg's `UnsupportedFormat` → "Unsupported format: <mime>"), as opposed to a
/// genuine extraction failure on a real document. Such files are skipped, not failed.
#[cfg(feature = "documents")]
fn is_unsupported_format_error(msg: &str) -> bool {
    msg.to_ascii_lowercase().contains("unsupported format")
}

/// File mtime as nanoseconds since the Unix epoch (0 when unavailable). Nanosecond resolution — not
/// seconds — so the mtime+size fast-path in `process_file` is effectively race-free: a same-size edit
/// would have to reproduce the exact nanosecond mtime to be missed. The value is only ever compared
/// against a previously-stored one (never displayed), so the unit is a pure internal detail. `i64`
/// nanos overflow in year 2262; saturate rather than wrap.
fn mtime_nanos(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn read_working_tree(root: &Path, rel: &str, filters: &Filters) -> Result<(Vec<u8>, u64, i64), FileStatus> {
    let abs = root.join(rel);
    let metadata = std::fs::metadata(&abs).map_err(|e| FileStatus::ReadFailed {
        kind: e.kind(),
        msg: e.to_string(),
    })?;
    if metadata.len() > filters.max_file_bytes {
        return Err(FileStatus::SkippedTooLarge { size: metadata.len() });
    }
    let bytes = std::fs::read(&abs).map_err(|e| FileStatus::ReadFailed {
        kind: e.kind(),
        msg: e.to_string(),
    })?;
    let mtime = mtime_nanos(&metadata);
    let size = metadata.len();
    Ok((bytes, size, mtime))
}

fn read_via_git(filters: &Filters, blob: Result<Option<Vec<u8>>, GitError>) -> Result<(Vec<u8>, u64, i64), FileStatus> {
    let blob = blob.map_err(|e| FileStatus::ReadFailed {
        kind: std::io::ErrorKind::Other,
        msg: e.to_string(),
    })?;
    let bytes = blob.ok_or(FileStatus::ReadFailed {
        kind: std::io::ErrorKind::NotFound,
        msg: "blob not present in this git source".to_string(),
    })?;
    if bytes.len() as u64 > filters.max_file_bytes {
        return Err(FileStatus::SkippedTooLarge {
            size: bytes.len() as u64,
        });
    }
    let size = bytes.len() as u64;
    Ok((bytes, size, 0))
}

fn format_extract_err(e: &ExtractError) -> String {
    e.to_string()
}

/// First-byte heuristic for "definitely not source code": a NUL byte in the first 8 KiB.
/// PNG, ELF, Mach-O, .so/.dylib, .wasm, compiled .pyc/.class, and most archive formats hit
/// this within the first 16 bytes. Source code never contains a NUL byte legitimately. The
/// scan is bounded so we never traverse a multi-megabyte binary just to classify it.
pub fn looks_binary(bytes: &[u8]) -> bool {
    let probe = &bytes[..bytes.len().min(8 * 1024)];
    memchr::memchr(0, probe).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner_lanes::run_optional_lane;

    #[cfg(feature = "documents")]
    #[test]
    fn unsupported_format_error_is_a_skip_not_a_failure() {
        assert!(is_unsupported_format_error(
            "document extract: Unsupported format: application/x-wais-source"
        ));
        assert!(is_unsupported_format_error("Unsupported Format: text/x-foo"));
        assert!(!is_unsupported_format_error(
            "document extract: failed to parse PDF: corrupt xref table"
        ));
        assert!(!is_unsupported_format_error(
            "document extract: OCR engine returned no text"
        ));
    }

    /// The containment fix rests on rayon re-raising a worker's panic on the thread that called
    /// `ThreadPool::install`. If that ever stopped holding, `run_optional_lane` would be catching
    /// nothing and a panicking resolve pass would abort the scan again — so pin it directly.
    #[test]
    fn rayon_install_reraises_a_worker_panic_on_the_calling_thread() {
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scanner_pool(0).install(|| {
                (0..64u32).into_par_iter().for_each(|i| {
                    assert_ne!(i, 17, "worker panic");
                });
            });
        }));
        assert!(caught.is_err(), "a panic on a rayon worker must unwind into `install`");
    }

    /// The lane guard is what turns that re-raised panic into a logged degradation: the body runs,
    /// it panics on a worker, and control still returns to the caller.
    #[test]
    fn run_optional_lane_contains_a_panicking_rayon_lane() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let entered = std::sync::Arc::new(AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&entered);
        run_optional_lane("test_lane", move || {
            flag.store(true, Ordering::SeqCst);
            scanner_pool(0).install(|| {
                (0..64u32).into_par_iter().for_each(|i| {
                    assert_ne!(i, 17, "worker panic");
                });
            });
            unreachable!("the rayon lane above must have panicked");
        });
        assert!(entered.load(Ordering::SeqCst), "the lane body must have run");
    }

    #[test]
    fn looks_binary_detects_nul_in_first_kib() {
        let mut data = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        data.extend_from_slice(&[0; 32]);
        assert!(looks_binary(&data));
    }

    #[test]
    fn looks_binary_accepts_plain_source() {
        assert!(!looks_binary(b"pub fn hello() {}\n"));
        assert!(!looks_binary(b""));
    }

    #[test]
    fn looks_binary_ignores_nul_past_probe_window() {
        let mut data = vec![b'/'; 8 * 1024];
        data.push(0);
        assert!(!looks_binary(&data));
    }
}
