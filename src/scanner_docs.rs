//! Document-tier branch helpers for the scanner.
//!
//! Gated on `feature = "documents"`. Lives in its own file so the core
//! `src/scanner.rs` module stays under the 1000-line cap and so the
//! intelligence-only code path is easy to inspect in isolation.
//!
//! The flow that the scanner drives is:
//!
//! 1. `process_file` sees `lang::detect` return `None` (non-source file).
//! 2. `should_extract_document` decides whether the file qualifies for the
//!    document tier based on `[documents]` config + MIME allowlist.
//! 3. `extract_and_persist_doc` runs xberg, writes the msgpack blob, and
//!    returns a `PendingDocBatch` carrying only the lightweight metadata (path,
//!    blob hash, scope, counts) needed to rebuild rows later — never the
//!    embedding vectors themselves.
//! 4. The single-threaded apply pass calls `flush_document_batches`, which — per
//!    file — re-reads that file's already-persisted `.doc.msgpack` blob, builds
//!    its LanceDB rows, writes them, and drops them before moving on. Peak embed
//!    RAM is one file's rows, independent of corpus size (the streaming write
//!    that replaced the old "accumulate every file's rows, then write once" pass
//!    which held multiple GB resident on a large repo).

#![cfg(feature = "documents")]

use std::path::Path;
use std::sync::{Arc, OnceLock};

use ahash::AHashSet;
use anyhow::Context as _;
use xberg::core::mime;
use xberg::embeddings::{EMBEDDING_PRESETS, EmbeddingPreset};

use crate::config::{DocumentsConfig, ResourcesConfig};
use crate::extract::doc::{DocBlobs, DocConfig, FileMapDoc, extract_doc};
use crate::extract::doc_guard::ChunkCutovers;
use crate::hashing::{self, Hash};
use crate::scan_retrieval::PendingDocBatch;
use crate::scanner::EmbedMode;
use crate::scanner_filter::Filters;
use crate::store::Store;

/// Look the configured embedding preset up in xberg's preset table and
/// return its vector dimension as a `u16` (LanceDB's `FixedSizeList<Float32, N>`
/// uses `i32` but we treat the value as a `u16` everywhere because every
/// shipped preset is < 65 535 dims).
///
/// Returns an error rather than guessing when the preset name is unknown — a
/// silent fallback would create a LanceDB table with the wrong dim and force a
/// later wipe-and-rebuild.
pub(crate) fn preset_dim(name: &str) -> anyhow::Result<u16> {
    let preset: &EmbeddingPreset = EMBEDDING_PRESETS
        .iter()
        .find(|p| p.name == name)
        .with_context(|| format!("unknown xberg embedding preset: {name}"))?;
    u16::try_from(preset.dimensions)
        .with_context(|| format!("preset {name} dimensions {} exceeds u16", preset.dimensions))
}

/// Translate the project-level `[documents]` config into the xberg-facing
/// [`DocConfig`] the extractor expects. Pulled out so the wiring in
/// `process_file` stays a single call.
pub(crate) fn doc_config_from(cfg: &DocumentsConfig, resources: &ResourcesConfig, embed: bool) -> DocConfig {
    DocConfig {
        max_pages: cfg.max_pages,
        extraction_timeout_secs: cfg.extraction_timeout_secs,
        max_abandoned_extractions: cfg.max_abandoned_extractions,
        phase: None,
        max_chunks_per_document: cfg.max_chunks_per_document,
        chunk_cutovers: ChunkCutovers {
            markdown_bytes: cfg.markdown_chunk_max_bytes,
            plain_text_bytes: cfg.plain_text_chunk_max_bytes,
        },
        max_characters: cfg.max_characters,
        overlap: cfg.overlap,
        embedding_preset: Some(cfg.embedding_preset.clone()),
        embed,
        language: cfg.language.clone(),
        keywords: cfg.keywords.clone(),
        ner: cfg.ner.clone(),
        summarization: cfg.summarization.clone(),
        embed_max_threads: resources.effective_embed_threads(cfg.embed_max_threads),
        embed_batch_size: resources.embed_batch_size,
        document_models: resources.document_models,
        onnx_provider: resources.onnx_provider,
        max_archive_bytes: usize::try_from(cfg.max_archive_bytes).unwrap_or(usize::MAX),
        max_archive_entries: cfg.max_archive_entries,
    }
}

