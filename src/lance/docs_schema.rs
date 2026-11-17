//! Arrow shape of the retrieval store's two tables (ADR-0029 rule 2): `doc_chunks`, one immutable
//! row per `(doc_key, chunk_ord)`, and `docs`, one row per `doc_key`. Rows carry no path and no
//! scope: which documents a view can see is decided by its live keys at query time.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
#[cfg(test)]
use lancedb::arrow::arrow_array::ListArray;
use lancedb::arrow::arrow_array::builder::{
    BooleanBuilder, FixedSizeListBuilder, Float32Builder, ListBuilder, StringBuilder, UInt32Builder,
};
use lancedb::arrow::arrow_array::{Array, BooleanArray, RecordBatch, StringArray, UInt32Array};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema, SchemaRef};

/// One row per chunk of a document.
pub const DOC_CHUNKS_TABLE: &str = "doc_chunks";
/// One row per document; also the record that a document's chunk rows are complete.
pub const DOCS_TABLE: &str = "docs";

/// Separator of a chunk's heading trail in the `section` column.
pub const SECTION_SEPARATOR: &str = " > ";

/// The columns a search reads back: everything but the vector.
pub const HIT_COLUMNS: &[&str] = &[
    "doc_key",
    "chunk_ord",
    "mime_type",
    "chunk_kind",
    "page_start",
    "page_end",
    "section",
    "line_start",
    "line_end",
    "byte_start",
    "byte_end",
    "text",
];

fn utf8_list() -> DataType {
    DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)))
}

fn vector(dim: u16) -> DataType {
    DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), i32::from(dim))
}

/// `doc_chunks`. `embedding` is null for a document that was not embedded: it is still found by
/// the lexical lane.
pub fn doc_chunks_schema(dim: u16) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("doc_key", DataType::Utf8, false),
        Field::new("chunk_ord", DataType::UInt32, false),
        Field::new("mime_type", DataType::Utf8, false),
        Field::new("mime_group", DataType::Utf8, false),
        Field::new("chunk_kind", DataType::Utf8, false),
        Field::new("page_start", DataType::UInt32, true),
        Field::new("page_end", DataType::UInt32, true),
        Field::new("section", DataType::Utf8, true),
        Field::new("line_start", DataType::UInt32, true),
        Field::new("line_end", DataType::UInt32, true),
        Field::new("byte_start", DataType::UInt32, false),
        Field::new("byte_end", DataType::UInt32, false),
        Field::new("entity_cats", utf8_list(), false),
        Field::new("text", DataType::Utf8, false),
        Field::new("embedding", vector(dim), true),
    ]))
}

/// `docs`. `keywords` are lower-cased whole keywords; `embedded` says whether the chunk rows
/// carry vectors, so a later scan that has them replaces rows written without.
pub fn docs_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("doc_key", DataType::Utf8, false),
        Field::new("mime_type", DataType::Utf8, false),
        Field::new("title", DataType::Utf8, true),
        Field::new("keywords", utf8_list(), false),
        Field::new("n_chunks", DataType::UInt32, false),
        Field::new("embedded", DataType::Boolean, false),
    ]))
}

/// One chunk of a document, as stored.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DocChunkRow {
    pub chunk_ord: u32,
    pub chunk_kind: String,
    pub page_start: Option<u32>,
    pub page_end: Option<u32>,
    /// Heading trail joined with [`SECTION_SEPARATOR`].
    pub section: Option<String>,
    pub line_start: Option<u32>,
    pub line_end: Option<u32>,
    pub byte_start: u32,
    pub byte_end: u32,
    /// Lower-cased categories of the entities whose span overlaps the chunk.
    pub entity_cats: Vec<String>,
    pub text: String,
    pub embedding: Option<Vec<f32>>,
}

/// Every row of one document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DocRows {
    pub doc_key: String,
    pub mime_type: String,
    pub mime_group: String,
    pub title: Option<String>,
    pub keywords: Vec<String>,
    pub chunks: Vec<DocChunkRow>,
}

impl DocRows {
    /// Whether the chunk rows carry vectors. A document with no chunk has nothing to embed.
    pub fn embedded(&self) -> bool {
        self.chunks.iter().all(|chunk| chunk.embedding.is_some())
    }
}

