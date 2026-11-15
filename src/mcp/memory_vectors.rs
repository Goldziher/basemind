//! Memory notes found by meaning: the vector rows of the notes, and the search over them.
//!
//! The Fjall `memory_by_key` record is the truth for a note. Its vector row lives in a second
//! store and is written by a delete and an insert that commit separately, from any process that has
//! the notes open. So a row can outlive its note, hold the text of an earlier write, or exist
//! twice. Nothing read from a row is served: a hit is kept only when the note still exists and
//! carries the `updated_at` the row was embedded from, once per key, and it is answered from the
//! note ([`truth_join`]).
//!
//! The rows are derived from the notes and rebuilt from them ([`backfill`]): a search embeds the
//! notes whose row is missing or stale and removes the rows whose note is gone, a bounded amount
//! per call, until the namespace is level. The table is empty after every release minor (its
//! `schema_ver` follows the minor) and for every embedding preset opened for the first time.
//!
//! The store lives beside the notes, in the repo group directory, one directory per embedding model
//! and dimension ([`memory_lance_dir`]): a note saved from one worktree is found from every other,
//! and two worktrees configured with different presets each keep their own table.

#![cfg(feature = "memory")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ahash::{AHashMap, AHashSet};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{elapsed_us, json_result};
use super::memory::{
    embed_query, lance_visibility, memory_get_many_wire, memory_list_page, memory_namespace, shared_embedder,
};
use super::memory_ops::{MEMORY_PREVIEW_CHARS, WireMemoryEntry};
use super::types_memory::{MemoryPutParams, MemorySearchHit, MemorySearchParams, MemorySearchResponse, Visibility};
use crate::lance::{LanceStore, MemoryHit, MemoryRow, MemoryStamps};

/// Notes listed per page while looking for the ones without a current row.
const BACKFILL_PAGE: usize = 500;
/// Notes embedded by one model call and written by one upsert.
const BACKFILL_BATCH: usize = 32;
/// Most notes one search embeds before it answers; the next search continues.
pub(super) const BACKFILL_NOTES_PER_CALL: usize = if cfg!(test) { 5 } else { 256 };
/// Once this much of a search has gone into embedding, no further batch starts.
const BACKFILL_TIME_BUDGET: Duration = Duration::from_secs(3);
/// Rows fetched beyond twice the limit, and the most a search fetches while the join keeps
/// dropping rows.
const FETCH_SLACK: usize = 8;
const FETCH_MAX: usize = 4096;

/// What a search by meaning says while the rows are behind the notes.
const CATCHING_UP: &str = "the memory vector index is still catching up with the saved notes, so this search may \
     miss some of them; later `memory_search` calls continue the work. A read by `key` or a listing by `prefix` \
     is complete";

/// Where one namespace's rows stand against its notes in this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backfill {
    /// A search is embedding the namespace's notes now.
    Running,
    /// Every note has a current row and no row lacks a note.
    Level,
}

/// The repo group's memory vector store for one embedding model:
/// `<repo_group_dir>/memory.lance/<model>-<dim>/`, beside `memory.fjall`. Per group, not per
/// workspace, because the notes are; per model and dimension, because a store holds vectors of
/// one model and is wiped when opened for another. No garbage collector lists the group directory.
pub(super) fn memory_lance_dir(root: &Path, model: &str, dim: u16) -> PathBuf {
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
    crate::store_layout::memory_vectors_dir(root).join(format!("{slug}-{dim}"))
}

/// Open the repo group's memory vector store for the configured embedder.
pub(super) async fn memory_lance_store(state: &ServerState) -> Result<Arc<LanceStore>, McpError> {
    state
        .shared
        .memory_lance
        .get_or_try_init(|| async {
            let embedder = shared_embedder(state)
                .await
                .map_err(|e| format!("embedder init: {e}"))?;
            let (root, model, dim) = (state.shared.root.clone(), embedder.model().to_string(), embedder.dim());
            tokio::task::spawn_blocking(move || {
                let dir = memory_lance_dir(&root, &model, dim);
                LanceStore::open(&dir, dim, &model)
            })
            .await
            .map_err(|e| format!("lance open join: {e}"))?
            .map(Arc::new)
            .map_err(|e| format!("open LanceStore: {e}"))
        })
        .await
        .cloned()
        .map_err(|e| McpError::internal_error(e.clone(), None))
}

