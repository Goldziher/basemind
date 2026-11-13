use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use arrow_array::{RecordBatch, StringArray, UInt32Array};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::LanceStore;
use super::shared::LivePaths;
use super::shared_store::{SharedLane, SharedStore};

/// A snapshot is valid only while the source table revision and generation match.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Snapshot {
    generation: String,
    revision: u64,
    store_epoch: String,
    shared_epoch: String,
    paths: BTreeMap<(String, u32), String>,
}

impl Snapshot {
    pub(crate) fn paths(&self) -> LivePaths<'_> {
        LivePaths::from_exact(
            self.paths
                .iter()
                .map(|((path, ordinal), key)| (key.as_str(), *ordinal, path.as_str())),
        )
    }

    pub(crate) fn matches(&self, source: &LanceStore, lane: SharedLane, generation: &str) -> Result<bool> {
        if generation != self.generation || read_epoch(source.dir())? != self.store_epoch {
            return Ok(false);
        }
        source.inner.rt().block_on(async {
            let table = source.inner.connection.open_table(lane.name()).execute().await?;
            table.checkout_latest().await?;
            Ok(self.revision == table.version().await?)
        })
    }
}

pub(crate) fn publish(
    source: &LanceStore,
    shared: &SharedStore,
    lane: SharedLane,
    scope: &str,
    generation: String,
    key_for: impl Fn(&str) -> Option<String>,
) -> Result<Snapshot> {
    let runtime = source.inner.rt();
    let (revision, mut stream) = runtime.block_on(async {
        let table = source.inner.connection.open_table(lane.name()).execute().await?;
        table.checkout_latest().await?;
        let table = table.checkout_current().await?;
        let revision = table.version().await?;
        let predicate = format!("scope = '{}'", super::escape_sql_literal(scope));
        let stream = table.query().only_if(predicate).execute().await?;
        anyhow::Ok((revision, stream))
    })?;
    let mut paths = BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(batch) = runtime.block_on(stream.try_next())? {
        if let Some(batch) = project(&batch, lane, source.dim(), &key_for, &mut paths, &mut seen)? {
            shared.insert(lane, batch)?;
        }
    }
    Ok(Snapshot {
        generation,
        revision,
        store_epoch: epoch(source.dir())?,
        shared_epoch: epoch(shared.directory())?,
        paths,
    })
}

fn read_epoch(dir: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(dir.join("shared-snapshot-epoch"))?)
}

fn epoch(dir: &Path) -> Result<String> {
    match read_epoch(dir) {
        Ok(existing) => Ok(existing),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            let mut token = tempfile::NamedTempFile::new_in(dir)?;
            let identity = token
                .path()
                .file_name()
                .context("missing epoch token")?
                .to_string_lossy()
                .into_owned();
            token.write_all(identity.as_bytes())?;
            token.as_file().sync_all()?;
            match token.persist_noclobber(dir.join("shared-snapshot-epoch")) {
                Ok(_) => Ok(identity),
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => read_epoch(dir),
                Err(error) => Err(error.into()),
            }
        }
        Err(error) => Err(error),
    }
}

fn snapshot_path(source: &LanceStore, lane: SharedLane, scope: &str) -> std::path::PathBuf {
    let scope = blake3::hash(scope.as_bytes());
    source.dir().join(format!("shared-{}-{scope}.msgpack", lane.name()))
}

fn save(snapshot: &Snapshot, source: &LanceStore, lane: SharedLane, scope: &str) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(source.dir())?;
    rmp_serde::encode::write_named(&mut file, snapshot)?;
    file.as_file().sync_all()?;
    file.persist(snapshot_path(source, lane, scope))?;
    Ok(())
}

fn load(source: &LanceStore, lane: SharedLane, scope: &str) -> Result<Snapshot> {
    Ok(rmp_serde::from_read(std::fs::File::open(snapshot_path(
        source, lane, scope,
    ))?)?)
}

pub(crate) fn generation(
    index: &crate::store::Index,
    config: &crate::config::Config,
    lane: SharedLane,
) -> Result<String> {
    let mut hash = blake3::Hasher::new();
    hash.update(&serde_json::to_vec(config)?);
    let mut entries = BTreeMap::new();
    match lane {
        #[cfg(feature = "code-search")]
        SharedLane::Code => {
            for (path, entry) in &index.files {
                entries.insert(path, super::shared::code_content_key(entry));
            }
        }
        _ => {
            for (path, entry) in &index.doc_files {
                entries.insert(path, entry.hash_hex.clone());
            }
        }
    }
    hash.update(&rmp_serde::to_vec(&entries)?);
    Ok(hash.finalize().to_hex().to_string())
}

