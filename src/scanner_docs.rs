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
use std::sync::OnceLock;

use ahash::AHashSet;
use anyhow::Context as _;
use xberg::core::mime;
use xberg::embeddings::{EMBEDDING_PRESETS, EmbeddingPreset};

use crate::config::{DocumentsConfig, LlmConfig};
use crate::extract::doc::{DocConfig, FileMapDoc, extract_doc};
use crate::hashing::{self, Hash};
use crate::lance::DocumentRow;
use crate::scanner::EmbedMode;
use crate::store::Store;

/// Per-file deferred-LanceDB-write descriptor. Constructed inside `process_file`'s parallel
/// worker; consumed in the single-threaded apply pass via [`flush_document_batches`].
///
/// Deliberately **metadata only** — it carries the content hash needed to re-read the persisted
/// `.doc.msgpack` blob at flush time, NOT the chunk text or the embedding vectors. Accumulating the
/// vectors here (one `Vec<f32>` per chunk, hundreds of chunks per document) across an entire corpus
/// was the memory leak this shape fixes: the flush now rebuilds one file's rows at a time.
#[derive(Debug, Clone)]
pub(crate) struct PendingDocBatch {
    /// Repository-relative path, forward-slash separated. Becomes the `path`
    /// column in LanceDB.
    pub rel_path: String,
    /// Content hash (hex) of the source document — the key under which the `.doc.msgpack` blob is
    /// content-addressed. The flush re-reads that blob to rebuild rows on demand.
    pub blob_hash: String,
    /// LanceDB scope stamped onto this file's emitted rows (the repo scope, or `path:<extra_root>`
    /// for an external-root document). The delete predicate uses the scan-wide scope, mirroring the
    /// pre-streaming behavior; only the inserted rows carry this per-file scope.
    pub doc_scope: String,
    /// Number of chunks indexed (zero is valid — xberg may yield no chunks
    /// when the file body is empty or below the chunk threshold).
    pub chunk_count: usize,
    /// Length of each chunk's embedding vector. Zero when embeddings are off.
    pub embedding_dim: u16,
    /// Whether this file should emit LanceDB rows at flush time. False for the no-embed, empty, and
    /// over-`max_chunks_per_document` cases — the blob is still tracked, but nothing lands in LanceDB
    /// (mirrors the old rows-empty batch). Decided at construction, where the document is in hand.
    pub emit_rows: bool,
}

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
pub(crate) fn doc_config_from(cfg: &DocumentsConfig, llm: &LlmConfig, embed: bool) -> DocConfig {
    DocConfig {
        max_characters: cfg.max_characters,
        overlap: cfg.overlap,
        embedding_preset: Some(cfg.embedding_preset.clone()),
        embed,
        language: cfg.language.clone(),
        keywords: cfg.keywords.clone(),
        ner: cfg.ner.clone(),
        summarization: cfg.summarization.clone(),
        llm: llm.clone(),
        embed_max_threads: cfg.embed_max_threads,
    }
}

/// Archive / compressed-container extensions. xberg routes `.zip/.tar/.gz/...` into its
/// `ZipExtractor`/`GzipExtractor`/`build_archive_doc` path, which recursively unpacks the archive and
/// embeds every entry — an enormous, pointless cost during a code-map scan (the observed 1119%-CPU /
/// 15 GB footgun). Rejected by default so a single archive can't explode into thousands of embeds;
/// gated OFF (relaxed → routed to xberg's archive extractor) when `documents.extract_archives` is
/// set. This extension floor is the primary guard because `mime::detect_mime_type` is extension-based
/// and several of these (`.jar/.whl/.war/...`) collapse to `application/octet-stream` via the
/// `mime_guess` fallback, so a MIME-only denylist would miss them. Kept separate from
/// [`BINARY_EXTENSIONS`] so the toggle only affects archives, never true binaries.
const ARCHIVE_EXTENSIONS: &[&str] = &[
    "zip", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "zst", "zstd", "7z", "rar", "lz", "lz4", "lzma", "br",
    "cab", "ar", "iso", "dmg", "jar", "war", "ear", "apk", "whl", "egg", "deb", "rpm", "nupkg", "pkg", "msi", "crate",
];

/// Native / compiled-binary extensions. ALWAYS rejected — these carry no extractable text, and
/// `extract_archives` never relaxes them (they are not archives).
const BINARY_EXTENSIONS: &[&str] = &[
    "so", "dylib", "dll", "a", "o", "obj", "bin", "exe", "wasm", "class", "pyc", "pyo", "pyd", "node", "pack", "idx",
];

