//! Document extraction tier — non-source files (PDFs, Office docs, emails,
//! images, …) ingested via `kreuzberg::extract_file_sync` and serialised to
//! `.basemind/blobs/<hash>.doc.msgpack`.
//!
//! Layered on top of the existing `l1` / `l2` blob shape:
//! - `l1`/`l2`/`l3` cover source code (tree-sitter outlines + calls + body hashes)
//! - `doc` covers everything else (PDFs, DOCX, XLSX, EML, HTML, images via OCR, …)
//!
//! When the document feature is on, each extracted chunk carries its embedding
//! vector inline so the scanner can stage it for LanceDB insert without a second
//! pass through the embedding engine.

use std::path::Path;

use kreuzberg::core::config::ExtractionConfig;
use kreuzberg::core::config::processing::{ChunkingConfig, EmbeddingConfig};
use kreuzberg::core::extractor::extract_file_sync;
use serde::{Deserialize, Serialize};

use super::{ExtractError, SCHEMA_VER};

/// Per-file document extraction result. Mirrors the shape of `FileMapL1` —
/// `schema_ver` for migration, plus the structured kreuzberg output we care
/// about for downstream vector search.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileMapDoc {
    pub schema_ver: u16,
    /// IANA MIME type as reported by kreuzberg's detector.
    pub mime_type: String,
    /// Plain-text representation of the document (concatenation of all chunks
    /// before chunking is applied; not exactly the source bytes).
    pub content: String,
    /// Document-level metadata (author, title, dates, format-specific keys).
    /// Flattened to `String -> String` so the on-disk shape stays stable.
    pub metadata: Vec<(String, String)>,
    /// ISO 639-1 language codes detected in the content, when language
    /// detection succeeded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detected_languages: Vec<String>,
    /// Chunks, each with its embedding vector inline. Empty when chunking is
    /// disabled in the kreuzberg config; embedding fields empty when the
    /// embedding engine is not configured.
    pub chunks: Vec<DocChunk>,
    /// Name of the embedding model that produced the vectors. Empty when no
    /// embeddings were generated. Used by the LanceDB layer to detect
    /// model-change wipes.
    pub embedding_model: String,
    /// Length of each chunk embedding vector. 0 when no embeddings.
    pub embedding_dim: u16,
}

/// A single chunked region of a document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocChunk {
    /// UTF-8 byte offset where this chunk starts in the original text.
    pub byte_start: u32,
    /// UTF-8 byte offset where this chunk ends.
    pub byte_end: u32,
    /// The chunk text. Stored even when an embedding is present so MCP search
    /// can return snippets without round-tripping to the source file.
    pub text: String,
    /// Embedding vector. Empty when chunking ran without an embedding config.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedding: Vec<f32>,
}

/// Caller-supplied knobs for document extraction.
///
/// Kept independent from kreuzberg's full `ExtractionConfig` so the scanner
/// callsite stays readable; we translate to `ExtractionConfig` at the boundary.
#[derive(Debug, Clone)]
pub struct DocConfig {
    pub max_characters: usize,
    pub overlap: usize,
    pub embedding_preset: Option<String>,
    pub embed: bool,
}

impl Default for DocConfig {
    fn default() -> Self {
        Self {
            max_characters: 1000,
            overlap: 200,
            embedding_preset: Some("balanced".to_string()),
            embed: true,
        }
    }
}

impl DocConfig {
    fn to_kreuzberg(&self) -> ExtractionConfig {
        let embedding = if self.embed {
            Some(EmbeddingConfig::default())
        } else {
            None
        };
        let chunking = ChunkingConfig {
            max_characters: self.max_characters,
            overlap: self.overlap,
            embedding,
            preset: self.embedding_preset.clone(),
            ..Default::default()
        };
        ExtractionConfig {
            chunking: Some(chunking),
            ..Default::default()
        }
    }
}

/// Run kreuzberg against `path` and translate the result into a `FileMapDoc`.
///
/// `mime_type` may be supplied by the caller (e.g. from `lang::detect`); when
/// `None`, kreuzberg sniffs the file content.
pub fn extract_doc(
    path: &Path,
    mime_type: Option<&str>,
    config: &DocConfig,
) -> Result<FileMapDoc, ExtractError> {
    let krz_config = config.to_kreuzberg();
    let result = extract_file_sync(path, mime_type, &krz_config)
        .map_err(|e| ExtractError::Document(e.to_string()))?;

    let mut chunks: Vec<DocChunk> = Vec::new();
    let mut embedding_dim: u16 = 0;
    if let Some(input_chunks) = result.chunks {
        for c in input_chunks {
            let dim = c.embedding.as_ref().map(|v| v.len()).unwrap_or(0);
            if dim > 0 && embedding_dim == 0 {
                embedding_dim = u16::try_from(dim).unwrap_or(u16::MAX);
            }
            chunks.push(DocChunk {
                byte_start: u32::try_from(c.metadata.byte_start).unwrap_or(u32::MAX),
                byte_end: u32::try_from(c.metadata.byte_end).unwrap_or(u32::MAX),
                text: c.content,
                embedding: c.embedding.unwrap_or_default(),
            });
        }
    }

    let embedding_model = if embedding_dim > 0 {
        config
            .embedding_preset
            .clone()
            .unwrap_or_else(|| "default".to_string())
    } else {
        String::new()
    };

    let metadata = metadata_pairs(&result.metadata);

    Ok(FileMapDoc {
        schema_ver: SCHEMA_VER,
        mime_type: result.mime_type.into_owned(),
        content: result.content,
        metadata,
        detected_languages: result.detected_languages.unwrap_or_default(),
        chunks,
        embedding_model,
        embedding_dim,
    })
}

fn metadata_pairs(metadata: &kreuzberg::types::Metadata) -> Vec<(String, String)> {
    // Round-trip the metadata via JSON to flatten its (large, heterogeneous)
    // shape into stable string pairs without enumerating every field.
    match serde_json::to_value(metadata) {
        Ok(serde_json::Value::Object(map)) => map
            .into_iter()
            .filter_map(|(k, v)| {
                let value_str = match v {
                    serde_json::Value::Null => return None,
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                Some((k, value_str))
            })
            .collect(),
        _ => Vec::new(),
    }
}
