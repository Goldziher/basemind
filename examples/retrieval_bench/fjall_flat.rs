//! Arm C: nothing linked that basemind does not already link. Fjall holds chunk rows, the document
//! dictionary and BM25 postings keyed `term ‖ 0 ‖ u32 doc ‖ u16 ord`; vectors live in two flat
//! memory-mapped files (SQ8 for the exact scan, f32 for rescoring). There is no ANN.

use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use ahash::AHashMap;
use anyhow::{Context, Result, bail};
use fjall::config::CompressionPolicy;
use fjall::{CompressionType, Database, Keyspace, KeyspaceCreateOptions, PersistMode};
use lancedb::arrow::arrow_array::RecordBatch;
use memmap2::Mmap;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, Filter, Hit, VectorMode};
use crate::corpus::Columns;

const GROUPS: [&str; 7] = ["email", "pdf", "markdown", "html", "office", "text", "notebook"];
const UNKNOWN_GROUP: u8 = u8::MAX;
/// Bytes per row in `rids.bin`: `u32 doc ‖ u16 ord`.
const RID_BYTES: usize = 6;
/// Bytes per document in `doc_keys.bin`: the raw 32-byte key.
const KEY_BYTES: usize = 32;
/// Rows one rayon task scans; small enough to balance, large enough to amortise the task.
const SCAN_BLOCK: usize = 4096;
const CACHE_BYTES: u64 = 256 * 1024 * 1024;
pub const BM25_K1: f32 = 1.2;
pub const BM25_B: f32 = 0.75;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Meta {
    dim: usize,
    rows: u64,
    docs: u32,
    total_tokens: u64,
    /// SQ8 range: a component `x` is stored as `round((x - lo) / (hi - lo) * 255)`.
    lo: f32,
    hi: f32,
}

pub fn group_code(group: &str) -> u8 {
    GROUPS
        .iter()
        .position(|known| *known == group)
        .map_or(UNKNOWN_GROUP, |at| at as u8)
}

/// Lower-cased alphanumeric runs; no stemming (the corpus vocabulary is stem-neutral).
pub fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
}

/// Lance's BM25 (`lance-index-14.0.2/src/scalar/inverted/scorer.rs`): `idf * (K1+1)*tf / (tf + norm)`.
pub fn bm25(tf: f32, doc_len: f32, avg_len: f32, df: usize, docs: u64) -> f32 {
    let idf = ((docs as f32 - df as f32 + 0.5) / (df as f32 + 0.5) + 1.0).ln();
    let norm = BM25_K1 * (1.0 - BM25_B + BM25_B * doc_len / avg_len);
    idf * (BM25_K1 + 1.0) * tf / (tf + norm)
}

fn l2_u8(a: &[u8], b: &[u8]) -> u32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let d = i32::from(*x) - i32::from(*y);
            (d * d) as u32
        })
        .sum()
}

fn posting_prefix(term: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(term.len() + 7);
    key.extend_from_slice(term.as_bytes());
    key.push(0);
    key
}

struct Maps {
    sq8: Mmap,
    f32s: Mmap,
    attrs: Mmap,
    rids: Mmap,
    doc_keys: Mmap,
}

pub struct FjallFlat {
    dir: PathBuf,
    db: Database,
    rows: Keyspace,
    postings: Keyspace,
    docs: Keyspace,
    meta: Meta,
    maps: Option<Maps>,
    /// SQ8 candidates rescored against f32 per requested hit.
    rescore_factor: usize,
}

fn map(path: &Path) -> Result<Mmap> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    // SAFETY: the harness runs one process per measurement and never writes a file it has mapped:
    // `build` and `append` only write, `query` only reads.
    Ok(unsafe { Mmap::map(&file) }?)
}

fn appender(path: &Path) -> Result<BufWriter<File>> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {} for append", path.display()))?;
    Ok(BufWriter::with_capacity(1 << 20, file))
}