/// Archive MIME denylist (belt-and-suspenders with [`ARCHIVE_EXTENSIONS`]) — catches content-typed
/// archives xberg maps to a real archive MIME. Gated OFF by `documents.extract_archives`, mirroring
/// the extension floor. Entries ending in `/` are prefix matches (see [`matches_mime`]).
const ARCHIVE_MIME: &[&str] = &[
    "application/zip",
    "application/x-tar",
    "application/gzip",
    "application/x-7z-compressed",
    "application/java-archive",
    "application/vnd.rar",
];

/// Binary MIME denylist — the unambiguously non-extractable native-binary + audio/video/font
/// families. ALWAYS rejected (never relaxed by `extract_archives`). `image/` is deliberately NOT
/// denied: xberg OCR can extract text from images, so images retain their allowlist behavior.
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

/// True when a file must be skipped by the document tier because it is a binary blob, or (unless
/// `documents.extract_archives` is set) an archive / compressed container. True binaries and the
/// user-configured `extension_denylist` are always rejected; the archive floor is relaxed only when
/// the caller opts into archive extraction.
fn is_denied_binary_or_archive(abs: &Path, mime_type: &str, cfg: &DocumentsConfig) -> bool {
    if let Some(ext) = abs.extension().and_then(|e| e.to_str()) {
        let ext_lower = ext.to_ascii_lowercase();
        if binary_ext_set().contains(ext_lower.as_str())
            || cfg
                .extension_denylist
                .iter()
                .any(|e| e.eq_ignore_ascii_case(&ext_lower))
        {
            return true;
        }
        if !cfg.extract_archives && archive_ext_set().contains(ext_lower.as_str()) {
            return true;
        }
    }
    if BINARY_MIME.iter().any(|entry| matches_mime(entry, mime_type)) {
        return true;
    }
    !cfg.extract_archives && ARCHIVE_MIME.iter().any(|entry| matches_mime(entry, mime_type))
}

/// Quick filter run before any xberg work happens. Returns the detected
/// MIME type when the file should be document-extracted, or `None` when it
/// should be skipped (configured-off, archive/binary, MIME unknown, MIME outside the allowlist).
///
/// The MIME allowlist is treated as "match this exact MIME OR a prefix ending
/// in `/`" so callers can say `"image/"` to whitelist every image type.
pub(crate) fn should_extract_document(abs: &Path, cfg: &DocumentsConfig) -> Option<String> {
    if !cfg.enabled {
        return None;
    }
    let mime_type = mime::detect_mime_type(abs, false).ok()?;
    if is_denied_binary_or_archive(abs, &mime_type, cfg) {
        return None;
    }
    if cfg.mime_allowlist.is_empty() {
        return Some(mime_type);
    }
    let allowed = cfg.mime_allowlist.iter().any(|entry| matches_mime(entry, &mime_type));
    if allowed { Some(mime_type) } else { None }
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
    mime_type: &str,
    cfg: &DocumentsConfig,
    llm: &LlmConfig,
    scope: &str,
    mode: EmbedMode,
) -> Result<Option<PendingDocBatch>, anyhow::Error> {
    let embed = matches!(mode, EmbedMode::Inline)
        && cfg.embed
        && !crate::scanner_filter::embed_excluded(rel, &cfg.embed_exclude);
    let hex_buf = hashing::hex_buf(hash);
    let hash_hex = hashing::hex_str(&hex_buf);

    if let Some(cached) = store.read_doc_by_hex(hash_hex).ok().flatten()
        && cached_doc_is_reusable(&cached, cfg, embed)
    {
        return Ok(Some(pending_from_doc(&cached, rel, hash_hex, scope, cfg, embed)));
    }

    let doc_config = doc_config_from(cfg, llm, embed);
    let doc: FileMapDoc =
        extract_doc(abs, Some(mime_type), &doc_config).with_context(|| format!("extract document {rel}"))?;
    store
        .write_doc(hash, &doc)
        .with_context(|| format!("write doc blob for {rel}"))?;

    Ok(Some(pending_from_doc(&doc, rel, hash_hex, scope, cfg, embed)))
}

