//! Deterministic synthetic corpus: every row is a pure function of `(seed, row index)`, so the
//! base corpus, the appended rows and the queries can be regenerated independently and in parallel.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use lancedb::arrow::arrow_array::builder::{
    FixedSizeListBuilder, Float32Builder, ListBuilder, StringBuilder, UInt32Builder,
};
use lancedb::arrow::arrow_array::{Array, FixedSizeListArray, Float32Array, RecordBatch, StringArray, UInt32Array};
use lancedb::arrow::arrow_ipc::reader::FileReader;
use lancedb::arrow::arrow_ipc::writer::FileWriter;
use lancedb::arrow::arrow_schema::{DataType, Field, Schema, SchemaRef};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// Rows per Arrow batch: the cap `src/lance/batch.rs` (`BATCH_MAX_ROWS`) puts on one write.
pub const BATCH_ROWS: usize = 4096;
pub const CHUNKS_PER_DOC: u64 = 20;
const ROWS_PER_CLUSTER: u64 = 1000;
const TOKENS_PER_CHUNK: usize = 120;
const CLUSTER_VOCAB: usize = 30;
const ZIPF_VOCAB: usize = 50_000;
const CLUSTER_TOKEN_PERCENT: u64 = 70;
/// Noise scale of a corpus vector around its cluster centre, and of a query around its source row.
/// Both multiply an isotropic Gaussian of expected norm 1 (per-dimension sigma `1/sqrt(dim)`).
const ROW_NOISE: f32 = 0.35;
const QUERY_NOISE: f32 = 0.25;
pub const QUERY_COUNT: usize = 200;
const QUERY_TOKENS: usize = 6;
/// Consonants only, without `s`, `d` or `y`: a Snowball English stemmer has no vowel to anchor a
/// suffix rule on, so Lance's stemming FTS index and arm C's plain tokenizer see the same terms.
const WORD_ALPHABET: &[u8; 12] = b"bcfgkmpqtvxz";
const WORD_LEN: usize = 5;

const STREAM_CLUSTER: u64 = 1;
const STREAM_CENTRE: u64 = 2;
const STREAM_VECTOR: u64 = 3;
const STREAM_TEXT: u64 = 4;
const STREAM_VOCAB: u64 = 5;
const STREAM_QUERY: u64 = 6;
const STREAM_LIVE: u64 = 7;
const STREAM_META: u64 = 8;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Params {
    pub n: u64,
    pub dim: usize,
    pub seed: u64,
}

impl Params {
    pub fn append_rows(&self) -> u64 {
        self.n / 100
    }

    pub fn raw_vector_bytes(&self) -> u64 {
        self.n * self.dim as u64 * 4
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("params.json");
        let body = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        Ok(serde_json::from_slice(&body)?)
    }
}

/// The LCG of `examples/doc_measure.rs`, seeded per `(seed, stream, index)`.
pub struct Lcg(u64);