/// The store, when a row could be in it: already open in this process, or present on disk. A
/// store that was never created holds no row and is not created to find that out.
async fn memory_lance_store_if_any(state: &ServerState) -> Option<Arc<LanceStore>> {
    if let Some(lance) = state.shared.memory_lance.get() {
        return Some(Arc::clone(lance));
    }
    let embedder = shared_embedder(state).await.ok()?;
    let (root, model, dim) = (state.shared.root.clone(), embedder.model().to_string(), embedder.dim());
    let on_disk = tokio::task::spawn_blocking(move || memory_lance_dir(&root, &model, dim).is_dir())
        .await
        .unwrap_or(false);
    if !on_disk {
        return None;
    }
    match memory_lance_store(state).await {
        Ok(lance) => Some(lance),
        Err(error) => {
            tracing::warn!(error = %error.message, "memory vector store could not be opened");
            None
        }
    }
}

/// Run `work` against the store on a blocking thread.
async fn on_store<T, F>(lance: &Arc<LanceStore>, what: &'static str, work: F) -> Result<T, McpError>
where
    T: Send + 'static,
    F: FnOnce(&LanceStore) -> anyhow::Result<T> + Send + 'static,
{
    let lance = Arc::clone(lance);
    tokio::task::spawn_blocking(move || work(&lance))
        .await
        .map_err(|e| McpError::internal_error(format!("spawn_blocking: {e}"), None))?
        .map_err(|e| McpError::internal_error(format!("{what}: {e}"), None))
}

/// Embed a saved note and upsert its vector row.
pub(super) async fn embed_note(
    state: &ServerState,
    params: &MemoryPutParams,
    owner: &str,
    tags: Vec<String>,
    created_at: i64,
    updated_at: i64,
) -> Result<(), String> {
    let embedding = embed_query(state, &params.value)
        .await
        .map_err(|error| error.message.into_owned())?;
    let lance = memory_lance_store(state)
        .await
        .map_err(|error| error.message.into_owned())?;
    let row = MemoryRow {
        scope: state.shared.scope.clone(),
        key: params.key.clone(),
        value: params.value.clone(),
        tags,
        visibility: lance_visibility(params.visibility).to_string(),
        agent_id: owner.to_string(),
        embedding,
        created_at,
        updated_at,
    };
    on_store(&lance, "upsert_memory", move |lance| lance.upsert_memory(row))
        .await
        .map_err(|error| error.message.into_owned())
}

/// Best-effort removal of one note's vector row from the configured model's store. The fjall
/// record is the source of truth; a row that cannot be removed is logged, never an error, and the
/// join keeps it from being served.
pub(super) async fn delete_vector_row(state: &ServerState, visibility: Visibility, owner: &str, key: &str) {
    let Some(lance) = memory_lance_store_if_any(state).await else {
        return;
    };
    let scope = state.shared.scope.clone();
    let row_key = key.to_string();
    let visibility = lance_visibility(visibility);
    let agent_id = owner.to_string();
    let deleted = on_store(&lance, "delete_memory", move |lance| {
        lance.delete_memory(&scope, visibility, &agent_id, &row_key)
    })
    .await;
    if let Err(error) = deleted {
        tracing::warn!(%key, error = %error.message, "the note's vector row was not removed");
    }
}

/// The notes to embed, given the rows the table holds: those with no row, a row embedded from an
/// earlier write, or more than one row. A note with no text cannot be embedded and is never behind.
pub(super) fn notes_behind(notes: Vec<WireMemoryEntry>, rows: &MemoryStamps) -> Vec<WireMemoryEntry> {
    notes
        .into_iter()
        .filter(|note| !note.value.is_empty())
        .filter(|note| rows.stamps.get(&note.key) != Some(&note.updated_at) || rows.duplicated.contains(&note.key))
        .collect()
}

/// Drops the `Running` mark of a backfill that did not reach `Level`, however it ended.
struct RunningMark<'a> {
    state: &'a ServerState,
    namespace: (u8, String),
    level: bool,
}

