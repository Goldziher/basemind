//! The document retrieval store (ADR-0029): `doc_chunks` and `docs`, keyed by `doc_key`, in one
//! LanceDB directory per embedding model and dimension.
//!
//! The directory is the repository group's (ADR-0029 rule 2, ADR-0037 rule 3): every worktree of
//! a repository shares one store, [`group_store_dir`], since rows carry no path, scope or workspace.
//! A view reads only the keys its own document map names, and a row is deleted only when no view
//! of the group names its key.

use std::path::{Path, PathBuf};

use ahash::{AHashMap, AHashSet};
use anyhow::{Context, Result};
use futures::TryStreamExt;
use lancedb::DistanceType;
use lancedb::expr::{col, is_in, lit};
use lancedb::index::Index;
use lancedb::index::scalar::{FtsIndexBuilder, FullTextSearchQuery};
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::table::{OptimizeAction, OptimizeOptions, Table};

use super::docs_schema::{
    DOC_CHUNKS_TABLE, DOCS_TABLE, DocChunkHit, DocMeta, DocRows, HIT_COLUMNS, chunks_batch, decode_chunk_hits,
    decode_doc_meta, decode_keys, doc_chunks_schema, docs_schema,
};
use super::{LanceStore, escape_sql_literal};

pub use crate::store::doc_key;

/// Keys per `doc_key IN (...)` statement of a write or a lookup.
const KEYS_PER_STATEMENT: usize = 2_000;

/// Directory of a repository group's retrieval stores, under the group directory
/// (`cache/groups/<group>/lance/`).
pub const GROUP_STORE_DIR: &str = "lance";

/// The directory of the retrieval store for `model` at `dim`, under `parent`. The name ends in a
/// hash of the model's own name, so two models whose readable slug collides never share, and wipe,
/// one directory.
pub fn store_dir(parent: &Path, model: &str, dim: u16) -> PathBuf {
    let model = crate::embeddings::model_key(model);
    let slug: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let hash = crate::hashing::hex(&crate::hashing::hash_bytes(model.as_bytes()));
    parent.join(format!("{slug}-{dim}-{}", &hash[..8]))
}

/// The retrieval store of `root`'s repository group for `model` at `dim`:
/// `cache/groups/<group>/lance/<model>-<dim>-<hash>/`. Shared by every worktree of the group.
pub fn group_store_dir(root: &Path, model: &str, dim: u16) -> PathBuf {
    store_dir(
        &crate::store_layout::group_index_dir(root).join(GROUP_STORE_DIR),
        model,
        dim,
    )
}

/// The coarse kind of a MIME type, the `kind` filter of `docs_search`.
pub fn mime_group(mime: &str) -> &'static str {
    let mime = mime.to_ascii_lowercase();
    let mime = mime.split(';').next().unwrap_or_default().trim();
    if mime == "application/pdf" {
        "pdf"
    } else if mime.contains("markdown") || mime == "text/mdx" || mime == "text/x-rst" || mime.contains("rst") {
        "markdown"
    } else if mime == "text/html" || mime == "application/xhtml+xml" {
        "html"
    } else if mime == "message/rfc822" || mime.contains("outlook") || mime == "application/mbox" {
        "email"
    } else if mime.contains("ipynb") {
        "notebook"
    } else if mime.contains("officedocument")
        || mime.contains("opendocument")
        || mime.contains("msword")
        || mime.contains("ms-excel")
        || mime.contains("ms-powerpoint")
        || mime == "application/rtf"
        || mime == "text/rtf"
    {
        "office"
    } else if mime.starts_with("text/") {
        "text"
    } else {
        "other"
    }
}

/// The values [`mime_group`] returns, which `kind` accepts.
pub const MIME_GROUPS: &[&str] = &[
    "pdf", "office", "html", "markdown", "email", "notebook", "text", "other",
];

/// A filter both lanes apply before they rank (ADR-0029 rule 5). `keys` is the allowed `doc_key`
/// set when it is small enough to send to the store; `None` leaves liveness to the caller.
#[derive(Debug, Clone, Default)]
pub struct DocPredicate {
    pub mime_type: Option<String>,
    pub mime_group: Option<String>,
    /// A lower-cased entity category a chunk must name.
    pub entity: Option<String>,
    pub keys: Option<Vec<String>>,
}

