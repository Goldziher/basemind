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
//!    returns a `PendingDocBatch` carrying the rows that need to land in
//!    LanceDB.
//! 4. The single-threaded apply pass calls `flush_document_batches` to push
//!    all pending rows into LanceDB in one pass.

#![cfg(feature = "documents")]

use std::path::Path;

use anyhow::Context as _;
use xberg::core::mime;
use xberg::embeddings::{EMBEDDING_PRESETS, EmbeddingPreset};

use crate::config::{DocumentsConfig, LlmConfig};
use crate::extract::doc::{DocConfig, FileMapDoc, extract_doc};
use crate::hashing::{self, Hash};
use crate::lance::DocumentRow;
use crate::store::Store;

/// Per-file deferred LanceDB write. Constructed inside `process_file`'s parallel
/// worker; consumed in the single-threaded apply pass via
/// [`flush_document_batches`].
#[derive(Debug, Clone)]
pub(crate) struct PendingDocBatch {
    /// Repository-relative path, forward-slash separated. Becomes the `path`
    /// column in LanceDB.
    pub rel_path: String,
    /// Number of chunks indexed (zero is valid — xberg may yield no chunks
    /// when the file body is empty or below the chunk threshold).
    pub chunk_count: usize,
    /// Length of each chunk's embedding vector. Zero when embeddings are off.
    pub embedding_dim: u16,
    /// The chunks themselves, ready to be turned into [`DocumentRow`]s.
    pub rows: Vec<DocumentRow>,
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
pub(crate) fn doc_config_from(cfg: &DocumentsConfig, llm: &LlmConfig) -> DocConfig {
    DocConfig {
        max_characters: cfg.max_characters,
        overlap: cfg.overlap,
        embedding_preset: Some(cfg.embedding_preset.clone()),
        embed: cfg.embed,
        language: cfg.language.clone(),
        keywords: cfg.keywords.clone(),
        ner: cfg.ner.clone(),
        // Summarisation + LLM ride on the same `DocConfig` because the boundary
        // code in `DocConfig::to_xberg` resolves abstractive ⇒ LLM lookup
        // in one place. The top-level `[llm]` block is intentionally shared
        // across capabilities (ner-llm, summarization-llm, …).
        summarization: cfg.summarization.clone(),
        llm: llm.clone(),
    }
}

/// Archive / compressed / binary-blob extensions that must NEVER reach xberg. xberg routes
/// `.zip/.tar/.gz/...` into its `ZipExtractor`/`GzipExtractor`/`build_archive_doc` path, which
/// recursively unpacks the archive and embeds every entry — an enormous, pointless cost during a
/// code-map scan (the observed 1119%-CPU / 15 GB footgun). Binary blobs (`.so/.class/.wasm/...`)
/// carry no extractable text and only waste an embed. This is the primary guard because
/// `mime::detect_mime_type` is extension-based, and several of these (`.jar/.whl/.war/.so/...`)
/// collapse to `application/octet-stream` via the `mime_guess` fallback, so a MIME-only denylist
/// would miss them.
const ARCHIVE_BINARY_EXTENSIONS: &[&str] = &[
    // archives / compressed
    "zip", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "zst", "zstd", "7z", "rar", "lz", "lz4", "lzma", "br", "cab",
    "ar", "iso", "dmg", // packaged archives (zip/tar underneath)
    "jar", "war", "ear", "apk", "whl", "egg", "deb", "rpm", "nupkg", "pkg", "msi", "crate",
    // native / compiled binaries
    "so", "dylib", "dll", "a", "o", "obj", "bin", "exe", "wasm", "class", "pyc", "pyo", "pyd", "node", "pack", "idx",
];

/// MIME denylist (belt-and-suspenders with [`ARCHIVE_BINARY_EXTENSIONS`]) — catches content-typed
/// archives xberg maps to a real archive MIME (`.zip/.tar/.gz/.tgz/.7z`) plus the unambiguously
/// non-extractable audio/video/font families. Entries ending in `/` are prefix matches (see
/// [`matches_mime`]). `image/` is deliberately NOT denied: xberg OCR can extract text from images,
/// so images retain their pre-existing allowlist behavior.
const DENY_MIME: &[&str] = &[
    "application/zip",
    "application/x-tar",
    "application/gzip",
    "application/x-7z-compressed",
    "application/java-archive",
    "application/vnd.rar",
    "application/wasm",
    "application/x-executable",
    "application/x-sharedlib",
    "application/x-mach-binary",
    "application/octet-stream",
    "audio/",
    "video/",
    "font/",
];

/// True when a file must be skipped by the document tier because it is an archive, a compressed
/// container, or a binary blob. Checks the extension denylist (the const floor plus any
/// user-configured `extension_denylist`) first, then the MIME denylist.
fn is_denied_binary_or_archive(abs: &Path, mime_type: &str, cfg: &DocumentsConfig) -> bool {
    if let Some(ext) = abs.extension().and_then(|e| e.to_str()) {
        let ext_lower = ext.to_ascii_lowercase();
        if ARCHIVE_BINARY_EXTENSIONS.contains(&ext_lower.as_str())
            || cfg
                .extension_denylist
                .iter()
                .any(|e| e.eq_ignore_ascii_case(&ext_lower))
        {
            return true;
        }
    }
    DENY_MIME.iter().any(|entry| matches_mime(entry, mime_type))
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
    // Reject archives / compressed containers / binaries BEFORE the allowlist branch, so they never
    // reach xberg's recursive archive extractor + embedder regardless of allowlist configuration.
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
        // Treat "image/" as the prefix "image/" — match `image/png` etc.
        // Zero-alloc: check the prefix then that the very next byte is `/`,
        // instead of building a throwaway `format!("{prefix}/")` per call.
        return mime_type.starts_with(prefix) && mime_type.as_bytes().get(prefix.len()) == Some(&b'/');
    }
    false
}