impl FjallFlat {
    pub fn open(dir: &Path, dim: usize, rescore_factor: usize) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let db = Database::builder(dir.join("fjall")).cache_size(CACHE_BYTES).open()?;
        let options = || {
            KeyspaceCreateOptions::default()
                .data_block_compression_policy(CompressionPolicy::all(CompressionType::Lz4))
                .index_block_compression_policy(CompressionPolicy::all(CompressionType::Lz4))
        };
        let rows = db.keyspace("rows", options)?;
        let postings = db.keyspace("postings", options)?;
        let docs = db.keyspace("docs", options)?;
        let meta_path = dir.join("meta.json");
        let meta: Meta = if meta_path.exists() {
            serde_json::from_slice(&std::fs::read(&meta_path)?)?
        } else {
            Meta { dim, ..Meta::default() }
        };
        if meta.dim != dim {
            bail!("store dim {} does not match corpus dim {dim}", meta.dim);
        }
        let maps = if meta.rows > 0 {
            Some(Maps {
                sq8: map(&dir.join("vectors.sq8"))?,
                f32s: map(&dir.join("vectors.f32"))?,
                attrs: map(&dir.join("attrs.bin"))?,
                rids: map(&dir.join("rids.bin"))?,
                doc_keys: map(&dir.join("doc_keys.bin"))?,
            })
        } else {
            None
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            db,
            rows,
            postings,
            docs,
            meta,
            maps,
            rescore_factor,
        })
    }

    fn quantize(&self, x: f32) -> u8 {
        ((x - self.meta.lo) / (self.meta.hi - self.meta.lo) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8
    }

    fn doc_id(&mut self, key: &str, new_keys: &mut Vec<u8>) -> Result<u32> {
        if let Some(found) = self.docs.get(key)? {
            return Ok(u32::from_be_bytes(found.as_ref().try_into().context("doc id width")?));
        }
        let id = self.meta.docs;
        self.meta.docs += 1;
        self.docs.insert(key, id.to_be_bytes())?;
        let raw = blake3::Hash::from_hex(key).context("doc_key is not 64 hex characters")?;
        new_keys.extend_from_slice(raw.as_bytes());
        Ok(id)
    }

    fn ingest(&mut self, rows: &mut dyn Iterator<Item = Result<RecordBatch>>) -> Result<()> {
        // A mapping must not outlive a write to its file.
        self.maps = None;
        let mut f32s = appender(&self.dir.join("vectors.f32"))?;
        let mut sq8 = appender(&self.dir.join("vectors.sq8"))?;
        let mut attrs = appender(&self.dir.join("attrs.bin"))?;
        let mut rids = appender(&self.dir.join("rids.bin"))?;
        let mut doc_keys = appender(&self.dir.join("doc_keys.bin"))?;
        let mut last_doc: Option<(String, u32)> = None;
        for batch in rows {
            let batch = batch?;
            let columns = Columns::of(&batch)?;
            if self.meta.rows == 0 {
                let (lo, hi) = columns
                    .vectors
                    .iter()
                    .fold((f32::MAX, f32::MIN), |(lo, hi), x| (lo.min(*x), hi.max(*x)));
                let margin = (hi - lo) * 0.05;
                (self.meta.lo, self.meta.hi) = (lo - margin, hi + margin);
            }
            let mut write = self.db.batch();
            let mut new_keys = Vec::new();
            for i in 0..batch.num_rows() {
                let key = columns.doc_key.value(i);
                let doc = match &last_doc {
                    Some((known, id)) if known == key => *id,
                    _ => {
                        let id = self.doc_id(key, &mut new_keys)?;
                        last_doc = Some((key.to_string(), id));
                        id
                    }
                };
                let ord = u16::try_from(columns.chunk_ord.value(i)).context("chunk ordinal exceeds u16")?;
                let rid = u32::try_from(self.meta.rows).context("row id exceeds u32")?;
                let mut id_bytes = [0u8; RID_BYTES];
                id_bytes[..4].copy_from_slice(&doc.to_be_bytes());
                id_bytes[4..].copy_from_slice(&ord.to_be_bytes());
                rids.write_all(&id_bytes)?;
                attrs.write_all(&[group_code(columns.mime_group.value(i))])?;
                let vector = &columns.vectors[i * self.meta.dim..(i + 1) * self.meta.dim];
                for x in vector {
                    f32s.write_all(&x.to_le_bytes())?;
                }
                let codes: Vec<u8> = vector.iter().map(|x| self.quantize(*x)).collect();
                sq8.write_all(&codes)?;

                let text = columns.text.value(i);
                let mut tf: AHashMap<String, u16> = AHashMap::new();
                let mut doc_len = 0u16;
                for token in tokens(text) {
                    *tf.entry(token).or_default() += 1;
                    doc_len = doc_len.saturating_add(1);
                }
                for (term, count) in tf {
                    let mut posting = posting_prefix(&term);
                    posting.extend_from_slice(&id_bytes);
                    let mut value = [0u8; 8];
                    value[..4].copy_from_slice(&rid.to_le_bytes());
                    value[4..6].copy_from_slice(&count.to_le_bytes());
                    value[6..].copy_from_slice(&doc_len.to_le_bytes());
                    write.insert(&self.postings, posting, value);
                }
                write.insert(&self.rows, id_bytes, text.as_bytes());
                self.meta.total_tokens += u64::from(doc_len);
                self.meta.rows += 1;
            }
            write.commit()?;
            doc_keys.write_all(&new_keys)?;
        }
        for mut file in [f32s, sq8, attrs, rids, doc_keys] {
            file.flush()?;
            file.get_ref().sync_all()?;
        }
        self.db.persist(PersistMode::SyncAll)?;
        std::fs::write(self.dir.join("meta.json"), serde_json::to_vec(&self.meta)?)?;
        Ok(())
    }

    fn maps(&self) -> Result<&Maps> {
        self.maps
            .as_ref()
            .context("the store is empty or was written in this process; reopen it to query")
    }

    /// The pre-filter as a row test: the group code, and the live-key list resolved through the
    /// document dictionary into a per-document bitmap (one point read per key, per query).
    fn row_filter<'a>(&self, maps: &'a Maps, filter: &Filter) -> Result<impl Fn(usize) -> bool + Sync + 'a> {
        let wanted = filter.group.map(group_code);
        let live = match &filter.keys {
            Some(keys) => {
                let mut live = vec![false; self.meta.docs as usize];
                for key in keys.iter() {
                    if let Some(found) = self.docs.get(key)? {
                        let id = u32::from_be_bytes(found.as_ref().try_into().context("doc id width")?);
                        live[id as usize] = true;
                    }
                }
                Some(live)
            }
            None => None,
        };
        Ok(move |rid: usize| {
            if wanted.is_some_and(|code| maps.attrs[rid] != code) {
                return false;
            }
            live.as_ref().is_none_or(|live| {
                let at = rid * RID_BYTES;
                live[u32::from_be_bytes(maps.rids[at..at + 4].try_into().expect("4 bytes")) as usize]
            })
        })
    }

    fn hit(&self, maps: &Maps, rid: u32) -> Hit {
        let at = rid as usize * RID_BYTES;
        let doc = u32::from_be_bytes(maps.rids[at..at + 4].try_into().expect("4 bytes")) as usize;
        let ord = u16::from_be_bytes(maps.rids[at + 4..at + RID_BYTES].try_into().expect("2 bytes"));
        let key: [u8; KEY_BYTES] = maps.doc_keys[doc * KEY_BYTES..(doc + 1) * KEY_BYTES]
            .try_into()
            .expect("32 bytes");
        Hit {
            doc_key: blake3::Hash::from_bytes(key).to_hex().to_string(),
            ord: u32::from(ord),
        }
    }
}