impl DocPredicate {
    /// The metadata half as SQL, or `None` when there is none.
    pub fn sql(&self) -> Option<String> {
        let mut clauses = Vec::new();
        if let Some(mime) = &self.mime_type {
            clauses.push(format!("mime_type = '{}'", escape_sql_literal(mime)));
        }
        if let Some(group) = &self.mime_group {
            clauses.push(format!("mime_group = '{}'", escape_sql_literal(group)));
        }
        if let Some(entity) = &self.entity {
            clauses.push(format!(
                "array_has_any(entity_cats, ['{}'])",
                escape_sql_literal(entity)
            ));
        }
        (!clauses.is_empty()).then(|| clauses.join(" AND "))
    }

    fn apply<Q: QueryBase>(&self, mut query: Q) -> Q {
        if let Some(sql) = self.sql() {
            query = query.only_if(sql);
        }
        if let Some(keys) = &self.keys {
            // An `InList` expression, not SQL text: a list of thousands of keys is not re-parsed,
            // and with the BTree on `doc_key` it is the cheaper form (benchmarks/retrieval/PROBES.md).
            let list = keys.iter().map(|key| lit(key.as_str())).collect();
            query = query.only_if_expr(is_in(col("doc_key"), list));
        }
        query
    }

    /// Whether the allowed key set is known to be empty, so no lane needs to run.
    pub fn selects_nothing(&self) -> bool {
        self.keys.as_ref().is_some_and(Vec::is_empty)
    }
}

/// The indexes of the two tables: `(table, name, column)`. Built once rows exist, kept current by
/// [`DocStore::maintain_indexes`].
const INDEXES: &[(&str, &str, &str)] = &[
    (DOC_CHUNKS_TABLE, "doc_key_idx", "doc_key"),
    (DOC_CHUNKS_TABLE, "mime_type_idx", "mime_type"),
    (DOC_CHUNKS_TABLE, "mime_group_idx", "mime_group"),
    (DOC_CHUNKS_TABLE, "entity_cats_idx", "entity_cats"),
    (DOC_CHUNKS_TABLE, "text_idx", "text"),
    (DOCS_TABLE, "docs_doc_key_idx", "doc_key"),
    (DOCS_TABLE, "keywords_idx", "keywords"),
];

fn index_for(column: &str) -> Index {
    match column {
        "doc_key" => Index::BTree(Default::default()),
        "mime_type" | "mime_group" => Index::Bitmap(Default::default()),
        "entity_cats" | "keywords" => Index::LabelList(Default::default()),
        // Lance's defaults are the analyzer ADR-0029 rule 4 names: simple tokenizer, lower case,
        // ASCII folding, English stemming, stop words removed, no positions.
        _ => Index::FTS(FtsIndexBuilder::default()),
    }
}

/// The retrieval store. Cheap to clone.
#[derive(Clone)]
pub struct DocStore {
    lance: LanceStore,
}

impl DocStore {
    /// Open (or create) the store at `dir` for vectors of `model` at `dim`. A store of another
    /// model, dimension or table version is wiped; one of a newer version is refused.
    pub fn open(dir: &Path, dim: u16, model: &str) -> Result<Self> {
        let lance = LanceStore::open_tables(
            dir,
            dim,
            model,
            &[(DOC_CHUNKS_TABLE, doc_chunks_schema(dim)), (DOCS_TABLE, docs_schema())],
        )?;
        Ok(Self { lance })
    }

    /// The process's handle on the store at `dir`, opening it on first use. A scan and a search in
    /// one process share it. A handle whose directory was removed since is dropped and the store
    /// opened again.
    pub fn shared(dir: &Path, dim: u16, model: &str) -> Result<Self> {
        static HANDLES: std::sync::OnceLock<std::sync::Mutex<AHashMap<PathBuf, DocStore>>> = std::sync::OnceLock::new();
        let handles = HANDLES.get_or_init(Default::default);
        let mut handles = handles.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(store) = handles.get(dir)
            && store.dim() == dim
            && store.lance.embedding_model() == crate::embeddings::model_key(model)
            && dir.join(super::META_FILE).exists()
        {
            return Ok(store.clone());
        }
        let store = Self::open(dir, dim, model)?;
        handles.insert(dir.to_path_buf(), store.clone());
        Ok(store)
    }

    /// [`Self::shared`] when a store exists at `dir`; `None` when none was ever written there.
    pub fn shared_existing(dir: &Path, dim: u16, model: &str) -> Result<Option<Self>> {
        if !dir.join(super::META_FILE).exists() {
            return Ok(None);
        }
        Self::shared(dir, dim, model).map(Some)
    }