impl Lcg {
    pub fn seeded(seed: u64, stream: u64, index: u64) -> Self {
        let mut z = seed
            ^ stream.wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ index.wrapping_mul(0xbf58_476d_1ce4_e5b9).rotate_left(31);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        Self(z ^ (z >> 31))
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    /// Uniform in `(0, 1]`.
    fn unit(&mut self) -> f64 {
        (self.next() + 1) as f64 / (1u64 << 31) as f64
    }

    /// Box-Muller; fills `out` with independent standard normals.
    fn fill_gaussian(&mut self, out: &mut [f32]) {
        for pair in out.chunks_mut(2) {
            let radius = (-2.0 * self.unit().ln()).sqrt();
            let angle = std::f64::consts::TAU * self.unit();
            pair[0] = (radius * angle.cos()) as f32;
            if let Some(second) = pair.get_mut(1) {
                *second = (radius * angle.sin()) as f32;
            }
        }
    }
}

fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

pub fn word(id: u32) -> String {
    let mut rest = id as usize;
    let mut out = [0u8; WORD_LEN];
    for slot in out.iter_mut().rev() {
        *slot = WORD_ALPHABET[rest % WORD_ALPHABET.len()];
        rest /= WORD_ALPHABET.len();
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Stands in for `blake3(content_hash ‖ doc_digest)`: 64 hex characters, as the design's key is.
pub fn doc_key(seed: u64, doc: u64) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&seed.to_le_bytes());
    hasher.update(&doc.to_le_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Whether `doc` is in the worktree manifest that marks `percent` of documents live.
pub fn is_live(seed: u64, doc: u64, percent: u64) -> bool {
    percent >= 100 || Lcg::seeded(seed, STREAM_LIVE, doc).next() % 100 < percent
}

/// `email` is exactly 1% of documents and `pdf` exactly 10%; the rest cycle through five groups.
pub fn mime_group(doc: u64) -> &'static str {
    if doc.is_multiple_of(100) {
        "email"
    } else if doc % 10 == 1 {
        "pdf"
    } else {
        ["markdown", "html", "office", "text", "notebook"][(doc % 5) as usize]
    }
}

fn mime_type(group: &str) -> &'static str {
    match group {
        "email" => "message/rfc822",
        "pdf" => "application/pdf",
        "markdown" => "text/markdown",
        "html" => "text/html",
        "office" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "notebook" => "application/x-ipynb+json",
        _ => "text/plain",
    }
}

/// The `doc_chunks` schema of design section 2.2.
pub fn chunk_schema(dim: usize) -> SchemaRef {
    let item = Arc::new(Field::new("item", DataType::Float32, true));
    let label = Arc::new(Field::new("item", DataType::Utf8, true));
    Arc::new(Schema::new(vec![
        Field::new("doc_key", DataType::Utf8, false),
        Field::new("chunk_ord", DataType::UInt32, false),
        Field::new("mime_type", DataType::Utf8, false),
        Field::new("mime_group", DataType::Utf8, false),
        Field::new("lang", DataType::Utf8, false),
        Field::new("page_start", DataType::UInt32, true),
        Field::new("page_end", DataType::UInt32, true),
        Field::new("section", DataType::Utf8, true),
        Field::new("chunk_kind", DataType::Utf8, false),
        Field::new("entity_cats", DataType::List(label), true),
        Field::new("byte_start", DataType::UInt32, false),
        Field::new("byte_end", DataType::UInt32, false),
        Field::new("text", DataType::Utf8, false),
        Field::new("embedding", DataType::FixedSizeList(item, dim as i32), true),
    ]))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    pub source_row: u64,
    pub text: String,
    pub vector: Vec<f32>,
}

pub struct Corpus {
    pub params: Params,
    clusters: u64,
    centres: Vec<Vec<f32>>,
    vocab: Vec<Vec<u32>>,
    zipf_cdf: Vec<f64>,
}

impl Corpus {
    pub fn new(params: Params) -> Self {
        let clusters = (params.n / ROWS_PER_CLUSTER).max(1);
        let centres = (0..clusters)
            .into_par_iter()
            .map(|c| {
                let mut v = vec![0.0; params.dim];
                Lcg::seeded(params.seed, STREAM_CENTRE, c).fill_gaussian(&mut v);
                normalize(&mut v);
                v
            })
            .collect();
        let vocab = (0..clusters)
            .map(|c| {
                let mut rng = Lcg::seeded(params.seed, STREAM_VOCAB, c);
                (0..CLUSTER_VOCAB)
                    .map(|_| (rng.next() % ZIPF_VOCAB as u64) as u32)
                    .collect()
            })
            .collect();
        let mut total = 0.0;
        let mut zipf_cdf: Vec<f64> = (1..=ZIPF_VOCAB)
            .map(|rank| {
                total += 1.0 / rank as f64;
                total
            })
            .collect();
        zipf_cdf.iter_mut().for_each(|x| *x /= total);
        Self {
            params,
            clusters,
            centres,
            vocab,
            zipf_cdf,
        }
    }