pub(crate) fn publish_scan(store: &crate::store::Store, config: &crate::config::Config, scope: &str) -> Result<()> {
    let Some(source) = &store.lance else {
        return Ok(());
    };
    let group = crate::store::repo_group_dir(&store.root);
    let shared = SharedStore::open(&group, source.embedding_model(), source.dim())?;
    #[cfg(feature = "documents")]
    if config.documents.enabled && config.documents.embed {
        let lane = SharedLane::Documents;
        let generation = generation(&store.index, config, lane)?;
        let snapshot = publish(source, &shared, lane, scope, generation, |path| {
            store.lookup_doc(path).map(|entry| entry.hash_hex.clone())
        })?;
        save(&snapshot, source, lane, scope)?;
    }
    #[cfg(feature = "code-search")]
    if config.code_search.enabled && config.code_search.embed {
        let lane = SharedLane::Code;
        let generation = generation(&store.index, config, lane)?;
        let snapshot = publish(source, &shared, lane, scope, generation, |path| {
            store.lookup(path).map(super::shared::code_content_key)
        })?;
        save(&snapshot, source, lane, scope)?;
    }
    Ok(())
}

pub(crate) fn enabled() -> bool {
    std::env::var_os("BASEMIND_INDEX_LAYERED").is_some_and(|value| value == "1")
}

pub(crate) fn read<T>(
    source: &LanceStore,
    group: &Path,
    lane: SharedLane,
    scope: &str,
    generation: &str,
    query: impl FnOnce(&SharedStore, &LivePaths<'_>) -> Result<T>,
) -> Option<T> {
    let result: Result<Option<T>> = (|| {
        let snapshot = load(source, lane, scope)?;
        if !snapshot.matches(source, lane, generation)? {
            return Ok(None);
        }
        let directory = super::shared::shared_directory(group, source.embedding_model(), source.dim());
        if read_epoch(&directory)? != snapshot.shared_epoch
            || !directory.join(format!("{}.lance", lane.name())).is_dir()
        {
            return Ok(None);
        }
        let shared = SharedStore::open(group, source.embedding_model(), source.dim())?;
        let hits = query(&shared, &snapshot.paths())?;
        let valid = read_epoch(&directory)? == snapshot.shared_epoch && snapshot.matches(source, lane, generation)?;
        Ok(valid.then_some(hits))
    })();
    match result {
        Ok(hits) => hits,
        Err(error) => {
            tracing::debug!(%error, "using standalone Lance snapshot");
            None
        }
    }
}

fn project(
    batch: &RecordBatch,
    lane: SharedLane,
    dim: u16,
    key_for: &impl Fn(&str) -> Option<String>,
    paths: &mut BTreeMap<(String, u32), String>,
    seen: &mut std::collections::BTreeSet<(String, u32)>,
) -> Result<Option<RecordBatch>> {
    let path = strings(batch, "path")?;
    let ordinals = ordinals(batch, lane)?;
    let mut keys = Vec::with_capacity(batch.num_rows());
    let mut selected = Vec::with_capacity(batch.num_rows());
    for (index, ordinal) in ordinals.into_iter().enumerate() {
        let path = path.value(index);
        let content = key_for(path).with_context(|| format!("no manifest identity for {path}"))?;
        let key = row_key(batch, index, &content)?;
        paths.insert((path.into(), ordinal), key.clone());
        if seen.insert((key.clone(), ordinal)) {
            keys.push(key);
            selected.push(u32::try_from(index)?);
        }
    }
    if selected.is_empty() {
        return Ok(None);
    }
    let selected = UInt32Array::from(selected);
    let mut columns = batch.columns().to_vec();
    let path_index = batch.schema().index_of("path")?;
    columns[path_index] = Arc::new(StringArray::from(keys));
    for (index, column) in columns.iter_mut().enumerate() {
        if index != path_index {
            *column = arrow_select::take::take(column.as_ref(), &selected, None)?;
        }
    }
    #[cfg(feature = "code-search")]
    if matches!(lane, SharedLane::Code) {
        let chunk_ids = strings(batch, "chunk_id")?;
        let selected_ordinals = selected
            .values()
            .iter()
            .map(|&i| code_ordinal(chunk_ids.value(i as usize)))
            .collect::<Result<Vec<_>>>()?;
        columns.push(Arc::new(UInt32Array::from(selected_ordinals)));
    }
    Ok(Some(RecordBatch::try_new(lane.schema(dim), columns)?))
}

fn row_key(batch: &RecordBatch, index: usize, content: &str) -> Result<String> {
    let schema = batch.schema();
    let columns = (0..batch.num_columns())
        .filter(|&column| schema.field(column).name() != "path")
        .collect::<Vec<_>>();
    let row = batch.slice(index, 1).project(&columns)?;
    let mut buffer = Vec::new();
    let mut writer = arrow_ipc::writer::StreamWriter::try_new(&mut buffer, &row.schema())?;
    writer.write(&row)?;
    writer.finish()?;
    let mut key = blake3::Hasher::new();
    key.update(&(content.len() as u64).to_le_bytes());
    key.update(content.as_bytes());
    key.update(&buffer);
    Ok(key.finalize().to_hex().to_string())
}

fn strings<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    batch
        .column_by_name(name)
        .and_then(|column| column.as_any().downcast_ref())
        .with_context(|| format!("missing {name}"))
}