/// True when a cached document blob can be reused without re-extraction. When embedding is on the
/// cached blob must carry embeddings produced by the current preset — matching both its **dimension**
/// AND its **model**. The model check is load-bearing: `balanced` and `multilingual` share dim 768,
/// so a dim-only gate would falsely reuse stale-model vectors when switching between them. A preset
/// change (dim OR model) therefore forces recompute — same gate as `chunk_and_embed`. An
/// empty-of-chunks doc is always reusable (recompute would yield nothing anyway). When embedding is
/// off, any cached doc is reusable (chunks only).
fn cached_doc_is_reusable(cached: &FileMapDoc, cfg: &DocumentsConfig, embed: bool) -> bool {
    if !embed || cached.chunks.is_empty() {
        return true;
    }
    let want_dim = preset_dim(&cfg.embedding_preset).ok();
    cached.embedding_dim > 0
        && Some(cached.embedding_dim) == want_dim
        && cached.embedding_model == cfg.embedding_preset
        && cached
            .chunks
            .iter()
            .all(|c| c.embedding.len() == cached.embedding_dim as usize)
}

/// Assemble the deferred-write descriptor from an extracted-or-cached document. Decides — while the
/// document is in hand — whether this file will emit LanceDB rows at flush time: only when embedding
/// is on, the doc has embeddings, it has chunks, and it is under the per-doc chunk cap. The heavy
/// row-building (chunk text + embedding vectors) is deferred to [`flush_document_batches`], which
/// re-reads the persisted blob per file so no embeddings are held across the corpus.
fn pending_from_doc(
    doc: &FileMapDoc,
    rel: &str,
    blob_hash: &str,
    scope: &str,
    cfg: &DocumentsConfig,
    embed: bool,
) -> PendingDocBatch {
    let chunk_count = doc.chunks.len();
    let embedding_dim = doc.embedding_dim;
    let over_cap = chunk_count > cfg.max_chunks_per_document;
    if over_cap {
        tracing::warn!(
            rel,
            chunks = chunk_count,
            cap = cfg.max_chunks_per_document,
            "document exceeds max_chunks_per_document; caching blob but skipping vector rows"
        );
    }
    let emit_rows = embed && embedding_dim > 0 && chunk_count > 0 && !over_cap;
    PendingDocBatch {
        rel_path: rel.to_string(),
        blob_hash: blob_hash.to_string(),
        doc_scope: scope.to_string(),
        chunk_count,
        embedding_dim,
        emit_rows,
    }
}

/// Turn a document's chunks into LanceDB rows. Hoists the per-row constant strings out of the map so
/// they allocate once instead of once per chunk (a doc can have hundreds of chunks).
///
/// Takes `doc` by value to move `chunk.text` and `chunk.embedding` directly into each row —
/// avoids cloning the chunk text (potentially large) and the embedding vector (`Vec<f32>`,
/// hundreds of kilobytes per chunk at typical preset dimensions).
fn build_doc_rows(doc: FileMapDoc, rel: &str, scope: &str) -> Vec<DocumentRow> {
    let scope_owned = scope.to_string();
    let rel_owned = rel.to_string();
    let mime_owned = doc.mime_type;
    doc.chunks
        .into_iter()
        .enumerate()
        .map(|(idx, chunk)| DocumentRow {
            scope: scope_owned.clone(),
            path: rel_owned.clone(),
            chunk_idx: u32::try_from(idx).unwrap_or(u32::MAX),
            mime_type: mime_owned.clone(),
            text: chunk.text,
            byte_start: chunk.byte_start,
            byte_end: chunk.byte_end,
            embedding: chunk.embedding,
        })
        .collect()
}

/// Purge the LanceDB `documents` rows AND the `index.doc_files` entries of docs that no longer
/// exist (or are no longer routed to the doc tier). Serial apply pass; mirrors
/// `scanner_code::delete_stale_code_chunks`. The tracking entry is dropped unconditionally (the doc
/// cache metadata must not leak for a removed file); the LanceDB delete is best-effort and never
/// *creates* the store. External-root docs were written under a per-root scope, so the delete uses
/// [`doc_scope_for`] to match it.
pub(crate) fn delete_stale_documents(store: &mut Store, config: &crate::config::Config, scope: &str, stale: &[String]) {
    if stale.is_empty() {
        return;
    }
    for path in stale {
        store.remove_doc(path);
    }
    if store.lance.is_none() && !store.lance_dir_exists() {
        return;
    }
    let model = &config.documents.embedding_preset;
    let dim = match preset_dim(model) {
        Ok(dim) => dim,
        Err(error) => {
            tracing::warn!(?error, preset = %model, "doc stale purge: unknown preset; skipping lance delete");
            return;
        }
    };
    let lance = match store.lance_or_open(dim, model) {
        Ok(lance) => lance.clone(),
        Err(error) => {
            tracing::warn!(?error, "doc stale purge: open LanceStore failed; skipping");
            return;
        }
    };
    for path in stale {
        let doc_scope = doc_scope_for(path, scope, config);
        if let Err(error) = lance.replace_document(doc_scope.as_ref(), path, Vec::new()) {
            tracing::warn!(
                rel = %path,
                ?error,
                "doc stale purge failed; search_documents may return a removed path"
            );
        }
    }
}