/// Run xberg against `abs`, write the document blob to the content-addressed
/// store, and assemble a [`PendingDocBatch`] for the apply pass. Returns
/// `Ok(None)` when extraction succeeded but produced no embeddings (we still
/// persist the blob; the LanceDB side is just a no-op for that file).
// Eight args sits one above clippy's default seven-arg threshold; collapsing
// `cfg` + `llm` into a wrapper just to satisfy the lint would obscure the
// callsite — the scanner already deals in those two structs by reference.
#[allow(clippy::too_many_arguments)]
pub(crate) fn extract_and_persist_doc(
    store: &Store,
    rel: &str,
    abs: &Path,
    bytes: &[u8],
    mime_type: &str,
    cfg: &DocumentsConfig,
    llm: &LlmConfig,
    scope: &str,
) -> Result<Option<PendingDocBatch>, anyhow::Error> {
    let doc_config = doc_config_from(cfg, llm);
    let doc: FileMapDoc =
        extract_doc(abs, Some(mime_type), &doc_config).with_context(|| format!("extract document {rel}"))?;

    // Content-address the blob on the source bytes — same flow as L1/L2 so the
    // blob is shared across views that hash the same content.
    let hash: Hash = hashing::hash_bytes(bytes);
    store
        .write_doc(&hash, &doc)
        .with_context(|| format!("write doc blob for {rel}"))?;

    // Guard against a pathological input exploding into tens of thousands of vector rows. The blob
    // is still cached (so it round-trips), but we emit no LanceDB rows. NOTE: xberg embeds during
    // `extract_doc`, so this bounds the LanceDB write, not the embed compute — WS4 gates compute.
    if doc.chunks.len() > cfg.max_chunks_per_document {
        tracing::warn!(
            rel,
            chunks = doc.chunks.len(),
            cap = cfg.max_chunks_per_document,
            "document exceeds max_chunks_per_document; caching blob but skipping vector rows"
        );
        return Ok(Some(PendingDocBatch {
            rel_path: rel.to_string(),
            chunk_count: doc.chunks.len(),
            embedding_dim: doc.embedding_dim,
            rows: Vec::new(),
        }));
    }

    if doc.embedding_dim == 0 || doc.chunks.is_empty() {
        return Ok(Some(PendingDocBatch {
            rel_path: rel.to_string(),
            chunk_count: doc.chunks.len(),
            embedding_dim: doc.embedding_dim,
            rows: Vec::new(),
        }));
    }

    // Hoist the per-row constant strings out of the map so we allocate them
    // once instead of once per chunk (a doc can have hundreds of chunks).
    let scope_owned = scope.to_string();
    let rel_owned = rel.to_string();
    let mime_owned = doc.mime_type.clone();
    let rows: Vec<DocumentRow> = doc
        .chunks
        .iter()
        .enumerate()
        .map(|(idx, chunk)| DocumentRow {
            scope: scope_owned.clone(),
            path: rel_owned.clone(),
            chunk_idx: u32::try_from(idx).unwrap_or(u32::MAX),
            mime_type: mime_owned.clone(),
            text: chunk.text.clone(),
            byte_start: chunk.byte_start,
            byte_end: chunk.byte_end,
            embedding: chunk.embedding.clone(),
        })
        .collect();

    Ok(Some(PendingDocBatch {
        rel_path: rel.to_string(),
        chunk_count: rows.len(),
        embedding_dim: doc.embedding_dim,
        rows,
    }))
}