    fn cluster_of_doc(&self, doc: u64) -> usize {
        (Lcg::seeded(self.params.seed, STREAM_CLUSTER, doc).next() % self.clusters) as usize
    }

    fn noisy(&self, base: &[f32], scale: f32, rng: &mut Lcg) -> Vec<f32> {
        let mut v = vec![0.0; self.params.dim];
        rng.fill_gaussian(&mut v);
        let sigma = scale / (self.params.dim as f32).sqrt();
        for (x, b) in v.iter_mut().zip(base) {
            *x = b + sigma * *x;
        }
        normalize(&mut v);
        v
    }

    pub fn row_vector(&self, row: u64) -> Vec<f32> {
        let centre = &self.centres[self.cluster_of_doc(row / CHUNKS_PER_DOC)];
        self.noisy(
            centre,
            ROW_NOISE,
            &mut Lcg::seeded(self.params.seed, STREAM_VECTOR, row),
        )
    }

    fn row_text(&self, row: u64) -> String {
        let vocab = &self.vocab[self.cluster_of_doc(row / CHUNKS_PER_DOC)];
        let mut rng = Lcg::seeded(self.params.seed, STREAM_TEXT, row);
        let mut out = String::with_capacity(TOKENS_PER_CHUNK * (WORD_LEN + 1));
        for i in 0..TOKENS_PER_CHUNK {
            if i > 0 {
                out.push(' ');
            }
            let id = if rng.next() % 100 < CLUSTER_TOKEN_PERCENT {
                vocab[(rng.next() as usize) % vocab.len()]
            } else {
                let u = rng.unit();
                self.zipf_cdf.partition_point(|c| *c < u).min(ZIPF_VOCAB - 1) as u32
            };
            out.push_str(&word(id));
        }
        out
    }

    /// Rows `[start, start + len)` as one batch of the target schema.
    pub fn batch(&self, start: u64, len: usize) -> Result<RecordBatch> {
        let seed = self.params.seed;
        let generated: Vec<(Vec<f32>, String)> = (start..start + len as u64)
            .into_par_iter()
            .map(|row| (self.row_vector(row), self.row_text(row)))
            .collect();
        let mut key = StringBuilder::new();
        let mut ord = UInt32Builder::new();
        let mut mime = StringBuilder::new();
        let mut group = StringBuilder::new();
        let mut lang = StringBuilder::new();
        let mut page_start = UInt32Builder::new();
        let mut page_end = UInt32Builder::new();
        let mut section = StringBuilder::new();
        let mut kind = StringBuilder::new();
        let mut cats = ListBuilder::new(StringBuilder::new());
        let mut byte_start = UInt32Builder::new();
        let mut byte_end = UInt32Builder::new();
        let mut text = StringBuilder::new();
        let mut embedding = FixedSizeListBuilder::new(Float32Builder::new(), self.params.dim as i32);
        for (offset, (vector, chunk_text)) in generated.iter().enumerate() {
            let row = start + offset as u64;
            let (doc, chunk) = (row / CHUNKS_PER_DOC, (row % CHUNKS_PER_DOC) as u32);
            let mut rng = Lcg::seeded(seed, STREAM_META, row);
            let doc_group = mime_group(doc);
            key.append_value(doc_key(seed, doc));
            ord.append_value(chunk);
            mime.append_value(mime_type(doc_group));
            group.append_value(doc_group);
            lang.append_value(["en", "en", "en", "en", "de", "fr"][(doc % 6) as usize]);
            if doc_group == "pdf" {
                page_start.append_value(chunk / 2 + 1);
                page_end.append_value(chunk / 2 + 1 + chunk % 2);
            } else {
                page_start.append_null();
                page_end.append_null();
            }
            section.append_value(format!("Part {} > Section {}", doc % 7, chunk / 5));
            kind.append_value("text");
            for label in ["person", "organization", "date"] {
                if rng.next().is_multiple_of(4) {
                    cats.values().append_value(label);
                }
            }
            cats.append(true);
            byte_start.append_value(chunk * 900);
            byte_end.append_value(chunk * 900 + chunk_text.len() as u32);
            text.append_value(chunk_text);
            embedding.values().append_slice(vector);
            embedding.append(true);
        }
        RecordBatch::try_new(
            chunk_schema(self.params.dim),
            vec![
                Arc::new(key.finish()),
                Arc::new(ord.finish()),
                Arc::new(mime.finish()),
                Arc::new(group.finish()),
                Arc::new(lang.finish()),
                Arc::new(page_start.finish()),
                Arc::new(page_end.finish()),
                Arc::new(section.finish()),
                Arc::new(kind.finish()),
                Arc::new(cats.finish()),
                Arc::new(byte_start.finish()),
                Arc::new(byte_end.finish()),
                Arc::new(text.finish()),
                Arc::new(embedding.finish()),
            ],
        )
        .context("assemble corpus batch")
    }