impl Backend for FjallFlat {
    fn build(&mut self, rows: &mut dyn Iterator<Item = Result<RecordBatch>>) -> Result<()> {
        self.ingest(rows)
    }

    /// Postings and SQ8 codes are written at ingest; what is left is settling the LSM tree.
    fn build_indexes(&mut self) -> Result<()> {
        self.postings.major_compact()?;
        self.rows.major_compact()?;
        self.db.persist(PersistMode::SyncAll)?;
        Ok(())
    }

    fn vector(&self, query: &[f32], filter: &Filter, k: usize) -> Result<(Vec<Hit>, VectorMode)> {
        let maps = self.maps()?;
        let dim = self.meta.dim;
        let keep_row = self.row_filter(maps, filter)?;
        let codes: Vec<u8> = query.iter().map(|x| self.quantize(*x)).collect();
        let keep = k * self.rescore_factor.max(1);
        let rows = self.meta.rows as usize;
        let blocks = rows.div_ceil(SCAN_BLOCK);
        let candidates = (0..blocks)
            .into_par_iter()
            .fold(
                || BinaryHeap::with_capacity(keep + 1),
                |mut heap: BinaryHeap<(u32, u32)>, block| {
                    let end = ((block + 1) * SCAN_BLOCK).min(rows);
                    for rid in block * SCAN_BLOCK..end {
                        if !keep_row(rid) {
                            continue;
                        }
                        let distance = l2_u8(&codes, &maps.sq8[rid * dim..(rid + 1) * dim]);
                        if heap.len() < keep {
                            heap.push((distance, rid as u32));
                        } else if heap.peek().is_some_and(|worst| distance < worst.0) {
                            heap.pop();
                            heap.push((distance, rid as u32));
                        }
                    }
                    heap
                },
            )
            .reduce(BinaryHeap::new, |mut a, b| {
                a.extend(b);
                a
            });
        let mut shortlist = candidates.into_sorted_vec();
        shortlist.truncate(keep);
        let mut exact: Vec<(f32, u32)> = shortlist
            .into_iter()
            .map(|(_, rid)| {
                let bytes = &maps.f32s[rid as usize * dim * 4..(rid as usize + 1) * dim * 4];
                let distance = bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(query)
                    .map(|(raw, q)| {
                        let d = f32::from_le_bytes(*raw) - q;
                        d * d
                    })
                    .sum::<f32>();
                (distance, rid)
            })
            .collect();
        exact.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        exact.truncate(k);
        Ok((
            exact.into_iter().map(|(_, rid)| self.hit(maps, rid)).collect(),
            VectorMode::Exact,
        ))
    }