    pub fn dim(&self) -> u16 {
        self.lance.dim()
    }

    pub fn dir(&self) -> &Path {
        self.lance.dir()
    }

    /// The store's id (see [`super::store_id`]).
    pub fn id(&self) -> Option<String> {
        super::store_id(self.lance.dir())
    }

    fn block_on<T>(&self, work: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        self.lance.inner.rt().block_on(work)
    }

    async fn table(&self, name: &str) -> Result<Table> {
        self.lance
            .inner
            .connection
            .open_table(name)
            .execute()
            .await
            .with_context(|| format!("open {name} table"))
    }

    /// Which of `keys` have a complete set of rows, and whether those rows carry vectors.
    pub fn present(&self, keys: &[String]) -> Result<AHashMap<String, bool>> {
        let mut out = AHashMap::with_capacity(keys.len());
        if keys.is_empty() {
            return Ok(out);
        }
        self.block_on(async {
            let table = self.table(DOCS_TABLE).await?;
            for part in keys.chunks(KEYS_PER_STATEMENT) {
                let list = part.iter().map(|key| lit(key.as_str())).collect();
                let mut stream = table
                    .query()
                    .only_if_expr(is_in(col("doc_key"), list))
                    .select(Select::columns(&["doc_key", "title", "n_chunks", "embedded"]))
                    .execute()
                    .await
                    .context("look up present documents")?;
                let mut metas = Vec::new();
                while let Some(batch) = stream.try_next().await.context("read present documents")? {
                    decode_doc_meta(&batch, &mut metas)?;
                }
                out.extend(metas.into_iter().map(|meta| (meta.doc_key, meta.embedded)));
            }
            Ok(out)
        })
    }

    /// Every `doc_key` the store holds a document row for.
    pub fn keys(&self) -> Result<Vec<String>> {
        self.block_on(async {
            let table = self.table(DOCS_TABLE).await?;
            let mut stream = table
                .query()
                .select(Select::columns(&["doc_key"]))
                .execute()
                .await
                .context("list documents")?;
            let mut keys = Vec::new();
            while let Some(batch) = stream.try_next().await.context("read documents")? {
                decode_keys(&batch, &mut keys)?;
            }
            Ok(keys)
        })
    }

    /// Write the rows of `docs`, replacing any rows of the same keys. The chunk rows commit before
    /// the document rows, so a document row is only ever seen beside its complete chunk rows: a
    /// write cut short between the two leaves the key absent, and the next scan writes it again.
    pub fn write(&self, docs: &[DocRows]) -> Result<()> {
        if docs.is_empty() {
            return Ok(());
        }
        let keys: Vec<String> = docs.iter().map(|doc| doc.doc_key.clone()).collect();
        self.delete_keys(&keys)?;
        let dim = self.lance.dim();
        self.block_on(async {
            if docs.iter().any(|doc| !doc.chunks.is_empty()) {
                let chunks = self.table(DOC_CHUNKS_TABLE).await?;
                chunks
                    .add(chunks_batch(dim, docs)?)
                    .execute()
                    .await
                    .context("insert doc_chunks rows")?;
                self.lance.after_commit(&chunks, DOC_CHUNKS_TABLE).await;
            }
            let table = self.table(DOCS_TABLE).await?;
            table
                .add(super::docs_schema::docs_batch(docs)?)
                .execute()
                .await
                .context("insert docs rows")?;
            self.lance.after_commit(&table, DOCS_TABLE).await;
            Ok(())
        })
    }

    /// Delete every row of `keys`, document rows first, so a key is never present without its
    /// chunk rows.
    pub fn delete_keys(&self, keys: &[String]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        self.block_on(async {
            for name in [DOCS_TABLE, DOC_CHUNKS_TABLE] {
                let table = self.table(name).await?;
                if table.count_rows(None).await.with_context(|| format!("count {name}"))? == 0 {
                    continue;
                }
                for part in keys.chunks(KEYS_PER_STATEMENT) {
                    let list: Vec<String> = part
                        .iter()
                        .map(|key| format!("'{}'", escape_sql_literal(key)))
                        .collect();
                    table
                        .delete(&format!("doc_key IN ({})", list.join(", ")))
                        .await
                        .with_context(|| format!("delete {} keys from {name}", part.len()))?;
                    self.lance.after_commit(&table, name).await;
                }
            }
            Ok(())
        })
    }

