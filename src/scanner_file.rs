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
use crate::index::IndexDb;
use crate::path::RelPath;
use crate::scanner::{EmbedMode, FileResult, FileStatus, ScanCancel, ScanSource};
use crate::scanner_filter::Filters;
use crate::scanner_index_batch::WorkerIndexBatch;
use crate::store::{FileEntry, Store};

/// Per-worker stack size for the scanner's rayon pool. Tree-sitter parse trees and the recursive
/// msgpack (de)serialization of large extraction blobs can recurse deeper than rayon's small default
/// worker stack (~2 MiB), overflowing it on pathological inputs (deeply-nested or machine-generated
/// files) — observed as a hard `stack overflow` abort on a full fresh scan of a large real repo. A
/// full scan of that repo was measured to complete under a 128 MiB stack, so 256 MiB carries a 2×
/// margin. The stack is reserved lazily (VA only, not committed until touched), so this large ceiling
/// costs nothing on the common shallow path.
const SCANNER_STACK_SIZE: usize = 256 * 1024 * 1024;

/// Source size at or above which a fresh extraction goes through the memory gate. Below it the
/// transient is small enough that sampling the gate would cost more than it saves; above it a
/// handful of concurrent workers can move the process's footprint by hundreds of megabytes.
const LARGE_FILE_ADMIT_BYTES: usize = 1024 * 1024;