/// Archive / compressed-container extensions. ALWAYS rejected: xberg's archive extractor is not
/// compiled in (recursively unpacking and embedding every entry was the observed 1119%-CPU / 15 GB
/// footgun), and `documents.extract_archives` is accepted but inert. This extension floor is the
/// primary guard because `mime::detect_mime_type` is extension-based and several of these
/// (`.jar/.whl/.war/...`) collapse to `application/octet-stream` via the `mime_guess` fallback, so a
/// MIME-only denylist would miss them.
const ARCHIVE_EXTENSIONS: &[&str] = &[
    "zip", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "zst", "zstd", "7z", "rar", "lz", "lz4", "lzma", "br",
    "cab", "ar", "iso", "dmg", "jar", "war", "ear", "apk", "whl", "egg", "deb", "rpm", "nupkg", "pkg", "msi", "crate",
];

/// Native / compiled-binary extensions. ALWAYS rejected — these carry no extractable text.
const BINARY_EXTENSIONS: &[&str] = &[
    "so", "dylib", "dll", "a", "o", "obj", "bin", "exe", "wasm", "class", "pyc", "pyo", "pyd", "node", "pack", "idx",
];

/// Document formats temporarily rejected because xberg 1.0.14 routes them through biblib 0.4.3's
/// vulnerable quick-xml 0.37 parser (RUSTSEC-2026-0194). Remove with the matching MIME guard once
/// xberg #1398 ships its already-landed biblib 0.8 / quick-xml 0.41 dependency update.
const UNSAFE_DOCUMENT_EXTENSIONS: &[&str] = &["enw"];

/// MIME counterpart to [`UNSAFE_DOCUMENT_EXTENSIONS`]. Keeping both guards makes the rejection
/// explicit even if xberg's extension-based MIME detection learns another EndNote XML suffix.
const UNSAFE_DOCUMENT_MIME: &[&str] = &["application/x-endnote+xml"];

/// Archive MIME denylist (belt-and-suspenders with [`ARCHIVE_EXTENSIONS`]) — catches content-typed
/// archives xberg maps to a real archive MIME. Entries ending in `/` are prefix matches (see
/// [`matches_mime`]).
const ARCHIVE_MIME: &[&str] = &[
    "application/zip",
    "application/x-tar",
    "application/gzip",
    "application/x-7z-compressed",
    "application/java-archive",
    "application/vnd.rar",
];

/// Binary MIME denylist — the unambiguously non-extractable native-binary + audio/video/font
/// families. ALWAYS rejected. `image/` is deliberately NOT listed here: with the OCR tier compiled
/// in, xberg extracts text from images. Without it they are denied separately, see
/// [`is_denied_binary_or_archive`].
const BINARY_MIME: &[&str] = &[
    "application/wasm",
    "application/x-executable",
    "application/x-sharedlib",
    "application/x-mach-binary",
    "application/octet-stream",
    "audio/",
    "video/",
    "font/",
];

/// Lazily-initialized `AHashSet` view of `exts`, built once and reused across all scanner threads.
///
/// Replaces the O(n) `slice::contains` call in [`is_denied_binary_or_archive`] with an O(1) hash
/// lookup. The `OnceLock` ensures thread-safe initialisation.
fn ext_set(
    cell: &'static OnceLock<AHashSet<&'static str>>,
    exts: &'static [&'static str],
) -> &'static AHashSet<&'static str> {
    cell.get_or_init(|| exts.iter().copied().collect())
}

fn archive_ext_set() -> &'static AHashSet<&'static str> {
    static SET: OnceLock<AHashSet<&'static str>> = OnceLock::new();
    ext_set(&SET, ARCHIVE_EXTENSIONS)
}

fn binary_ext_set() -> &'static AHashSet<&'static str> {
    static SET: OnceLock<AHashSet<&'static str>> = OnceLock::new();
    ext_set(&SET, BINARY_EXTENSIONS)
}