/// Stream every pending document batch into LanceDB, one file at a time. Opens the store lazily — if
/// no batch will emit rows (no embeddings configured) no LanceDB connection is ever made.
///
/// For each row-emitting file this re-reads the file's already-persisted `.doc.msgpack` blob, builds
/// its [`DocumentRow`]s, writes them, and drops them before the next file — so peak embed-write RAM
/// is one document's rows plus one Arrow batch, independent of corpus size. (The blob is guaranteed
/// on disk here: every doc that produced a batch was written by `extract_and_persist_doc` or was a
/// cache hit, both before this post-barrier lane runs.)
///
/// Returns the number of files for which rows were written. Errors are logged and skipped on a
/// per-file basis so one malformed embedding doesn't abort the scan.
pub(crate) fn flush_document_batches(
    store: &mut Store,
    scope: &str,
    batches: Vec<PendingDocBatch>,
    embedding_model: &str,
) -> usize {
    let mut inserted = 0usize;
    let Some(dim) = batches
        .iter()
        .find(|b| b.emit_rows && b.embedding_dim > 0)
        .map(|b| b.embedding_dim)
    else {
        return 0;
    };

    match preset_dim(embedding_model) {
        Ok(expected) if expected != dim => {
            tracing::error!(
                preset = %embedding_model,
                expected,
                actual = dim,
                "preset/runtime dim mismatch — refusing to write document batch"
            );
            return 0;
        }
        Ok(_) => {}
        Err(error) => {
            tracing::error!(
                ?error,
                preset = %embedding_model,
                "unknown embedding preset — refusing to write document batch"
            );
            return 0;
        }
    }

    let lance = match store.lance_or_open(dim, embedding_model) {
        Ok(s) => s.clone(),
        Err(error) => {
            tracing::error!(?error, "open LanceStore for document batch failed");
            return 0;
        }
    };

    for batch in batches {
        if !batch.emit_rows {
            continue;
        }
        let doc = match store.read_doc_by_hex(&batch.blob_hash) {
            Ok(Some(doc)) => doc,
            Ok(None) => {
                tracing::warn!(rel = %batch.rel_path, "doc blob missing at flush; skipping vector rows");
                continue;
            }
            Err(error) => {
                tracing::warn!(rel = %batch.rel_path, ?error, "re-read doc blob failed; skipping vector rows");
                continue;
            }
        };
        let rows = build_doc_rows(doc, &batch.rel_path, &batch.doc_scope);
        if rows.is_empty() {
            continue;
        }
        match lance.replace_document(scope, &batch.rel_path, rows) {
            Ok(()) => inserted += 1,
            Err(error) => {
                tracing::warn!(
                    rel = %batch.rel_path,
                    ?error,
                    "lance replace_document failed; document search may be incomplete"
                );
            }
        }
    }
    inserted
}