/// A chunk read back by a search or a neighbour lookup.
#[derive(Debug, Clone, PartialEq)]
pub struct DocChunkHit {
    pub doc_key: String,
    pub chunk_ord: u32,
    pub mime_type: String,
    pub chunk_kind: String,
    pub page_start: Option<u32>,
    pub page_end: Option<u32>,
    pub section: Option<String>,
    pub line_start: Option<u32>,
    pub line_end: Option<u32>,
    pub byte_start: u32,
    pub byte_end: u32,
    pub text: String,
}

/// What the `docs` table says about one document.
#[derive(Debug, Clone, PartialEq)]
pub struct DocMeta {
    pub doc_key: String,
    pub title: Option<String>,
    pub n_chunks: u32,
    pub embedded: bool,
}

/// The `doc_chunks` batch of `docs`. A vector of another dimension is an error: it would fail the
/// whole write.
pub fn chunks_batch(dim: u16, docs: &[DocRows]) -> Result<RecordBatch> {
    let mut doc_key = StringBuilder::new();
    let mut chunk_ord = UInt32Builder::new();
    let mut mime_type = StringBuilder::new();
    let mut mime_group = StringBuilder::new();
    let mut chunk_kind = StringBuilder::new();
    let mut page_start = UInt32Builder::new();
    let mut page_end = UInt32Builder::new();
    let mut section = StringBuilder::new();
    let mut line_start = UInt32Builder::new();
    let mut line_end = UInt32Builder::new();
    let mut byte_start = UInt32Builder::new();
    let mut byte_end = UInt32Builder::new();
    let mut entity_cats = ListBuilder::new(StringBuilder::new());
    let mut text = StringBuilder::new();
    let mut embedding = FixedSizeListBuilder::new(Float32Builder::new(), i32::from(dim));
    for doc in docs {
        for chunk in &doc.chunks {
            doc_key.append_value(&doc.doc_key);
            chunk_ord.append_value(chunk.chunk_ord);
            mime_type.append_value(&doc.mime_type);
            mime_group.append_value(&doc.mime_group);
            chunk_kind.append_value(&chunk.chunk_kind);
            page_start.append_option(chunk.page_start);
            page_end.append_option(chunk.page_end);
            section.append_option(chunk.section.as_deref());
            line_start.append_option(chunk.line_start);
            line_end.append_option(chunk.line_end);
            byte_start.append_value(chunk.byte_start);
            byte_end.append_value(chunk.byte_end);
            for category in &chunk.entity_cats {
                entity_cats.values().append_value(category);
            }
            entity_cats.append(true);
            text.append_value(&chunk.text);
            match &chunk.embedding {
                Some(vector) if vector.len() == usize::from(dim) => {
                    embedding.values().append_slice(vector);
                    embedding.append(true);
                }
                Some(vector) => {
                    return Err(anyhow!(
                        "chunk {} of {} has a vector of dimension {}, the store holds {dim}",
                        chunk.chunk_ord,
                        doc.doc_key,
                        vector.len()
                    ));
                }
                None => {
                    for _ in 0..dim {
                        embedding.values().append_null();
                    }
                    embedding.append(false);
                }
            }
        }
    }
    RecordBatch::try_new(
        doc_chunks_schema(dim),
        vec![
            Arc::new(doc_key.finish()),
            Arc::new(chunk_ord.finish()),
            Arc::new(mime_type.finish()),
            Arc::new(mime_group.finish()),
            Arc::new(chunk_kind.finish()),
            Arc::new(page_start.finish()),
            Arc::new(page_end.finish()),
            Arc::new(section.finish()),
            Arc::new(line_start.finish()),
            Arc::new(line_end.finish()),
            Arc::new(byte_start.finish()),
            Arc::new(byte_end.finish()),
            Arc::new(entity_cats.finish()),
            Arc::new(text.finish()),
            Arc::new(embedding.finish()),
        ],
    )
    .context("assemble doc_chunks batch")
}

/// The `docs` batch of `docs`.
pub fn docs_batch(docs: &[DocRows]) -> Result<RecordBatch> {
    let mut doc_key = StringBuilder::new();
    let mut mime_type = StringBuilder::new();
    let mut title = StringBuilder::new();
    let mut keywords = ListBuilder::new(StringBuilder::new());
    let mut n_chunks = UInt32Builder::new();
    let mut embedded = BooleanBuilder::new();
    for doc in docs {
        doc_key.append_value(&doc.doc_key);
        mime_type.append_value(&doc.mime_type);
        title.append_option(doc.title.as_deref());
        for keyword in &doc.keywords {
            keywords.values().append_value(keyword);
        }
        keywords.append(true);
        n_chunks.append_value(u32::try_from(doc.chunks.len()).unwrap_or(u32::MAX));
        embedded.append_value(doc.embedded());
    }
    RecordBatch::try_new(
        docs_schema(),
        vec![
            Arc::new(doc_key.finish()),
            Arc::new(mime_type.finish()),
            Arc::new(title.finish()),
            Arc::new(keywords.finish()),
            Arc::new(n_chunks.finish()),
            Arc::new(embedded.finish()),
        ],
    )
    .context("assemble docs batch")
}

fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<T>())
        .ok_or_else(|| anyhow!("`{name}` column missing or of another type"))
}

fn optional(array: &UInt32Array, i: usize) -> Option<u32> {
    array.is_valid(i).then(|| array.value(i))
}

/// Append the chunk rows of `batch` (projected to [`HIT_COLUMNS`]) to `out`.
pub fn decode_chunk_hits(batch: &RecordBatch, out: &mut Vec<DocChunkHit>) -> Result<()> {
    let doc_key = column::<StringArray>(batch, "doc_key")?;
    let chunk_ord = column::<UInt32Array>(batch, "chunk_ord")?;
    let mime_type = column::<StringArray>(batch, "mime_type")?;
    let chunk_kind = column::<StringArray>(batch, "chunk_kind")?;
    let page_start = column::<UInt32Array>(batch, "page_start")?;
    let page_end = column::<UInt32Array>(batch, "page_end")?;
    let section = column::<StringArray>(batch, "section")?;
    let line_start = column::<UInt32Array>(batch, "line_start")?;
    let line_end = column::<UInt32Array>(batch, "line_end")?;
    let byte_start = column::<UInt32Array>(batch, "byte_start")?;
    let byte_end = column::<UInt32Array>(batch, "byte_end")?;
    let text = column::<StringArray>(batch, "text")?;
    for i in 0..batch.num_rows() {
        out.push(DocChunkHit {
            doc_key: doc_key.value(i).to_string(),
            chunk_ord: chunk_ord.value(i),
            mime_type: mime_type.value(i).to_string(),
            chunk_kind: chunk_kind.value(i).to_string(),
            page_start: optional(page_start, i),
            page_end: optional(page_end, i),
            section: section.is_valid(i).then(|| section.value(i).to_string()),
            line_start: optional(line_start, i),
            line_end: optional(line_end, i),
            byte_start: byte_start.value(i),
            byte_end: byte_end.value(i),
            text: text.value(i).to_string(),
        });
    }
    Ok(())
}

/// Append the `docs` rows of `batch` to `out`.
pub fn decode_doc_meta(batch: &RecordBatch, out: &mut Vec<DocMeta>) -> Result<()> {
    let doc_key = column::<StringArray>(batch, "doc_key")?;
    let title = column::<StringArray>(batch, "title")?;
    let n_chunks = column::<UInt32Array>(batch, "n_chunks")?;
    let embedded = column::<BooleanArray>(batch, "embedded")?;
    for i in 0..batch.num_rows() {
        out.push(DocMeta {
            doc_key: doc_key.value(i).to_string(),
            title: title.is_valid(i).then(|| title.value(i).to_string()),
            n_chunks: n_chunks.value(i),
            embedded: embedded.value(i),
        });
    }
    Ok(())
}

/// The `doc_key` column of `batch`.
pub fn decode_keys(batch: &RecordBatch, out: &mut Vec<String>) -> Result<()> {
    let doc_key = column::<StringArray>(batch, "doc_key")?;
    out.extend((0..batch.num_rows()).map(|i| doc_key.value(i).to_string()));
    Ok(())
}

/// The `keywords` list of row `i`, for tests that read a `docs` row back.
#[cfg(test)]
pub fn keywords_of(batch: &RecordBatch, i: usize) -> Vec<String> {
    let Ok(list) = column::<ListArray>(batch, "keywords") else {
        return Vec::new();
    };
    let values = list.value(i);
    let Some(strings) = values.as_any().downcast_ref::<StringArray>() else {
        return Vec::new();
    };
    (0..strings.len()).map(|j| strings.value(j).to_string()).collect()
}

/// Whether row `i` of a `doc_chunks` batch carries a vector, for tests.
#[cfg(test)]
pub fn has_vector(batch: &RecordBatch, i: usize) -> bool {
    batch
        .column_by_name("embedding")
        .is_some_and(|column| column.is_valid(i))
}
