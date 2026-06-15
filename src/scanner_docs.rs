//! Document-tier branch helpers for the scanner.
//!
//! Gated on `feature = "documents"`. Lives in its own file so the core
//! `src/scanner.rs` module stays under the 1000-line cap and so the
//! intelligence-only code path is easy to inspect in isolation.
//!
//! The flow that the scanner drives is:
//!
//! 1. `process_file` sees `lang::detect` return `None` (non-source file).
//! 2. [`should_extract_document`] decides whether the file qualifies for the
//!    document tier based on `[documents]` config + MIME allowlist.
//! 3. [`extract_and_persist_doc`] runs kreuzberg, writes the msgpack blob, and
//!    returns a [`PendingDocBatch`] carrying the rows that need to land in
//!    LanceDB.
//! 4. The single-threaded apply pass calls [`flush_document_batches`] to push
//!    all pending rows into LanceDB in one pass.

#![cfg(feature = "documents")]

use std::path::Path;

use anyhow::Context as _;
use kreuzberg::core::mime;
use kreuzberg::embeddings::{EMBEDDING_PRESETS, EmbeddingPreset};

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
    /// Number of chunks indexed (zero is valid — kreuzberg may yield no chunks
    /// when the file body is empty or below the chunk threshold).
    pub chunk_count: usize,
    /// Length of each chunk's embedding vector. Zero when embeddings are off.
    pub embedding_dim: u16,
    /// The chunks themselves, ready to be turned into [`DocumentRow`]s.
    pub rows: Vec<DocumentRow>,
}

/// Look the configured embedding preset up in kreuzberg's preset table and
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
        .with_context(|| format!("unknown kreuzberg embedding preset: {name}"))?;
    u16::try_from(preset.dimensions)
        .with_context(|| format!("preset {name} dimensions {} exceeds u16", preset.dimensions))
}

/// Translate the project-level `[documents]` config into the kreuzberg-facing
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
        // code in `DocConfig::to_kreuzberg` resolves abstractive ⇒ LLM lookup
        // in one place. The top-level `[llm]` block is intentionally shared
        // across capabilities (ner-llm, summarization-llm, …).
        summarization: cfg.summarization.clone(),
        llm: llm.clone(),
    }
}

/// Quick filter run before any kreuzberg work happens. Returns the detected
/// MIME type when the file should be document-extracted, or `None` when it
/// should be skipped (configured-off, MIME unknown, MIME outside the allowlist).
///
/// The MIME allowlist is treated as "match this exact MIME OR a prefix ending
/// in `/`" so callers can say `"image/"` to whitelist every image type.
pub(crate) fn should_extract_document(abs: &Path, cfg: &DocumentsConfig) -> Option<String> {
    if !cfg.enabled {
        return None;
    }
    let mime_type = mime::detect_mime_type(abs, false).ok()?;
    if cfg.mime_allowlist.is_empty() {
        return Some(mime_type);
    }
    let allowed = cfg
        .mime_allowlist
        .iter()
        .any(|entry| matches_mime(entry, &mime_type));
    if allowed { Some(mime_type) } else { None }
}

fn matches_mime(entry: &str, mime_type: &str) -> bool {
    if entry == mime_type {
        return true;
    }
    if let Some(prefix) = entry.strip_suffix('/') {
        // Treat "image/" as the prefix "image/" — match `image/png` etc.
        return mime_type.starts_with(&format!("{prefix}/"));
    }
    false
}

/// Run kreuzberg against `abs`, write the document blob to the content-addressed
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
    let doc: FileMapDoc = extract_doc(abs, Some(mime_type), &doc_config)
        .with_context(|| format!("extract document {rel}"))?;

    // Content-address the blob on the source bytes — same flow as L1/L2 so the
    // blob is shared across views that hash the same content.
    let hash: Hash = hashing::hash_bytes(bytes);
    store
        .write_doc(&hash, &doc)
        .with_context(|| format!("write doc blob for {rel}"))?;

    if doc.embedding_dim == 0 || doc.chunks.is_empty() {
        return Ok(Some(PendingDocBatch {
            rel_path: rel.to_string(),
            chunk_count: doc.chunks.len(),
            embedding_dim: doc.embedding_dim,
            rows: Vec::new(),
        }));
    }

    let rows: Vec<DocumentRow> = doc
        .chunks
        .iter()
        .enumerate()
        .map(|(idx, chunk)| DocumentRow {
            scope: scope.to_string(),
            path: rel.to_string(),
            chunk_idx: u32::try_from(idx).unwrap_or(u32::MAX),
            mime_type: doc.mime_type.clone(),
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
    let Some(dim) = batches
        .iter()
        .find(|b| b.embedding_dim > 0)
        .map(|b| b.embedding_dim)
    else {
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
        // before kreuzberg ever touches the filesystem.
        let out = should_extract_document(Path::new("dummy.pdf"), &cfg);
        assert!(out.is_none());
    }
}