/// Choose the LanceDB scope for a document. Repo files keep the scan-wide `default_scope`;
/// external-root files (absolute key, see [`crate::path::RelPath::is_external`]) are scoped
/// `path:<extra_root>` so they group under the out-of-repo tree they came from rather than the
/// repository's own doc scope. Retrieval is unaffected — `search_documents` has no scope filter —
/// so this only partitions storage.
pub(crate) fn doc_scope_for<'a>(
    rel: &str,
    default_scope: &'a str,
    config: &crate::config::Config,
) -> std::borrow::Cow<'a, str> {
    if !crate::path::is_external_key(rel.as_bytes()) {
        return std::borrow::Cow::Borrowed(default_scope);
    }
    for raw_root in &config.scan.extra_roots {
        if let Ok(canonical) = raw_root.canonicalize()
            && let Some(prefix) = canonical.to_str()
        {
            #[cfg(windows)]
            let prefix = prefix.replace('\\', "/");
            #[cfg(windows)]
            let prefix = prefix.as_str();
            if rel.starts_with(prefix) {
                return std::borrow::Cow::Owned(format!("path:{prefix}"));
            }
        }
    }
    std::borrow::Cow::Owned(format!("path:{rel}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_dim_for_balanced_returns_768() {
        let dim = preset_dim("balanced").expect("balanced preset");
        assert_eq!(dim, 768);
    }

    /// A cached doc embedded under `balanced` (dim 768) must NOT be reused when the configured
    /// preset switches to `multilingual` (also dim 768, different model) — the dim matches, so only
    /// the model check closes the stale-vector hole. It stays reusable when the preset is unchanged.
    #[test]
    fn cached_doc_not_reusable_when_preset_model_differs_at_same_dim() {
        use crate::extract::doc::{DocChunk, FileMapDoc};
        let doc = FileMapDoc {
            schema_ver: 0,
            mime_type: "application/pdf".to_string(),
            content: "hello".to_string(),
            metadata: Vec::new(),
            detected_languages: Vec::new(),
            chunks: vec![DocChunk {
                byte_start: 0,
                byte_end: 5,
                text: "hello".to_string(),
                embedding: vec![0.0_f32; 768],
            }],
            embedding_model: "balanced".to_string(),
            embedding_dim: 768,
            keywords: Vec::new(),
            entities: Vec::new(),
            summary: None,
        };

        let same = DocumentsConfig {
            embedding_preset: "balanced".to_string(),
            ..DocumentsConfig::default()
        };
        assert!(
            cached_doc_is_reusable(&doc, &same, true),
            "same preset (balanced) must reuse the cached vectors"
        );

        let switched = DocumentsConfig {
            embedding_preset: "multilingual".to_string(),
            ..DocumentsConfig::default()
        };
        assert!(
            !cached_doc_is_reusable(&doc, &switched, true),
            "switching balanced -> multilingual (same dim, different model) must force recompute"
        );

        assert!(
            cached_doc_is_reusable(&doc, &switched, false),
            "embedding off: cached doc is always reusable"
        );
    }

    /// Structural regression guard for the streaming-flush memory fix: the accumulated per-file
    /// descriptor must stay metadata-only. This exhaustive struct literal names EXACTLY the metadata
    /// fields — re-introducing a `rows: Vec<DocumentRow>` (or any embedding payload) field would
    /// break this compile, catching a regression back to the corpus-wide accumulation that held
    /// multiple GB resident on a large repo.
    #[test]
    fn pending_doc_batch_is_metadata_only() {
        let batch = PendingDocBatch {
            rel_path: "docs/manual.pdf".to_string(),
            blob_hash: "deadbeef".to_string(),
            doc_scope: "repo:origin".to_string(),
            chunk_count: 3,
            embedding_dim: 768,
            emit_rows: true,
        };
        assert!(batch.emit_rows);
        assert_eq!(batch.chunk_count, 3);
    }

    #[test]
    fn preset_dim_for_unknown_errors() {
        let err = preset_dim("does-not-exist").expect_err("unknown preset");
        let msg = err.to_string();
        assert!(
            msg.contains("does-not-exist"),
            "error should name the preset; got: {msg}"
        );
    }

    #[test]
    fn extract_archives_toggle_gates_only_archives_not_binaries() {
        let default_cfg = DocumentsConfig::default();
        assert!(!default_cfg.extract_archives, "archives rejected by default");
        assert!(is_denied_binary_or_archive(
            Path::new("bundle.zip"),
            "application/zip",
            &default_cfg
        ));

        let extract_cfg = DocumentsConfig {
            extract_archives: true,
            ..DocumentsConfig::default()
        };
        assert!(
            !is_denied_binary_or_archive(Path::new("bundle.zip"), "application/zip", &extract_cfg),
            "extract_archives=true must route archives to the extractor"
        );
        assert!(
            is_denied_binary_or_archive(Path::new("libfoo.so"), "application/x-sharedlib", &extract_cfg),
            "binaries stay denied even with extract_archives=true"
        );
        assert!(
            is_denied_binary_or_archive(Path::new("mod.wasm"), "application/wasm", &extract_cfg),
            "wasm binary stays denied"
        );
    }

    #[test]
    fn matches_mime_exact_and_prefix() {
        assert!(matches_mime("application/pdf", "application/pdf"));
        assert!(matches_mime("image/", "image/png"));
        assert!(matches_mime("image/", "image/jpeg"));
        assert!(!matches_mime("image/", "video/mp4"));
        assert!(!matches_mime("application/pdf", "application/json"));
        assert!(!matches_mime("image/", "imageprocessing/x"));
    }

    #[test]
    fn doc_config_from_propagates_language_settings() {
        use crate::config::DocLanguageConfig;
        let cfg = DocumentsConfig {
            language: DocLanguageConfig {
                auto_detect: true,
                min_confidence: 0.5,
                detect_multiple: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let doc_cfg = doc_config_from(&cfg, &LlmConfig::default(), cfg.embed);
        assert!(doc_cfg.language.auto_detect);
        assert_eq!(doc_cfg.language.min_confidence, 0.5);
        assert!(doc_cfg.language.detect_multiple);
    }

    #[test]
    fn doc_config_from_propagates_summarization_and_llm() {
        use crate::config::{SummarizationConfig, SummarizationStrategy};
        let cfg = DocumentsConfig {
            summarization: SummarizationConfig {
                enabled: true,
                strategy: SummarizationStrategy::Abstractive,
                max_tokens: Some(150),
            },
            ..Default::default()
        };
        let llm = LlmConfig {
            model: "openai/gpt-4o".to_string(),
            ..Default::default()
        };
        let doc_cfg = doc_config_from(&cfg, &llm, cfg.embed);
        assert!(doc_cfg.summarization.enabled);
        assert_eq!(doc_cfg.summarization.max_tokens, Some(150));
        assert_eq!(doc_cfg.llm.model, "openai/gpt-4o");
    }

    #[test]
    fn should_extract_document_respects_disabled_flag() {
        let cfg = DocumentsConfig {
            enabled: false,
            ..Default::default()
        };
        let out = should_extract_document(Path::new("dummy.pdf"), &cfg);
        assert!(out.is_none());
    }

    #[test]
    fn should_extract_document_rejects_archives_and_binaries() {
        let cfg = DocumentsConfig::default();
        for path in [
            "vendor/lib.zip",
            "target/app.jar",
            "dist/bundle.tar.gz",
            "build/libfoo.so",
            "pkg/module.wasm",
            "out/Main.class",
            "wheels/pkg-1.0.whl",
            "bin/tool.exe",
            "obj/thing.o",
        ] {
            assert!(
                should_extract_document(Path::new(path), &cfg).is_none(),
                "archive/binary must be denied: {path}"
            );
        }
    }

    #[test]
    fn should_extract_document_allows_real_documents() {
        let cfg = DocumentsConfig::default();
        for path in ["docs/manual.pdf", "notes/readme.txt", "report.csv"] {
            assert!(
                should_extract_document(Path::new(path), &cfg).is_some(),
                "extractable document must pass: {path}"
            );
        }
    }

    #[test]
    fn should_extract_document_honors_extension_denylist_override() {
        let cfg = DocumentsConfig {
            extension_denylist: vec!["pdf".to_string()],
            ..Default::default()
        };
        assert!(should_extract_document(Path::new("docs/manual.pdf"), &cfg).is_none());
        assert!(should_extract_document(Path::new("vendor/lib.zip"), &cfg).is_none());
    }

    #[test]
    fn images_pass_but_audio_video_denied() {
        let cfg = DocumentsConfig::default();
        assert!(should_extract_document(Path::new("assets/photo.png"), &cfg).is_some());
        assert!(should_extract_document(Path::new("clips/audio.mp3"), &cfg).is_none());
        assert!(should_extract_document(Path::new("clips/movie.mp4"), &cfg).is_none());
    }

    #[test]
    fn doc_scope_keeps_default_for_repo_relative_paths() {
        let cfg = crate::config::ConfigV1::with_defaults();
        let scope = doc_scope_for("docs/manual.pdf", "repo:origin", &cfg);
        assert_eq!(scope, "repo:origin");
        assert!(matches!(scope, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn doc_scope_namespaces_external_files_under_their_extra_root() {
        let ext = tempfile::tempdir().expect("tempdir");
        let ext_canonical = std::fs::canonicalize(ext.path()).unwrap();
        let mut cfg = crate::config::ConfigV1::with_defaults();
        cfg.scan.extra_roots = vec![ext.path().to_path_buf()];

        let file_key = ext_canonical.join("pkg/notes.pdf");
        let scope = doc_scope_for(file_key.to_str().unwrap(), "repo:origin", &cfg);
        assert_eq!(scope, format!("path:{}", ext_canonical.to_str().unwrap()));
    }
}