fn ordinals(batch: &RecordBatch, lane: SharedLane) -> Result<Vec<u32>> {
    #[cfg(feature = "code-search")]
    if matches!(lane, SharedLane::Code) {
        let ids = strings(batch, "chunk_id")?;
        return ids
            .iter()
            .map(|id| code_ordinal(id.context("null chunk ID")?))
            .collect();
    }
    ensure!(matches!(lane, SharedLane::Documents), "unsupported snapshot lane");
    let ordinals = batch
        .column_by_name("chunk_idx")
        .and_then(|column| column.as_any().downcast_ref::<UInt32Array>())
        .context("missing chunk ordinal")?;
    Ok(ordinals.values().to_vec())
}

#[cfg(feature = "code-search")]
fn code_ordinal(id: &str) -> Result<u32> {
    Ok(id.rsplit_once(':').context("invalid chunk ID")?.1.parse()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lance::DocumentRow;

    #[test]
    fn snapshot_matches_generation_and_standalone_revision() {
        let directory = tempfile::tempdir().unwrap();
        let source = LanceStore::open(&directory.path().join("standalone"), 2, "model").unwrap();
        let shared = SharedStore::open(&directory.path().join("shared"), "model", 2).unwrap();
        let lane = SharedLane::Documents;
        let row = DocumentRow {
            scope: "repo".into(),
            path: "current.md".into(),
            chunk_idx: 0,
            mime_type: "text/plain".into(),
            text: "body".into(),
            byte_start: 0,
            byte_end: 4,
            embedding: vec![1.0, 0.0],
        };
        source.replace_document("repo", "current.md", vec![row]).unwrap();
        let snapshot = publish(&source, &shared, lane, "repo", "gen-1".into(), |path| {
            (path == "current.md").then(|| "content".to_owned())
        })
        .unwrap();
        save(&snapshot, &source, lane, "repo").unwrap();
        assert!(snapshot.matches(&source, lane, "gen-1").unwrap());
        let group = directory.path().join("shared");
        let query = |shared: &SharedStore, paths: &LivePaths<'_>| {
            shared.search_documents("repo", vec![0.0, 0.0], None, paths, 2)
        };
        assert!(read(&source, &group, lane, "repo", "gen-2", query).is_none());
        assert_eq!(read(&source, &group, lane, "repo", "gen-1", query).unwrap().len(), 1);
        assert!(!snapshot.matches(&source, lane, "gen-2").unwrap());
        let hits = shared
            .search_documents("repo", vec![0.0, 0.0], None, &snapshot.paths(), 2)
            .unwrap();
        assert_eq!((hits[0].path.as_str(), hits[0].text.as_str()), ("current.md", "body"));
        source
            .delete_documents_for_paths("repo", &["current.md".into()])
            .unwrap();
        assert!(!snapshot.matches(&source, lane, "gen-1").unwrap());
        assert!(read(&source, &group, lane, "repo", "gen-1", query).is_none());
    }
}