    fn lexical(&self, text: &str, filter: &Filter, k: usize) -> Result<Option<Vec<Hit>>> {
        let maps = self.maps()?;
        let keep_row = self.row_filter(maps, filter)?;
        let avg_len = self.meta.total_tokens as f32 / self.meta.rows.max(1) as f32;
        let mut terms: Vec<String> = tokens(text).collect();
        terms.sort_unstable();
        terms.dedup();
        let mut scores: AHashMap<u32, f32> = AHashMap::new();
        let mut postings: Vec<(u32, f32, f32)> = Vec::new();
        for term in &terms {
            postings.clear();
            for guard in self.postings.prefix(posting_prefix(term)) {
                let (_, value) = guard.into_inner()?;
                let value: [u8; 8] = value.as_ref().try_into().context("posting value width")?;
                postings.push((
                    u32::from_le_bytes(value[..4].try_into().expect("4 bytes")),
                    f32::from(u16::from_le_bytes(value[4..6].try_into().expect("2 bytes"))),
                    f32::from(u16::from_le_bytes(value[6..].try_into().expect("2 bytes"))),
                ));
            }
            let df = postings.len();
            for (rid, tf, doc_len) in &postings {
                if !keep_row(*rid as usize) {
                    continue;
                }
                *scores.entry(*rid).or_default() += bm25(*tf, *doc_len, avg_len, df, self.meta.rows);
            }
        }
        let mut ranked: Vec<(u32, f32)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked.truncate(k);
        Ok(Some(ranked.into_iter().map(|(rid, _)| self.hit(maps, rid)).collect()))
    }

    fn append(&mut self, rows: &mut dyn Iterator<Item = Result<RecordBatch>>) -> Result<()> {
        self.ingest(rows)
    }

    /// Appended rows are already in the postings and the scan files; nothing is left to fold in.
    fn optimize(&mut self) -> Result<()> {
        self.db.persist(PersistMode::SyncAll)?;
        Ok(())
    }
}