/// True when a file must be skipped by the document tier because it is unsafe to parse, a binary
/// blob, an archive / compressed container, on the user-configured `extension_denylist`, or a raster
/// image unless `[documents.ocr] enabled` is on in a build with the OCR tier (it would be indexed
/// with metadata and no text).
fn is_denied_binary_or_archive(abs: &Path, mime_type: &str, cfg: &DocumentsConfig) -> bool {
    if let Some(ext) = abs.extension().and_then(|e| e.to_str()) {
        let ext_lower = ext.to_ascii_lowercase();
        if UNSAFE_DOCUMENT_EXTENSIONS.contains(&ext_lower.as_str())
            || binary_ext_set().contains(ext_lower.as_str())
            || cfg
                .extension_denylist
                .iter()
                .any(|e| crate::config::rules::normalize_extension(e) == ext_lower)
        {
            return true;
        }
        if archive_ext_set().contains(ext_lower.as_str()) {
            return true;
        }
    }
    if BINARY_MIME.iter().any(|entry| matches_mime(entry, mime_type)) {
        return true;
    }
    if UNSAFE_DOCUMENT_MIME.iter().any(|entry| matches_mime(entry, mime_type)) {
        return true;
    }
    if ARCHIVE_MIME.iter().any(|entry| matches_mime(entry, mime_type)) {
        return true;
    }
    !(cfg.ocr.enabled && crate::extract::doc_tiers::ocr_compiled()) && crate::extract::doc_tiers::is_ocr_only_mime(mime_type)
}

/// Quick filter run before any xberg work happens. Returns the MIME type when the file should be
/// document-extracted, or `None` when it should be skipped (configured-off, archive/binary, MIME
/// unknown, MIME outside the allowlist). The type is `given` when the caller knows it (a
/// dependency's extension-less `README`), else detected from the name.
///
/// The MIME allowlist is treated as "match this exact MIME OR a prefix ending
/// in `/`" so callers can say `"image/"` to whitelist every image type.
pub(crate) fn should_extract_document_as(abs: &Path, cfg: &DocumentsConfig, given: Option<&str>) -> Option<String> {
    if !cfg.enabled {
        return None;
    }
    let mime_type = match given {
        Some(mime_type) => mime_type.to_string(),
        None => mime::detect_mime_type(abs, false).ok()?,
    };
    if is_denied_binary_or_archive(abs, &mime_type, cfg) {
        return None;
    }
    if cfg.mime_allowlist.is_empty() {
        return Some(mime_type);
    }
    let allowed = cfg.mime_allowlist.iter().any(|entry| matches_mime(entry, &mime_type));
    if allowed { Some(mime_type) } else { None }
}

/// [`should_extract_document_as`] with the type detected from the name.
#[cfg(test)]
pub(crate) fn should_extract_document(abs: &Path, cfg: &DocumentsConfig) -> Option<String> {
    should_extract_document_as(abs, cfg, None)
}

fn matches_mime(entry: &str, mime_type: &str) -> bool {
    if entry == mime_type {
        return true;
    }
    if let Some(prefix) = entry.strip_suffix('/') {
        return mime_type.starts_with(prefix) && mime_type.as_bytes().get(prefix.len()) == Some(&b'/');
    }
    false
}

/// Run xberg against `abs`, write the document blob to the content-addressed
/// store, and assemble a [`PendingDocBatch`] for the apply pass. Returns
/// `Ok(None)` when extraction succeeded but produced no embeddings (we still
/// persist the blob; the LanceDB side is just a no-op for that file).
#[allow(clippy::too_many_arguments)]
pub(crate) fn extract_and_persist_doc(
    store: &Store,
    rel: &str,
    abs: &Path,
    hash: &Hash,
    read_stamp: (u64, i64),
    mime_type: &str,
    cfg: &DocumentsConfig,
    resources: &ResourcesConfig,
    filters: &Filters,
    mode: EmbedMode,
) -> Result<Option<PendingDocBatch>, anyhow::Error> {
    let embed = doc_embed_requested(rel, cfg, mode, filters);
    let hex_buf = hashing::hex_buf(hash);
    let hash_hex = hashing::hex_str(&hex_buf);

    if let Some(mut cached) = store.read_doc_by_hex(hash_hex).ok().flatten()
        && cached_doc_is_reusable(&cached, cfg, embed, &filters.doc_digest)
    {
        if cached.config_digest.is_empty() {
            cached.config_digest.clone_from(&filters.doc_digest);
            store
                .write_doc(hash, &cached)
                .with_context(|| format!("stamp doc blob for {rel}"))?;
        }
        // Keep this reused blob "young" so the blob GC's grace window protects it through a NoCache
        // rename's transient entry-less gap (issue #44) instead of reaping it and forcing a re-embed.
        store.touch_doc_blob(hash_hex);
        return Ok(Some(pending_from_doc(&cached, rel, cfg, embed, true)));
    }

    let mut doc_config = doc_config_from(cfg, resources, embed);
    doc_config.phase = Some(Arc::clone(&filters.doc_phase));
    let mut doc: FileMapDoc =
        extract_doc(abs, Some(mime_type), &doc_config).with_context(|| format!("extract document {rel}"))?;
    // xberg read the file by path after `hash` was taken from an earlier read; a blob stored under
    // that hash must describe those bytes, or a later revert to them would reuse the wrong text.
    if file_stamp(abs) != Some(read_stamp) {
        anyhow::bail!("{rel} changed while it was being extracted; the next scan extracts it again");
    }
    doc.config_digest.clone_from(&filters.doc_digest);
    store
        .write_doc(hash, &doc)
        .with_context(|| format!("write doc blob for {rel}"))?;

    Ok(Some(pending_from_doc(&doc, rel, cfg, embed, false)))
}