/// A file of this name panics `process_file` in test builds, standing in for a pathological input.
#[cfg(test)]
pub(crate) const TEST_PANIC_FILE: &str = "basemind-test-panic.rs";

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
///
/// `max_workers` caps how many of the pool's threads this call may spread across; `0` leaves
/// rayon's own splitting alone, which is what the unthrottled path wants (finer splits balance a
/// batch whose files differ by orders of magnitude in parse cost). A non-zero value is the drive
/// loop's actuating response to sustained memory pressure — see [`crate::scanner_drive`] — and is
/// applied as a minimum split length, so rayon cannot subdivide the chunk past that many jobs. The
/// rayon pool itself is process-global and fixed at first use; this narrows a single call, not it.
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
    cancel: &ScanCancel,
    max_workers: usize,
) -> Vec<FileResult> {
    let min_len = if max_workers == 0 {
        1
    } else {
        candidates.len().div_ceil(max_workers).max(1)
    };
    scanner_pool(config.resources.scan_threads).install(|| {
        candidates
            .par_iter()
            .with_min_len(min_len)
            .fold(
                || WorkerIndexBatch::new(store),
                |mut batch, rel| {
                    // Cooperative cancellation, per-file granularity: one relaxed load before each ~keep
                    // candidate. A skipped file emits no FileResult, so the caller can tell a ~keep
                    // partial pass apart from a complete one (and must skip its stale purge). ~keep
                    // A fatal index commit failure stops the walk the same way: see `scan_drive`. ~keep
                    if cancel.is_cancelled() || store.index_db.as_ref().is_some_and(IndexDb::has_fatal_fault) {
                        return batch;
                    }
                    // One file whose extraction panics is reported failed and keeps its prior rows ~keep
                    // (`keeps_prior_entry`); unwinding would fail this scan and every later one. ~keep
                    let result = crate::scanner_lanes::contain_panic(|| {
                        process_file(root, rel, filters, store, source, config, scope, &mut batch, embed)
                    })
                    .unwrap_or_else(|reason| {
                        tracing::warn!(rel, %reason, "extraction panicked; the file is reported failed");
                        FileResult::bare(
                            rel.clone(),
                            FileStatus::ExtractFailed {
                                msg: format!("extraction panicked: {reason}"),
                            },
                        )
                    });
                    batch.push_result(result);
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
fn extraction_sidecars_present(store: &Store, config: &Config, filters: &Filters, hash_hex: &str) -> bool {
    // After an unclean shutdown a blob the fsynced file map names may be torn or empty, and a file
    // skipped over it as unchanged would stay unreadable for good. ~keep
    let filemap_exists = if filters.verify_blobs {
        store.filemap_blob_is_intact(hash_hex)
    } else {
        store.blob_path_fm_hex(hash_hex).exists()
    };
    #[cfg(feature = "code-search")]
    {
        filemap_exists && (!crate::scanner_code::should_chunk(config) || store.blob_path_chunk_hex(hash_hex).exists())
    }
    #[cfg(not(feature = "code-search"))]
    {
        let _ = config;
        filemap_exists
    }
}

/// True when the on-disk chunk sidecar already satisfies THIS scan's requirements, so an otherwise
/// unchanged file may be short-circuited as `Unchanged`.
///
/// This is distinct from [`extraction_sidecars_present`] (blob *presence*, which governs l1/l2/chunk
/// reuse). The sidecar must have been chunked under the current `[code_search]` settings (its
/// `config_digest`; a legacy digest-less sidecar is re-checked once so it can be stamped). Then, a
/// `Deferred` pass never embeds, so any such sidecar satisfies it; an `Inline` pass over an
/// embed-eligible file requires embeddings for the current preset to already exist, else the file
/// must be re-processed to fill them, and a changed embed policy re-processes it so its vector rows
/// are rebuilt. The daemon writes chunk-only sidecars in `Deferred`, which a later `Inline` pass
/// upgrades in place — without this gate the Inline pass would short-circuit past `chunk_and_embed`
/// and never embed.
#[cfg(feature = "code-search")]
fn embed_state_satisfied(
    store: &Store,
    config: &Config,
    filters: &Filters,
    rel: &str,
    hash_hex: &str,
    mode: EmbedMode,
) -> bool {
    if !crate::scanner_code::should_chunk(config) {
        return true;
    }
    let Ok(Some(peek)) = store.peek_chunk_state(hash_hex) else {
        return false;
    };
    if peek.config_digest != filters.code_digest {
        return false;
    }
    // A changed embed policy may have purged this file's keyword postings along with its vectors
    // (`[code_search] enabled = false`), and the cached sidecar alone would leave them gone when the
    // tier is switched back on. Send every file back through the cached-chunk path to re-stage them,
    // whatever its own embed eligibility.
    if filters.reflush_embeds {
        return false;
    }
    let cfg = &config.code_search;
    if !matches!(mode, EmbedMode::Inline) || !cfg.embed || !filters.code_embed_allowed(rel) {
        return true;
    }
    peek.chunks.is_empty()
        || (peek.embedding_dim > 0
            && peek.embedding_model == config.documents.embedding_preset
            && peek.embeddings.len() == peek.chunks.len())
}

/// Without `code-search` there is no embedding tier, so every scan's embed requirement is trivially
/// satisfied.
#[cfg(not(feature = "code-search"))]
fn embed_state_satisfied(
    _store: &Store,
    _config: &Config,
    _filters: &Filters,
    _rel: &str,
    _hash_hex: &str,
    _mode: EmbedMode,
) -> bool {
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
    #[cfg(test)]
    assert!(!rel.ends_with(TEST_PANIC_FILE), "injected extraction panic for {rel}");
    // Only code is code-mapped. Prose, data and config files belong to the document tier (chunked,
    // searchable text); a markdown "outline" or a JSON file's keys are noise in the symbol index.
    // When the documents tier is compiled out there is nowhere else for them to go, so they keep
    // their grammar.
    #[cfg(feature = "documents")]
    let detected = filters.detect_lang(rel).lang().filter(|l| !crate::lang::is_non_code(l));
    #[cfg(not(feature = "documents"))]
    let detected = filters.detect_lang(rel).lang();
    let lang = match detected {
        Some(l) => l,
        None => {
            #[cfg(feature = "documents")]
            {
                if matches!(source, ScanSource::WorkingTree) {
                    return filters
                        .retrieval
                        .documents
                        .process(root, rel, filters, store, config, scope, embed);
                }
            }
            return FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang);
        }
    };

    if matches!(source, ScanSource::WorkingTree)
        && let Some(existing) = store.lookup(rel)
        && existing.mtime != 0
        && existing.language == lang
        && existing.extract_epoch >= extract::EXTRACT_EPOCH
        && let Ok(meta) = std::fs::metadata(root.join(rel))
    {
        // The cap outranks the unchanged shortcut: lowering `max_file_bytes` must evict an already
        // indexed file that is now too large, not keep reporting it `Unchanged`.
        if meta.len() > filters.max_file_bytes {
            return FileResult::bare(rel.to_string(), FileStatus::SkippedTooLarge { size: meta.len() });
        }
        let mtime = mtime_nanos(&meta);
        if meta.len() == existing.size_bytes
            && mtime == existing.mtime
            && extraction_sidecars_present(store, config, filters, &existing.hash_hex)
            && embed_state_satisfied(store, config, filters, rel, &existing.hash_hex, embed)
        {
            // Bytes were not read, so a missing grep bloom (an index that predates it) is backfilled
            // here rather than leaving the file a permanent grep candidate.
            index_batch.backfill_grep_bloom(root, &RelPath::from(rel), &existing.hash_hex);
            return FileResult::bare(rel.to_string(), FileStatus::Unchanged);
        }
    }

    let (bytes, size_bytes, mtime) = match source {
        ScanSource::WorkingTree => {
            match read_working_tree(root, rel, filters.max_file_bytes, config.scan.follow_symlinks) {
                Ok(triple) => triple,
                Err(status) => {
                    return FileResult::bare(rel.to_string(), status);
                }
            }
        }
        ScanSource::Staged(repo) => match read_via_git(filters.max_file_bytes, repo.read_blob_staged(rel)) {
            Ok(triple) => triple,
            Err(status) => {
                return FileResult::bare(rel.to_string(), status);
            }
        },
        ScanSource::Rev { repo, sha } => match read_via_git(filters.max_file_bytes, repo.read_blob_at_rev(sha, rel)) {
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

    // A stamp taken too close to the clock cannot vouch for the bytes just read (see `settled_mtime`).
    let mtime = if matches!(source, ScanSource::WorkingTree) {
        settled_mtime(mtime, now_nanos())
    } else {
        mtime
    };

    let hash = hashing::hash_bytes(&bytes);
    let hex_buf = hashing::hex_buf(&hash);
    let hash_hex_str = hashing::hex_str(&hex_buf);

    let sidecars_present = extraction_sidecars_present(store, config, filters, hash_hex_str);
    if let Some(existing) = store.lookup(rel)
        && existing.hash_hex == hash_hex_str
        && existing.language == lang
        && existing.extract_epoch >= extract::EXTRACT_EPOCH
        && sidecars_present
        && embed_state_satisfied(store, config, filters, rel, hash_hex_str, embed)
    {
        // Content is unchanged but the stamp moved (touch, checkout): refresh the entry's stamp, which
        // grep holds the live file to, and make sure the content's bloom row exists.
        let mut result = FileResult::bare(rel.to_string(), FileStatus::Unchanged);
        if matches!(source, ScanSource::WorkingTree) {
            index_batch.restamp_grep_bloom(hash_hex_str, &bytes);
            if (existing.size_bytes, existing.mtime) != (size_bytes, mtime) {
                result.upsert = Some(FileEntry {
                    size_bytes,
                    mtime,
                    ..existing.clone()
                });
            }
        }
        return result;
    }

    let want_l2 = filters.eager_l2 && store.index_db.is_some();

    let mut foreign_lang = false;
    // Set when a schema-current blob of the right language predates `EXTRACT_EPOCH`: it is rewritten
    // in place. The epoch covers L2 calls too, so a blob that held L2 gets it re-extracted rather
    // than carried over, even when this scan would not extract L2 on its own.
    let mut stale_epoch = false;
    let mut stale_had_l2 = false;
    let reused_pair: Option<(FileMapL1, Option<FileMapL2>)> = if sidecars_present {
        match store.read_l1_by_hex(hash_hex_str) {
            Ok(Some(l1)) if l1.language == lang && l1.extract_epoch < extract::EXTRACT_EPOCH => {
                stale_epoch = true;
                stale_had_l2 = matches!(store.read_l2_by_hex(hash_hex_str), Ok(Some(_)));
                None
            }
            Ok(Some(l1)) if l1.language == lang => {
                let l2 = if want_l2 {
                    store.read_l2_by_hex(hash_hex_str).unwrap_or(None)
                } else {
                    None
                };
                if want_l2 && l2.is_none() { None } else { Some((l1, l2)) }
            }
            Ok(Some(_)) => {
                foreign_lang = true;
                None
            }
            _ => None,
        }
    } else {
        None
    };
    let reused = reused_pair.is_some();

    let (l1, l2_opt): (FileMapL1, Option<FileMapL2>) = match reused_pair {
        Some(pair) => pair,
        None => {
            // Advisory admit point (see `crate::scanner_drive` for the advisory/actuating split): ~keep
            // a fresh parse of a large file allocates a tree-sitter tree plus two extraction maps ~keep
            // several times the source size, and every pool worker can be doing it at once. There ~keep
            // is no smaller unit to fall back to here, so after `max_wait` the gate admits anyway. ~keep
            // Placed BEFORE `index_batch.stage`: parking with a Fjall batch open would turn a ~keep
            // memory problem into a liveness one. ~keep
            if bytes.len() >= LARGE_FILE_ADMIT_BYTES {
                crate::backpressure::FootprintGate::new(config.resources.max_footprint_mb).admit();
            }
            match extract::extract_l1_l2(lang, &bytes, want_l2 || stale_had_l2) {
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
            }
        }
    };

    let l2: Option<FileMapL2> = l2_opt;
    if !reused {
        let written = if foreign_lang || stale_epoch {
            store.overwrite_filemap_hex(hash_hex_str, &l1, l2.as_ref())
        } else {
            store.write_filemap_hex(hash_hex_str, &l1, l2.as_ref())
        };
        if let Err(e) = written {
            return FileResult::bare(rel.to_string(), FileStatus::ExtractFailed { msg: e.to_string() });
        }
    }

    let rel_path = RelPath::from(rel);
    if !index_batch.stage(&rel_path, &l1, l2.as_ref()) {
        tracing::warn!(rel, "index upsert failed; reference search may be incomplete");
    }
    // Grep reads the working tree, so only a working-tree scan describes what it will sweep.
    if matches!(source, ScanSource::WorkingTree) {
        index_batch.stage_grep_bloom(hash_hex_str, &bytes);
    }

    #[cfg(feature = "code-search")]
    let code_batch = if crate::scanner_code::should_chunk(config) {
        match crate::scanner_code::chunk_and_embed(
            store,
            rel,
            &bytes,
            &l1,
            l2.as_ref(),
            hash_hex_str,
            config,
            filters,
            embed,
            stale_epoch,
        ) {
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
        extract_epoch: crate::extract::EXTRACT_EPOCH,
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
        #[cfg(feature = "documents")]
        doc_tombstone: None,
        #[cfg(feature = "code-search")]
        code_batch,
    }
}

/// File mtime as nanoseconds since the Unix epoch (0 when unavailable). Nanosecond resolution — not
/// seconds — so the mtime+size fast-path in `process_file` is effectively race-free: a same-size edit
/// would have to reproduce the exact nanosecond mtime to be missed. The value is only ever compared
/// against a previously-stored one (never displayed), so the unit is a pure internal detail. `i64`
/// nanos overflow in year 2262; saturate rather than wrap.
pub fn mtime_nanos(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// How close to the clock a file's mtime may be before the filesystem's timestamp granularity can
/// no longer tell a later edit from the content just read.
const RACY_WINDOW_NANOS: u64 = 2_000_000_000;

/// The wall clock as nanoseconds since the Unix epoch, saturating like [`mtime_nanos`].
fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// The mtime to record for bytes read when the clock said `now_ns`: `mtime_ns` itself when it is far
/// enough from `now_ns`, else 0. An edit landing in the same timestamp tick as the read could keep
/// both size and mtime, so the grep bloom guard (which holds a live file to its indexed
/// `(size, mtime)`) must not trust such a stamp; 0 is the "unknown" stamp it never trusts. The next
/// scan re-reads the file, finds the same hash and restamps it once it has settled.
pub(crate) fn settled_mtime(mtime_ns: i64, now_ns: i64) -> i64 {
    if mtime_ns.abs_diff(now_ns) < RACY_WINDOW_NANOS {
        0
    } else {
        mtime_ns
    }
}

pub(crate) fn read_working_tree(
    root: &Path,
    rel: &str,
    max_file_bytes: u64,
    follow_symlinks: bool,
) -> Result<(Vec<u8>, u64, i64), FileStatus> {
    use std::io::Read;

    let read_failed = |e: std::io::Error| FileStatus::ReadFailed {
        kind: e.kind(),
        msg: e.to_string(),
    };
    let not_regular = |msg: &str| FileStatus::ReadFailed {
        kind: std::io::ErrorKind::InvalidInput,
        msg: msg.to_string(),
    };
    let abs = root.join(rel);
    // A full scan never yields a symlink unless `follow_symlinks` is on, but the incremental path
    // (watcher, `rescan paths=[..]`) hands over arbitrary paths: refuse links that could pull a file
    // from outside the repo (`notes.md -> ~/.ssh/id_rsa`) or an endless device (`x.rs -> /dev/zero`).
    if !follow_symlinks {
        let link = std::fs::symlink_metadata(&abs).map_err(read_failed)?;
        if !link.is_file() {
            return Err(not_regular("not a regular file (symlinks are not followed)"));
        }
        if !rel.starts_with('/') {
            let inside = match (abs.canonicalize(), root.canonicalize()) {
                (Ok(abs), Ok(root)) => abs.starts_with(root),
                _ => false,
            };
            if !inside {
                return Err(not_regular("resolves outside the workspace root"));
            }
        }
    }
    let file = std::fs::File::open(&abs).map_err(read_failed)?;
    // `fstat` on the open handle, so the size checked is the file actually read (no metadata-then-read
    // race) and a device like `/dev/zero` (reported length 0) is refused rather than slurped.
    let metadata = file.metadata().map_err(read_failed)?;
    if !metadata.is_file() {
        return Err(not_regular("not a regular file"));
    }
    if metadata.len() > max_file_bytes {
        return Err(FileStatus::SkippedTooLarge { size: metadata.len() });
    }
    // Capped read: a file that grows after the stat still cannot exceed the limit by more than a byte.
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(max_file_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(read_failed)?;
    if bytes.len() as u64 > max_file_bytes {
        return Err(FileStatus::SkippedTooLarge {
            size: bytes.len() as u64,
        });
    }
    let mtime = mtime_nanos(&metadata);
    let size = metadata.len();
    Ok((bytes, size, mtime))
}

fn read_via_git(
    max_file_bytes: u64,
    blob: Result<Option<Vec<u8>>, GitError>,
) -> Result<(Vec<u8>, u64, i64), FileStatus> {
    let blob = blob.map_err(|e| FileStatus::ReadFailed {
        kind: std::io::ErrorKind::Other,
        msg: e.to_string(),
    })?;
    let bytes = blob.ok_or(FileStatus::ReadFailed {
        kind: std::io::ErrorKind::NotFound,
        msg: "blob not present in this git source".to_string(),
    })?;
    if bytes.len() as u64 > max_file_bytes {
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

    #[cfg(unix)]
    #[test]
    fn read_working_tree_refuses_devices_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        // `join` with an absolute `rel` yields `rel`, which lets the test aim at a device node.
        let zero = read_working_tree(dir.path(), "/dev/zero", 1024, false).unwrap_err();
        assert!(matches!(zero, FileStatus::ReadFailed { .. }), "got {zero:?}");
        let sub = read_working_tree(
            dir.path().parent().unwrap(),
            dir.path().file_name().unwrap().to_str().unwrap(),
            1024,
            false,
        );
        assert!(matches!(sub, Err(FileStatus::ReadFailed { .. })));
    }

    #[test]
    fn read_working_tree_enforces_the_size_cap_on_the_read_itself() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"0123456789").unwrap();
        assert!(matches!(
            read_working_tree(dir.path(), "f.txt", 9, false),
            Err(FileStatus::SkippedTooLarge { size: 10 })
        ));
        let (bytes, size, _) = read_working_tree(dir.path(), "f.txt", 10, false).unwrap();
        assert_eq!((bytes.len(), size), (10, 10));
    }

    #[cfg(unix)]
    #[test]
    fn working_tree_reads_refuse_symlinks_and_links_out_of_the_root() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path();
        std::fs::write(outside.path().join("secret"), "AKIA-secret").expect("write");
        std::fs::write(root.join("real.rs"), "fn main() {}").expect("write");
        symlink(outside.path().join("secret"), root.join("notes.md")).expect("file link");
        symlink("/dev/zero", root.join("zero.rs")).expect("device link");
        symlink(outside.path(), root.join("linkdir")).expect("dir link");

        let read = |rel: &str, follow| read_working_tree(root, rel, 1024, follow);
        assert!(read("real.rs", false).is_ok());
        for rel in ["notes.md", "zero.rs", "linkdir/secret"] {
            assert!(
                matches!(read(rel, false), Err(FileStatus::ReadFailed { .. })),
                "{rel} must be refused"
            );
        }
        // An operator who granted `follow_symlinks` keeps the linked file, and a followed device
        // still cannot grow past the cap.
        assert!(read("notes.md", true).is_ok());
        assert!(read("zero.rs", true).is_err());
    }

    #[test]
    fn working_tree_reads_are_capped_by_size() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("big.txt"), vec![b'x'; 4096]).expect("write");
        assert!(matches!(
            read_working_tree(repo.path(), "big.txt", 1024, false),
            Err(FileStatus::SkippedTooLarge { .. })
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