impl Drop for RunningMark<'_> {
    fn drop(&mut self) {
        let mut marks = backfill_marks(self.state);
        if self.level {
            marks.insert(self.namespace.clone(), Backfill::Level);
        } else {
            marks.remove(&self.namespace);
        }
    }
}

fn backfill_marks(state: &ServerState) -> std::sync::MutexGuard<'_, AHashMap<(u8, String), Backfill>> {
    state
        .shared
        .memory_backfill
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Bring one namespace's rows nearer its notes, unless they are level or another search is at it.
/// Returns whether the rows are still behind. Best effort: a failure is logged, the namespace stays
/// unmarked so the next search tries again, and the search that triggered it still runs over
/// whatever rows exist.
pub(super) async fn backfill_step(state: &ServerState, visibility: Visibility, vis_byte: u8, owner: &str) -> bool {
    let namespace = (vis_byte, owner.to_string());
    {
        let mut marks = backfill_marks(state);
        match marks.get(&namespace) {
            Some(Backfill::Level) => return false,
            Some(Backfill::Running) => return true,
            None => marks.insert(namespace.clone(), Backfill::Running),
        };
    }
    let mut mark = RunningMark {
        state,
        namespace,
        level: false,
    };
    match backfill(state, visibility, vis_byte, owner).await {
        Ok(progress) => {
            if progress.embedded > 0 || progress.pruned > 0 {
                tracing::info!(
                    embedded = progress.embedded,
                    pruned = progress.pruned,
                    level = progress.level,
                    "memory vector rows reconciled with the notes"
                );
            }
            mark.level = progress.level;
        }
        Err(error) => tracing::warn!(error = %error.message, "memory vector backfill failed; the next search retries"),
    }
    !mark.level
}

/// What one backfill pass did.
struct Progress {
    embedded: usize,
    pruned: u64,
    /// Every note has a current row: nothing was left for a later call and nothing failed.
    level: bool,
}