    pub fn queries(&self) -> Vec<Query> {
        (0..QUERY_COUNT as u64)
            .map(|i| {
                let mut rng = Lcg::seeded(self.params.seed, STREAM_QUERY, i);
                let source_row = rng.next() % self.params.n;
                let vector = self.noisy(&self.row_vector(source_row), QUERY_NOISE, &mut rng);
                let vocab = &self.vocab[self.cluster_of_doc(source_row / CHUNKS_PER_DOC)];
                let text = (0..QUERY_TOKENS)
                    .map(|_| word(vocab[(rng.next() as usize) % vocab.len()]))
                    .collect::<Vec<_>>()
                    .join(" ");
                Query {
                    source_row,
                    text,
                    vector,
                }
            })
            .collect()
    }

    /// Write rows `[start, start + rows)` to `path` as an Arrow IPC file of `BATCH_ROWS` batches.
    pub fn write_ipc(&self, path: &Path, start: u64, rows: u64) -> Result<()> {
        let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
        let mut writer = FileWriter::try_new(BufWriter::new(file), &chunk_schema(self.params.dim))?;
        let mut at = start;
        while at < start + rows {
            let len = BATCH_ROWS.min((start + rows - at) as usize);
            writer.write(&self.batch(at, len)?)?;
            at += len as u64;
        }
        writer.finish()?;
        Ok(())
    }
}

pub fn open_ipc(path: &Path) -> Result<FileReader<BufReader<File>>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    Ok(FileReader::try_new(BufReader::new(file), None)?)
}

/// Borrowed columns of one corpus batch, for the consumers that do not hand batches to Lance.
pub struct Columns<'a> {
    pub doc_key: &'a StringArray,
    pub chunk_ord: &'a UInt32Array,
    pub mime_group: &'a StringArray,
    pub text: &'a StringArray,
    pub vectors: &'a [f32],
}

impl<'a> Columns<'a> {
    pub fn of(batch: &'a RecordBatch) -> Result<Self> {
        fn column<'b, T: 'static>(batch: &'b RecordBatch, name: &str) -> Result<&'b T> {
            batch
                .column_by_name(name)
                .and_then(|c| c.as_any().downcast_ref::<T>())
                .with_context(|| format!("column `{name}` missing or of an unexpected type"))
        }
        let embedding: &FixedSizeListArray = column(batch, "embedding")?;
        let values = embedding
            .values()
            .as_any()
            .downcast_ref::<Float32Array>()
            .context("embedding values are not Float32")?;
        Ok(Self {
            doc_key: column(batch, "doc_key")?,
            chunk_ord: column(batch, "chunk_ord")?,
            mime_group: column(batch, "mime_group")?,
            text: column(batch, "text")?,
            vectors: values.values(),
        })
    }
}