/// `(size, mtime)` of the file at `abs` now, as `read_working_tree` reports them for a read.
pub(crate) fn file_stamp(abs: &Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(abs).ok()?;
    Some((meta.len(), crate::scanner_file::mtime_nanos(&meta)))
}

/// Single source of truth for "will this pass embed this doc": embedding runs only on an `Inline`
/// pass, with `[documents] embed` on, for a path not excluded by `embed_exclude`. Shared by
/// `extract_and_persist_doc` (whether to ask xberg for vectors) and the `process_doc` unchanged
/// fast path (whether a tracked-but-unembedded doc must be re-processed) so the two sides can
/// never disagree about the requirement — a disagreement is exactly the issue-#44 loop.
pub(crate) fn doc_embed_requested(rel: &str, cfg: &DocumentsConfig, mode: EmbedMode, filters: &Filters) -> bool {
    matches!(mode, EmbedMode::Inline) && cfg.embed && filters.doc_embed_allowed(rel)
}

/// The pure (IO-free) half of the `process_doc` unchanged fast path: does a tracked entry settle the
/// current pass without re-processing? True when the content hash and preset both match AND either
/// the doc is embedded, a prior pass already attempted embedding (issue #44 follow-up — a failed
/// attempt must not be re-extracted + retried every scan), or this pass does not ask to embed. The
/// caller ANDs this with a blob-existence probe (the blob may have been GC'd out from under a
/// still-tracked entry).
///
/// The entry must also carry the current `config_digest` (so a changed chunk / extraction knob
/// re-processes the doc), and `reflush` (the embed policy changed since the last scan) sends every
/// doc that wants vectors back through the pipeline so its rows are rebuilt from the cached blob.
pub(crate) fn doc_entry_settled(
    existing: &crate::store::DocEntry,
    hash_hex: &str,
    embedding_preset: &str,
    embed_requested: bool,
    config_digest: &str,
    reflush: bool,
) -> bool {
    existing.hash_hex == hash_hex
        && existing.embedding_preset == embedding_preset
        && existing.config_digest == config_digest
        && !(embed_requested && reflush)
        && (existing.embedded || existing.embed_attempted || !embed_requested)
}

/// True when a cached document blob can be reused without re-extraction. When embedding is on the
/// cached blob must carry embeddings produced by the current preset — matching both its **dimension**
/// AND its **model**. The model check is load-bearing: `balanced` and `multilingual` share dim 768,
/// so a dim-only gate would falsely reuse stale-model vectors when switching between them. A preset
/// change (dim OR model) therefore forces recompute — same gate as `chunk_and_embed`. An
/// empty-of-chunks doc, or one over `max_chunks_per_document`, is always reusable (recompute would
/// embed nothing anyway). When embedding is
/// off, any cached doc is reusable (chunks only). In every case the blob must have been produced
/// under the current `config_digest` (or predate digests, in which case the caller stamps it).
fn cached_doc_is_reusable(cached: &FileMapDoc, cfg: &DocumentsConfig, embed: bool, config_digest: &str) -> bool {
    if !cached.config_digest.is_empty() && cached.config_digest != config_digest {
        return false;
    }
    if !embed || cached.chunks.is_empty() || cached.chunks.len() > cfg.max_chunks_per_document {
        return true;
    }
    doc_has_vectors_of(cached, &cfg.embedding_preset)
}

/// Whether every chunk of `doc` carries a vector of `preset`: the model's name and its dimension.
pub(crate) fn doc_has_vectors_of(doc: &FileMapDoc, preset: &str) -> bool {
    doc.embedding_dim > 0
        && Some(doc.embedding_dim) == preset_dim(preset).ok()
        && doc.embedding_model == crate::embeddings::model_key(preset)
        && doc
            .chunks
            .iter()
            .all(|c| c.embedding.len() == doc.embedding_dim as usize)
}