/// One bounded pass: remove the rows whose note is gone, then embed notes that have no current
/// row, at most [`BACKFILL_NOTES_PER_CALL`] and no new batch after [`BACKFILL_TIME_BUDGET`].
async fn backfill(
    state: &ServerState,
    visibility: Visibility,
    vis_byte: u8,
    owner: &str,
) -> Result<Progress, McpError> {
    let lance = memory_lance_store(state).await?;
    let scope = state.shared.scope.clone();
    let lance_vis = lance_visibility(visibility);
    let listed_at = crate::lance::now_micros();
    let rows = {
        let (scope, agent_id) = (scope.clone(), owner.to_string());
        on_store(&lance, "memory_stamps", move |lance| {
            lance.memory_stamps(&scope, lance_vis, &agent_id)
        })
        .await?
    };
    let mut live: AHashSet<String> = AHashSet::new();
    let mut behind = Vec::new();
    let mut cursor = None;
    loop {
        let page = memory_list_page(state, vis_byte, owner, "", None, BACKFILL_PAGE, cursor).await?;
        live.extend(page.entries.iter().map(|note| note.key.clone()));
        behind.extend(notes_behind(page.entries, &rows));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    let orphans: Vec<String> = rows.stamps.keys().filter(|key| !live.contains(*key)).cloned().collect();
    let pruned = if orphans.is_empty() {
        0
    } else {
        let (scope, agent_id) = (scope.clone(), owner.to_string());
        // Bounded by the instant the rows were listed: a note saved since has a newer row.
        on_store(&lance, "prune memory rows", move |lance| {
            let keys: Vec<&str> = orphans.iter().map(String::as_str).collect();
            lance.delete_memory_keys(&scope, lance_vis, &agent_id, &keys, Some(listed_at))
        })
        .await?
    };

    let mut level = behind.len() <= BACKFILL_NOTES_PER_CALL;
    behind.truncate(BACKFILL_NOTES_PER_CALL);
    let started = Instant::now();
    let mut embedded = 0;
    for (index, batch) in behind.chunks(BACKFILL_BATCH).enumerate() {
        if index > 0 && started.elapsed() >= BACKFILL_TIME_BUDGET {
            level = false;
            break;
        }
        let notes = whole_notes(state, vis_byte, owner, batch).await?;
        let wanted = notes.len();
        let rows: Vec<MemoryRow> = embed_notes(state, notes)
            .await
            .into_iter()
            .map(|(note, embedding)| MemoryRow {
                scope: scope.clone(),
                key: note.key,
                value: note.value,
                tags: note.tags,
                visibility: lance_vis.to_string(),
                agent_id: owner.to_string(),
                embedding,
                created_at: note.created_at,
                updated_at: note.updated_at,
            })
            .collect();
        // A note that could not be embedded has no row yet: the namespace is not level.
        level &= rows.len() == wanted;
        embedded += rows.len();
        on_store(&lance, "upsert_memory_rows", move |lance| {
            lance.upsert_memory_rows(&rows)
        })
        .await?;
    }
    Ok(Progress {
        embedded,
        pruned,
        level,
    })
}

/// The notes of `listed` with their whole text. A listing truncates a long value to a preview;
/// only those notes are read again, and one deleted since the listing is left out.
async fn whole_notes(
    state: &ServerState,
    vis_byte: u8,
    owner: &str,
    listed: &[WireMemoryEntry],
) -> Result<Vec<WireMemoryEntry>, McpError> {
    let clipped: Vec<String> = listed
        .iter()
        .filter(|note| note.value.len() > MEMORY_PREVIEW_CHARS)
        .map(|note| note.key.clone())
        .collect();
    let mut whole = memory_get_many_wire(state, vis_byte, owner, clipped).await?.into_iter();
    let mut notes = Vec::with_capacity(listed.len());
    for note in listed {
        if note.value.len() <= MEMORY_PREVIEW_CHARS {
            notes.push(note.clone());
        } else if let Some(Some(read)) = whole.next() {
            notes.push(read);
        }
    }
    Ok(notes)
}

/// Embed `notes` in one model call. When the batch fails each note is tried alone, so one note the
/// model rejects costs only itself; a note that still fails is logged and left out.
async fn embed_notes(state: &ServerState, notes: Vec<WireMemoryEntry>) -> Vec<(WireMemoryEntry, Vec<f32>)> {
    let batch = match shared_embedder(state).await {
        Ok(embedder) => {
            let texts: Vec<String> = notes.iter().map(|note| note.value.clone()).collect();
            tokio::task::spawn_blocking(move || {
                let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
                embedder.embed_batch(&refs)
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|embedded| embedded.map_err(|e| e.to_string()))
        }
        Err(error) => Err(error),
    };
    match batch {
        Ok(embeddings) if embeddings.len() == notes.len() => notes.into_iter().zip(embeddings).collect(),
        _ => {
            let mut embedded = Vec::with_capacity(notes.len());
            for note in notes {
                match embed_query(state, &note.value).await {
                    Ok(embedding) => embedded.push((note, embedding)),
                    Err(error) => {
                        tracing::warn!(key = %note.key, error = %error.message, "memory note could not be embedded");
                    }
                }
            }
            embedded
        }
    }
}

/// The hits a search serves, and how many rows the join refused.
pub(super) struct Joined {
    pub hits: Vec<MemorySearchHit>,
    /// Rows whose note is gone or rewritten, and second rows of a key. Rows the `tag` filter
    /// excluded are not counted: they are current.
    pub dropped: usize,
}

/// Join vector hits against the notes read for their keys. A hit is served once per key, from the
/// note, and only when the note exists, carries the `updated_at` the row was embedded from and has
/// `tag` when one is asked for.
pub(super) fn truth_join(
    hits: Vec<MemoryHit>,
    notes: &AHashMap<String, Option<WireMemoryEntry>>,
    tag: Option<&str>,
    limit: usize,
) -> Joined {
    let mut served: AHashSet<String> = AHashSet::new();
    let mut joined = Joined {
        hits: Vec::new(),
        dropped: 0,
    };
    for hit in hits {
        let current = notes
            .get(&hit.key)
            .and_then(Option::as_ref)
            .filter(|note| note.updated_at == hit.updated_at);
        let Some(note) = current else {
            joined.dropped += 1;
            continue;
        };
        if served.contains(&hit.key) {
            joined.dropped += 1;
            continue;
        }
        if tag.is_some_and(|tag| !note.tags.iter().any(|t| t == tag)) {
            continue;
        }
        if joined.hits.len() < limit {
            served.insert(hit.key);
            joined.hits.push(MemorySearchHit {
                key: note.key.clone(),
                value: note.value.clone(),
                tags: note.tags.clone(),
                distance: hit.distance,
            });
        }
    }
    joined
}

/// `memory_search` by meaning: embed the query, bring the rows nearer the notes, and answer the
/// nearest rows from the notes they were embedded from.
pub(super) async fn search(state: &ServerState, params: MemorySearchParams) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let limit = params.limit.unwrap_or(10).min(100) as usize;
    let (vis_byte, owner) = memory_namespace(state, params.visibility);
    let lance_vis = lance_visibility(params.visibility);
    let embedding = embed_query(state, &params.query).await?;
    let lance = memory_lance_store(state).await?;
    let catching_up = backfill_step(state, params.visibility, vis_byte, owner).await;

    // Rows the join refuses must not shrink the answer: the search fetches more than it serves,
    // and more again while refused rows leave it short and the table has rows left.
    let mut fetch = limit.saturating_mul(2).saturating_add(FETCH_SLACK);
    let mut notes: AHashMap<String, Option<WireMemoryEntry>> = AHashMap::new();
    let joined = loop {
        let (scope, agent_id, query) = (state.shared.scope.clone(), owner.to_string(), embedding.clone());
        let raw = on_store(&lance, "search_memory", move |lance| {
            lance.search_memory(&scope, lance_vis, &agent_id, query, fetch, None)
        })
        .await?;
        let exhausted = raw.len() < fetch;
        let mut unread: Vec<String> = Vec::new();
        for hit in &raw {
            if !notes.contains_key(&hit.key) && !unread.contains(&hit.key) {
                unread.push(hit.key.clone());
            }
        }
        let read = memory_get_many_wire(state, vis_byte, owner, unread.clone()).await?;
        notes.extend(unread.into_iter().zip(read));
        let joined = truth_join(raw, &notes, params.tag.as_deref(), limit);
        if joined.hits.len() >= limit || exhausted || fetch >= FETCH_MAX {
            break joined;
        }
        fetch = fetch.saturating_mul(4).min(FETCH_MAX);
    };
    if joined.dropped > 0 {
        // Rows without a current note are in the table: the next search reconciles again.
        let mut marks = backfill_marks(state);
        if marks.get(&(vis_byte, owner.to_string())) == Some(&Backfill::Level) {
            marks.remove(&(vis_byte, owner.to_string()));
        }
    }
    json_result(&MemorySearchResponse {
        query: params.query,
        hits: joined.hits,
        notice: catching_up.then(|| CATCHING_UP.to_string()),
        elapsed_us: elapsed_us(started),
    })
}

#[cfg(test)]
#[path = "memory_vectors_tests.rs"]
mod tests;