    /// Delete the rows of every key not in `live`. Returns how many documents went. This is the
    /// store's half of the dead-row sweep: the caller says what is live, the store lists nothing
    /// else (ADR-0029 rule 11).
    pub fn delete_dead(&self, live: &AHashSet<String>) -> Result<usize> {
        let dead: Vec<String> = self.keys()?.into_iter().filter(|key| !live.contains(key)).collect();
        // Chunk rows whose document row was never written (a cut-short write) are dead as well.
        let orphans = self.orphan_chunk_keys(live)?;
        let mut all = dead.clone();
        all.extend(orphans);
        all.sort_unstable();
        all.dedup();
        self.delete_keys(&all)?;
        Ok(dead.len())
    }

    fn orphan_chunk_keys(&self, live: &AHashSet<String>) -> Result<Vec<String>> {
        let present: AHashSet<String> = self.keys()?.into_iter().collect();
        self.block_on(async {
            let table = self.table(DOC_CHUNKS_TABLE).await?;
            let mut stream = table
                .query()
                .only_if("chunk_ord = 0")
                .select(Select::columns(&["doc_key"]))
                .execute()
                .await
                .context("list chunk keys")?;
            let mut keys = Vec::new();
            while let Some(batch) = stream.try_next().await.context("read chunk keys")? {
                decode_keys(&batch, &mut keys)?;
            }
            Ok(keys
                .into_iter()
                .filter(|key| !present.contains(key) && !live.contains(key))
                .collect())
        })
    }

    /// Build every missing index and bring the existing ones up to date with appended rows. An
    /// index on an empty table is left for the scan that first writes rows. Run at the end of a
    /// scan (ADR-0029 consequences: "index upkeep is added to the end of a scan").
    pub fn maintain_indexes(&self) -> Result<()> {
        self.block_on(async {
            for name in [DOC_CHUNKS_TABLE, DOCS_TABLE] {
                let table = self.table(name).await?;
                if table.count_rows(None).await.with_context(|| format!("count {name}"))? == 0 {
                    continue;
                }
                let existing: AHashSet<String> = table
                    .list_indices()
                    .await
                    .with_context(|| format!("list {name} indexes"))?
                    .into_iter()
                    .map(|index| index.name)
                    .collect();
                for (_, index_name, column) in INDEXES.iter().filter(|(table_name, _, _)| *table_name == name) {
                    if existing.contains(*index_name) {
                        continue;
                    }
                    table
                        .create_index(&[*column], index_for(column))
                        .name((*index_name).to_string())
                        .execute()
                        .await
                        .with_context(|| format!("create {index_name} on {name}.{column}"))?;
                }
                table
                    .optimize(OptimizeAction::Index(OptimizeOptions::default()))
                    .await
                    .with_context(|| format!("update {name} indexes"))?;
            }
            Ok(())
        })?;
        self.lance.maintain(super::SCAN_RETENTION)
    }

    /// What a search can use: whether the full-text index exists (the lexical lane fails without
    /// one) and how many chunk rows carry a vector.
    pub fn capabilities(&self) -> Result<StoreCapabilities> {
        self.block_on(async {
            let table = self.table(DOC_CHUNKS_TABLE).await?;
            let lexical = table
                .list_indices()
                .await
                .context("list doc_chunks indexes")?
                .iter()
                .any(|index| index.columns.iter().any(|column| column == "text"));
            let rows = table.count_rows(None).await.context("count doc_chunks")?;
            let vectors = table
                .count_rows(Some("embedding IS NOT NULL".to_string()))
                .await
                .context("count embedded doc_chunks")?;
            Ok(StoreCapabilities { lexical, rows, vectors })
        })
    }

    /// The vector lane: the `k` nearest chunks under `predicate`, exactly (ADR-0029 rule 6: no
    /// vector index below 50,000 pre-filtered rows, and none is built today). Chunks without a
    /// vector are never returned.
    pub fn nearest(&self, query: &[f32], predicate: &DocPredicate, k: usize) -> Result<Vec<DocChunkHit>> {
        if predicate.selects_nothing() {
            return Ok(Vec::new());
        }
        self.block_on(async {
            let table = self.table(DOC_CHUNKS_TABLE).await?;
            let search = table
                .vector_search(query)
                .context("build vector search")?
                .column("embedding")
                .distance_type(DistanceType::L2)
                .bypass_vector_index()
                .limit(k)
                .select(Select::columns(HIT_COLUMNS));
            collect(predicate.apply(search)).await
        })
    }