/// Push every pending document batch into LanceDB. Opens the store lazily — if
/// every batch is empty (no embeddings configured) no LanceDB connection is
/// ever made.
///
/// Returns the number of files for which rows were written. Errors are logged
/// and skipped on a per-file basis so one malformed embedding doesn't abort the
/// scan.
pub(crate) fn flush_document_batches(
    store: &mut Store,
    scope: &str,
    batches: Vec<PendingDocBatch>,
    embedding_model: &str,
) -> usize {
    let mut inserted = 0usize;
    // Determine the dim from the first batch that actually has rows; if every
    // batch is empty (no embeddings), there's nothing to push.
    let Some(dim) = batches.iter().find(|b| b.embedding_dim > 0).map(|b| b.embedding_dim) else {
        return 0;
    };

    // Sanity-check the configured preset name against the runtime dim. A mismatch here
    // means either the user passed an unknown preset, or the embedding backend swapped
    // out from under us — both worth surfacing rather than silently writing the wrong
    // dim into LanceDB.
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
        if batch.rows.is_empty() {
            continue;
        }
        match lance.replace_document(scope, &batch.rel_path, batch.rows) {
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
    if !rel.starts_with('/') {
        return std::borrow::Cow::Borrowed(default_scope);
    }
    for raw_root in &config.scan.extra_roots {
        if let Ok(canonical) = raw_root.canonicalize()
            && let Some(prefix) = canonical.to_str()
            && rel.starts_with(prefix)
        {
            return std::borrow::Cow::Owned(format!("path:{prefix}"));
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
    fn matches_mime_exact_and_prefix() {
        assert!(matches_mime("application/pdf", "application/pdf"));
        assert!(matches_mime("image/", "image/png"));
        assert!(matches_mime("image/", "image/jpeg"));
        assert!(!matches_mime("image/", "video/mp4"));
        assert!(!matches_mime("application/pdf", "application/json"));
        // The zero-alloc byte check must not let a longer type name that merely
        // *starts with* the prefix slip through: `image/` must NOT match
        // `imageprocessing/x` (next byte after `image` is `p`, not `/`).
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
        let doc_cfg = doc_config_from(&cfg, &LlmConfig::default());
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
        let doc_cfg = doc_config_from(&cfg, &llm);
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
        // Use a path that doesn't need to exist — the disabled flag short-circuits
        // before xberg ever touches the filesystem.
        let out = should_extract_document(Path::new("dummy.pdf"), &cfg);
        assert!(out.is_none());
    }

    #[test]
    fn should_extract_document_rejects_archives_and_binaries() {
        // `detect_mime_type(_, false)` is extension-based and does not stat the file, so
        // non-existent paths are fine here.
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
        // The built-in floor still applies AND the user extension is now denied too.
        assert!(should_extract_document(Path::new("docs/manual.pdf"), &cfg).is_none());
        assert!(should_extract_document(Path::new("vendor/lib.zip"), &cfg).is_none());
    }

    #[test]
    fn images_pass_but_audio_video_denied() {
        let cfg = DocumentsConfig::default();
        // Images retain allowlist behavior (xberg OCR may extract text) — not auto-denied.
        assert!(should_extract_document(Path::new("assets/photo.png"), &cfg).is_some());
        // Audio / video are never extractable → denied via the MIME prefix denylist.
        assert!(should_extract_document(Path::new("clips/audio.mp3"), &cfg).is_none());
        assert!(should_extract_document(Path::new("clips/movie.mp4"), &cfg).is_none());
    }

    #[test]
    fn doc_scope_keeps_default_for_repo_relative_paths() {
        let cfg = crate::config::ConfigV1::with_defaults();
        // Repo-relative keys (no leading `/`) keep the scan-wide default scope, borrowed.
        let scope = doc_scope_for("docs/manual.pdf", "repo:origin", &cfg);
        assert_eq!(scope, "repo:origin");
        assert!(matches!(scope, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn doc_scope_namespaces_external_files_under_their_extra_root() {
        // An external (absolute) key is scoped by the owning `extra_roots` entry so out-of-repo
        // documents don't land in the repository's own doc scope.
        let ext = tempfile::tempdir().expect("tempdir");
        let ext_canonical = std::fs::canonicalize(ext.path()).unwrap();
        let mut cfg = crate::config::ConfigV1::with_defaults();
        cfg.scan.extra_roots = vec![ext.path().to_path_buf()];

        let file_key = ext_canonical.join("pkg/notes.pdf");
        let scope = doc_scope_for(file_key.to_str().unwrap(), "repo:origin", &cfg);
        assert_eq!(scope, format!("path:{}", ext_canonical.to_str().unwrap()));
    }
}