/// Assemble the deferred-write descriptor from an extracted-or-cached document. Decides — while the
/// document is in hand — whether this file will emit LanceDB rows at flush time: only when embedding
/// is on, the doc has embeddings, it has chunks, and it is under the per-doc chunk cap. The heavy
/// row-building (chunk text + embedding vectors) is deferred to [`flush_document_batches`], which
/// re-reads the persisted blob per file so no embeddings are held across the corpus.
pub(crate) fn pending_from_doc(
    doc: &FileMapDoc,
    rel: &str,
    cfg: &DocumentsConfig,
    embed: bool,
    reused: bool,
) -> PendingDocBatch {
    let chunk_count = doc.chunks.len();
    let embedding_dim = doc.embedding_dim;
    let over_cap = chunk_count > cfg.max_chunks_per_document;
    if over_cap {
        tracing::warn!(
            rel,
            chunks = chunk_count,
            cap = cfg.max_chunks_per_document,
            "document exceeds max_chunks_per_document; indexed without vectors"
        );
    }
    let emit_rows = embed && embedding_dim > 0 && chunk_count > 0 && !over_cap;
    PendingDocBatch {
        rel_path: rel.to_string(),
        chunk_count,
        embedding_dim,
        emit_rows,
        embedded: !embed || embedding_dim > 0 || chunk_count == 0,
        // Only a fresh extraction (`!reused`) ran an attempt; a reused blob already carries vectors ~keep
        // (the reuse gate requires them), so `embedded` covers it and the flag is moot there. A ~keep
        // failed attempt is not one: the document owes its vectors until the embedder works. ~keep
        // A document over the cap is never embedded, so it owes no attempt either. ~keep
        embed_attempted: embed && (over_cap || (!reused && !doc.embed_failed)),
        reused,
        degraded: crate::extract::doc_guard::is_degraded_chunking(
            &doc.mime_type,
            doc.linear_chunked,
            doc.text_fallback,
        ),
        text_fallback: doc.text_fallback,
        over_chunk_cap: over_cap,
    }
}

#[path = "scanner_docs_store.rs"]
mod doc_rows;
#[cfg(test)]
pub(crate) use doc_rows::open_doc_store;

/// Drop the `index.doc_files` entries of documents that no longer exist (or are no longer routed
/// to the doc tier), then the store rows no view of the group names. Serial apply pass. The
/// tracking entry is dropped unconditionally; the row delete is best-effort and never creates the
/// store.
pub(crate) fn delete_stale_documents(store: &mut Store, config: &crate::config::Config, stale: &[String]) {
    if stale.is_empty() {
        return;
    }
    for path in stale {
        store.remove_doc(path);
        store.remove_doc_tombstone(path);
    }
    doc_rows::remove_dead_document_rows(store, config);
}

/// Rewrite without vectors the rows of every tracked doc that is no longer embed-eligible
/// (`[documents] embed = false`, or every path of it now falls outside `embed_include` / inside
/// `embed_exclude`). The docs stay indexed and searchable by the lexical lane; only their vectors
/// go. Run when the embed policy changed since the last scan; see [`crate::scanner_policy`].
/// Returns whether the purge fully applied (no store counts as applied).
pub(crate) fn purge_unembedded_documents(store: &mut Store, config: &crate::config::Config, filters: &Filters) -> bool {
    doc_rows::strip_unwanted_vectors(store, config, filters)
}

/// Write the rows the store lacks for this scan's documents and every other live document (a
/// wiped store is rebuilt here from the cached blobs), delete the rows of documents the view no
/// longer names, and update the indexes. The batches only say that the scan touched documents:
/// what is written is decided against the store. Returns the number of documents written.
pub(crate) fn flush_document_batches(
    store: &mut Store,
    config: &crate::config::Config,
    filters: &Filters,
    batches: Vec<PendingDocBatch>,
    budget: usize,
) -> usize {
    let scanned: ahash::AHashSet<String> = batches.into_iter().map(|batch| batch.rel_path).collect();
    doc_rows::sync_document_rows(store, config, filters, &scanned, budget)
}

#[cfg(test)]
#[path = "scanner_docs_security_tests.rs"]
mod security_tests;

#[cfg(test)]
#[path = "scanner_docs_tests.rs"]
mod tests;