    /// The lexical lane: BM25 over chunk text under `predicate`, `fuzzy` allowing one edit per
    /// term. Fails when the full-text index does not exist yet; see [`Self::capabilities`].
    pub fn lexical(&self, text: &str, predicate: &DocPredicate, k: usize, fuzzy: bool) -> Result<Vec<DocChunkHit>> {
        if predicate.selects_nothing() {
            return Ok(Vec::new());
        }
        let query = if fuzzy {
            FullTextSearchQuery::new_fuzzy(text.to_string(), Some(1))
        } else {
            FullTextSearchQuery::new(text.to_string())
        };
        self.block_on(async {
            let table = self.table(DOC_CHUNKS_TABLE).await?;
            let search = table
                .query()
                .full_text_search(query)
                .limit(k)
                .select(Select::columns(HIT_COLUMNS));
            collect(predicate.apply(search)).await
        })
    }

    /// The chunks `ords` of each document in `wanted`: the neighbours a page expands into.
    pub fn chunks(&self, wanted: &[(String, Vec<u32>)]) -> Result<Vec<DocChunkHit>> {
        let clauses: Vec<String> = wanted
            .iter()
            .filter(|(_, ords)| !ords.is_empty())
            .map(|(key, ords)| {
                let ords: Vec<String> = ords.iter().map(u32::to_string).collect();
                format!(
                    "(doc_key = '{}' AND chunk_ord IN ({}))",
                    escape_sql_literal(key),
                    ords.join(", ")
                )
            })
            .collect();
        if clauses.is_empty() {
            return Ok(Vec::new());
        }
        self.block_on(async {
            let table = self.table(DOC_CHUNKS_TABLE).await?;
            let query = table
                .query()
                .only_if(clauses.join(" OR "))
                .select(Select::columns(HIT_COLUMNS));
            collect(query).await
        })
    }

    /// The document rows of `keys`.
    pub fn documents(&self, keys: &[String]) -> Result<Vec<DocMeta>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        self.block_on(async {
            let table = self.table(DOCS_TABLE).await?;
            let mut out = Vec::new();
            for part in keys.chunks(KEYS_PER_STATEMENT) {
                let list = part.iter().map(|key| lit(key.as_str())).collect();
                let mut stream = table
                    .query()
                    .only_if_expr(is_in(col("doc_key"), list))
                    .select(Select::columns(&["doc_key", "title", "n_chunks", "embedded"]))
                    .execute()
                    .await
                    .context("read documents")?;
                while let Some(batch) = stream.try_next().await.context("read documents")? {
                    decode_doc_meta(&batch, &mut out)?;
                }
            }
            Ok(out)
        })
    }

    /// The keys of the documents carrying any of `keywords` (lower-cased whole keywords).
    pub fn keys_with_keywords(&self, keywords: &[String]) -> Result<Vec<String>> {
        if keywords.is_empty() {
            return Ok(Vec::new());
        }
        let list: Vec<String> = keywords
            .iter()
            .map(|keyword| format!("'{}'", escape_sql_literal(keyword)))
            .collect();
        self.block_on(async {
            let table = self.table(DOCS_TABLE).await?;
            let mut stream = table
                .query()
                .only_if(format!("array_has_any(keywords, [{}])", list.join(", ")))
                .select(Select::columns(&["doc_key"]))
                .execute()
                .await
                .context("select documents by keyword")?;
            let mut keys = Vec::new();
            while let Some(batch) = stream.try_next().await.context("read documents by keyword")? {
                decode_keys(&batch, &mut keys)?;
            }
            Ok(keys)
        })
    }
}

/// What [`DocStore::capabilities`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreCapabilities {
    /// The full-text index exists, so the lexical lane can run.
    pub lexical: bool,
    pub rows: usize,
    /// Chunk rows with a vector; the vector lane has nothing to rank without them.
    pub vectors: usize,
}

async fn collect(query: impl ExecutableQuery) -> Result<Vec<DocChunkHit>> {
    let mut stream = query.execute().await.context("run document query")?;
    let mut hits = Vec::new();
    while let Some(batch) = stream.try_next().await.context("read document query")? {
        decode_chunk_hits(&batch, &mut hits)?;
    }
    Ok(hits)
}

#[cfg(test)]
#[path = "docs_tests.rs"]
mod tests;
