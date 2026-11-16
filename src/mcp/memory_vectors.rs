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
//! notes whose row is missing or stale and removes the rows whose note is gone or has no text, a
//! bounded amount per call, until the namespace is level. Level is a belief of this process, and
//! other processes save notes too, so it lapses after [`LEVEL_TTL`], longer each time a look finds
//! nothing to do up to [`LEVEL_TTL_MAX`], and is dropped at once by a save here that wrote no row
//! and by a save in any process that wrote none, which says so in a file of the repo group
//! ([`owe_rows`]). A save that wrote its row wrote it in its own preset's table only, and says so
//! to every other table's searches the same way ([`owe_rows_elsewhere`]).
//! The table is empty after every release minor (its `schema_ver` follows the minor) and for every
//! embedding preset opened for the first time.
//!
//! A row holds a note's vector and tags, never its text.
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
    embed_query, lance_visibility, memory_get_many_wire, memory_list_page, memory_namespace, memory_stamps_wire,
    shared_embedder,
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
/// How long a namespace found level is taken to be level. Another process may have saved a note
/// whose embedding failed, or one saved with `embed: false`; after this long a search looks again.
const LEVEL_TTL: Duration = Duration::from_secs(300);
/// The longest a namespace is taken to be level. Each look that finds nothing to do doubles the
/// wait up to this, since a look lists every note and every row inside a search.
const LEVEL_TTL_MAX: Duration = Duration::from_secs(60 * 60);
/// Rows fetched beyond twice the limit, and the most a search fetches while the join keeps
/// dropping rows.
const FETCH_SLACK: usize = 8;
const FETCH_MAX: usize = 4096;

/// What a search by meaning says while the rows are behind the notes.
const CATCHING_UP: &str = "the memory vector index is still catching up with the saved notes, so this search may \
     miss some of them; later `memory_search` calls continue the work. A read by `key` or a listing by `prefix` \
     is complete";

/// What `memory_search` can still do while the vector store is unavailable.
const MEMORY_SEARCH_MEANWHILE: Option<&str> =
    Some("A read by `key` and a listing by `prefix` or with no arguments do not need it.");

/// Where one namespace's rows stand against its notes in this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backfill {
    /// A search is embedding the namespace's notes now.
    Running,
    /// Every note had a current row and no row lacked a note when last looked, which is believed
    /// until this instant, `ttl` after the look, and while the group's [`rows_owed`] reads `owed`.
    Level { until: Instant, ttl: Duration, owed: u64 },
}

impl Backfill {
    /// Level as of now, for `ttl`, with the group's [`rows_owed`] read before the look as `owed`.
    pub(crate) fn level(ttl: Duration, owed: u64) -> Self {
        Self::Level {
            until: Instant::now() + ttl,
            ttl,
            owed,
        }
    }
}

/// File in the repo group directory that a save rewrites when it leaves a note without a row of
/// its write: the embedding failed or was not asked for. Every process with the notes open reads
/// it before it trusts a namespace to be level, so a note saved elsewhere is not hidden from
/// search until the belief lapses.
const ROWS_OWED_FILE: &str = "memory-rows-owed";

fn rows_owed_path(root: &Path) -> PathBuf {
    crate::store_layout::repo_group_dir(root).join(ROWS_OWED_FILE)
}

/// The stamp a save rewrites for the table `table` (a directory name under
/// [`crate::store_layout::memory_vectors_dir`]) when it wrote its row in another preset's table.
fn table_owed_path(root: &Path, table: &str) -> PathBuf {
    crate::store_layout::repo_group_dir(root).join(format!("{ROWS_OWED_FILE}.{table}"))
}

/// The rows-owed stamp of the table `table`: `0` while no save has owed it a row, and another
/// value after each.
pub(super) fn rows_owed(root: &Path, table: &str) -> u64 {
    let group = std::fs::read(rows_owed_path(root)).unwrap_or_default();
    let own = std::fs::read(table_owed_path(root, table)).unwrap_or_default();
    if group.is_empty() && own.is_empty() {
        return 0;
    }
    let hash = crate::hashing::hash_bytes(&[group.as_slice(), b"\n", own.as_slice()].concat());
    u64::from_le_bytes([hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7]]) | 1
}

/// [`rows_owed`] of the table this process searches, read on a blocking thread.
pub(super) async fn rows_owed_here(state: &ServerState) -> u64 {
    let (root, table) = (state.shared.root.clone(), table_name(state).await.unwrap_or_default());
    tokio::task::spawn_blocking(move || rows_owed(&root, &table))
        .await
        .unwrap_or_default()
}

/// The directory name of the table this process's embedder writes, or `None` without an embedder.
async fn table_name(state: &ServerState) -> Option<String> {
    let embedder = shared_embedder(state).await.ok()?;
    let dir = memory_lance_dir(&state.shared.root, embedder.model(), embedder.dim());
    dir.file_name().and_then(|name| name.to_str()).map(str::to_string)
}

/// Tell every process with the group's notes open that a note has no row of its latest write.
/// Best effort: a stamp that cannot be written leaves other processes to find the note when their
/// belief lapses.
pub(super) fn owe_rows(root: &Path) {
    write_owed_stamp(&rows_owed_path(root));
}

/// A save wrote its note's row in this process's table: tell the processes searching every other
/// preset's table, which has no row of the write. A table created after the listing here lists
/// the note when it is first searched.
///
/// Every table directory present is stamped, one of a preset no longer in use too: neither those
/// directories nor their `memory-rows-owed.<table>` stamps are ever collected. A process that dies
/// between writing the row and these stamps leaves the other presets' searches to find the note
/// when their level lapses, after at most [`LEVEL_TTL_MAX`].
pub(super) async fn owe_rows_elsewhere(state: &ServerState) {
    let Some(own) = table_name(state).await else {
        return;
    };
    let root = state.shared.root.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let Ok(tables) = std::fs::read_dir(crate::store_layout::memory_vectors_dir(&root)) else {
            return;
        };
        for table in tables.flatten() {
            let name = table.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name != own && table.file_type().is_ok_and(|kind| kind.is_dir()) {
                write_owed_stamp(&table_owed_path(&root, name));
            }
        }
    })
    .await;
}

/// Rewrite the rows-owed stamp at `path` with a value no earlier write of it held. Written aside
/// and renamed into place: a reader between a truncate and a write would read no stamp, which
/// equals a mark taken before any stamp existed, and trust a level that a save has broken.
fn write_owed_stamp(path: &Path) {
    static SAVES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let save = SAVES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = format!("{} {} {save}", crate::lance::now_micros(), std::process::id());
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(ROWS_OWED_FILE);
    let temp = path.with_file_name(format!(".{name}.{}.{save}.tmp", std::process::id()));
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&temp, stamp))
        .and_then(|()| std::fs::rename(&temp, path));
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        tracing::debug!(%error, path = %path.display(), "the memory rows-owed stamp was not written");
    }
}

/// How long a namespace found level is believed level. A look after a lapse that found nothing to
/// do waits twice as long as the one before; any work done starts over at [`LEVEL_TTL`].
fn level_ttl(lapsed: Option<Duration>, found_work: bool) -> Duration {
    match lapsed {
        Some(ttl) if !found_work => ttl.saturating_mul(2).clamp(LEVEL_TTL, LEVEL_TTL_MAX),
        _ => LEVEL_TTL,
    }
}

/// Forget that a namespace was found level: a note in it has no current row. A pass that is
/// running keeps its mark, and finds out for itself ([`backfill`] re-reads what it embedded).
pub(super) fn not_level(state: &ServerState, vis_byte: u8, owner: &str) {
    let mut marks = backfill_marks(state);
    let namespace = (vis_byte, owner.to_string());
    if matches!(marks.get(&namespace), Some(Backfill::Level { .. })) {
        marks.remove(&namespace);
    }
}

/// The repo group's memory vector store for one embedding model:
/// `<repo_group_dir>/memory.lance/<model>-<dim>-<hash>/`, beside `memory.fjall`. Per group, not per
/// workspace, because the notes are; per model and dimension, because a store holds vectors of
/// one model and is wiped when opened for another. The readable part of the name maps several
/// model names to one string (`a/b` and `a-b`), so the name ends in a hash of the model's own
/// name: two models never share a directory and wipe each other's rows in turn. No garbage
/// collector lists the group directory.
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
    let hash = crate::hashing::hex(&crate::hashing::hash_bytes(model.as_bytes()));
    crate::store_layout::memory_vectors_dir(root).join(format!("{slug}-{dim}-{}", &hash[..8]))
}

/// Open the repo group's memory vector store for the configured embedder.
pub(super) async fn memory_lance_store(state: &ServerState) -> Result<Arc<LanceStore>, super::lance_cell::Unavailable> {
    state
        .shared
        .memory_lance
        .get_or_open(|| async {
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
            tracing::warn!(%error, "memory vector store could not be opened");
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
    let lance = memory_lance_store(state).await.map_err(|error| error.to_string())?;
    let row = MemoryRow {
        scope: state.shared.scope.clone(),
        key: params.key.clone(),
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
    /// How long the namespace is level for, once the pass found it level.
    level: Option<Duration>,
    /// The group's [`rows_owed`] read before the pass looked.
    owed: u64,
}

impl Drop for RunningMark<'_> {
    fn drop(&mut self) {
        let mut marks = backfill_marks(self.state);
        if let Some(ttl) = self.level {
            marks.insert(self.namespace.clone(), Backfill::level(ttl, self.owed));
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
    // Read before the look: a save that owes a row after this has its note listed by the look or
    // its stamp read by the next search. ~keep
    let owed = rows_owed_here(state).await;
    let lapsed = {
        let mut marks = backfill_marks(state);
        let lapsed = match marks.get(&namespace) {
            Some(Backfill::Level { until, owed: seen, .. }) if *seen == owed && Instant::now() < *until => {
                return false;
            }
            Some(Backfill::Running) => return true,
            Some(Backfill::Level { ttl, owed: seen, .. }) if *seen == owed => Some(*ttl),
            Some(Backfill::Level { .. }) | None => None,
        };
        marks.insert(namespace.clone(), Backfill::Running);
        lapsed
    };
    let mut mark = RunningMark {
        state,
        namespace,
        level: None,
        owed,
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
            let found_work = progress.embedded > 0 || progress.pruned > 0;
            mark.level = progress.level.then(|| level_ttl(lapsed, found_work));
        }
        Err(error) => tracing::warn!(error = %error.message, "memory vector backfill failed; the next search retries"),
    }
    mark.level.is_none()
}

/// What one backfill pass did.
struct Progress {
    embedded: usize,
    pruned: u64,
    /// Every note has a current row: nothing was left for a later call and nothing failed.
    level: bool,
}

/// One bounded pass: remove the rows whose note is gone or has no text, then embed notes that have
/// no current row, at most [`BACKFILL_NOTES_PER_CALL`] and no new batch after
/// [`BACKFILL_TIME_BUDGET`].
///
/// A note can be saved again while its batch is being embedded. The row written here is then of
/// an earlier write: the upsert leaves the later write's row alone, the join never serves this
/// one, and the pass reads the batch's notes once more after writing and does not call the
/// namespace level when one of them moved.
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
        // A note without text has no row to have: one left from when it had text is an orphan.
        let with_text = page.entries.iter().filter(|note| !note.value.is_empty());
        live.extend(with_text.map(|note| note.key.clone()));
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
        #[cfg(test)]
        test_hooks::while_embedding(state).await;
        let rows: Vec<MemoryRow> = embed_notes(state, notes)
            .await
            .into_iter()
            .map(|(note, embedding)| MemoryRow {
                scope: scope.clone(),
                key: note.key,
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
        let written: Vec<(String, i64)> = rows.iter().map(|row| (row.key.clone(), row.updated_at)).collect();
        on_store(&lance, "upsert_memory_rows", move |lance| {
            lance.upsert_memory_rows(&rows)
        })
        .await?;
        level &= still_current(state, vis_byte, owner, written).await?;
    }
    Ok(Progress {
        embedded,
        pruned,
        level,
    })
}

/// Whether every note of `written` (key and the `updated_at` its row was embedded from) is still
/// that write. A note saved again or deleted since has a row here that is behind it.
async fn still_current(
    state: &ServerState,
    vis_byte: u8,
    owner: &str,
    written: Vec<(String, i64)>,
) -> Result<bool, McpError> {
    let keys: Vec<String> = written.iter().map(|(key, _)| key.clone()).collect();
    let now = memory_stamps_wire(state, vis_byte, owner, &keys).await?;
    Ok(written
        .iter()
        .zip(&now)
        .all(|((_, embedded_from), stamp)| *stamp == Some(*embedded_from)))
}

/// A seam inside a backfill pass, between reading a batch's notes and embedding them, where a
/// test saves one of them again.
#[cfg(test)]
pub(super) mod test_hooks {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    use super::ServerState;

    pub(crate) type Hook =
        Box<dyn for<'a> FnOnce(&'a ServerState) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> + Send + Sync>;

    static WHILE_EMBEDDING: Mutex<Option<(String, Hook)>> = Mutex::new(None);

    /// Run `hook` once, in the next backfill pass of the server whose scope is `scope`.
    pub(crate) fn once_while_embedding(scope: &str, hook: Hook) {
        *WHILE_EMBEDDING.lock().unwrap() = Some((scope.to_string(), hook));
    }

    pub(super) async fn while_embedding(state: &ServerState) {
        let hook = WHILE_EMBEDDING
            .lock()
            .unwrap()
            .take_if(|(scope, _)| *scope == state.shared.scope);
        if let Some((_, hook)) = hook {
            hook(state).await;
        }
    }
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

/// The keys of `hits` whose row is current by `stamps`, each once, nearest first.
fn current_keys(hits: &[MemoryHit], stamps: &AHashMap<String, Option<i64>>) -> Vec<String> {
    let mut seen: AHashSet<&str> = AHashSet::new();
    hits.iter()
        .filter(|hit| stamps.get(&hit.key).copied().flatten() == Some(hit.updated_at))
        .filter(|hit| seen.insert(hit.key.as_str()))
        .map(|hit| hit.key.clone())
        .collect()
}

/// The hits [`truth_join`] can decide: those whose note was read whole, and those whose row is
/// not current by `stamps`, which it refuses. A current hit past the notes read cannot be served
/// yet and is not a refused row.
fn decidable(
    hits: &[MemoryHit],
    stamps: &AHashMap<String, Option<i64>>,
    notes: &AHashMap<String, Option<WireMemoryEntry>>,
) -> Vec<MemoryHit> {
    hits.iter()
        .filter(|hit| notes.contains_key(&hit.key) || stamps.get(&hit.key).copied().flatten() != Some(hit.updated_at))
        .cloned()
        .collect()
}

/// How many current keys to read next, having read `wanted` and served `served` of `limit`: at
/// least twice as many, since a tag filter can refuse most of what is read and one more note per
/// round would cost a round per note.
fn read_further(wanted: usize, limit: usize, served: usize) -> usize {
    wanted.saturating_add(limit.saturating_sub(served).max(wanted))
}

/// `memory_search` by meaning: embed the query, bring the rows nearer the notes, and answer the
/// nearest rows from the notes they were embedded from. Which rows are current is read from the
/// notes' stamps; only the notes that may be served are read whole.
pub(super) async fn search(state: &ServerState, params: MemorySearchParams) -> Result<CallToolResult, McpError> {
    let started = Instant::now();
    let limit = params.limit.unwrap_or(10).min(100) as usize;
    let (vis_byte, owner) = memory_namespace(state, params.visibility);
    let lance_vis = lance_visibility(params.visibility);
    let embedding = embed_query(state, &params.query).await?;
    let lance = memory_lance_store(state)
        .await
        .map_err(|unavailable| unavailable.refusal("memory_search", MEMORY_SEARCH_MEANWHILE))?;
    let catching_up = backfill_step(state, params.visibility, vis_byte, owner).await;

    // Rows the join refuses must not shrink the answer: the search fetches more than it serves,
    // and more again while refused rows leave it short and the table has rows left.
    let mut fetch = limit.saturating_mul(2).saturating_add(FETCH_SLACK);
    let mut stamps: AHashMap<String, Option<i64>> = AHashMap::new();
    let mut notes: AHashMap<String, Option<WireMemoryEntry>> = AHashMap::new();
    let joined = loop {
        let (scope, agent_id, query) = (state.shared.scope.clone(), owner.to_string(), embedding.clone());
        let tag = params.tag.clone();
        let raw = on_store(&lance, "search_memory", move |lance| {
            lance.search_memory(&scope, lance_vis, &agent_id, query, fetch, tag.as_deref())
        })
        .await?;
        let exhausted = raw.len() < fetch;
        let mut unstamped: Vec<String> = Vec::new();
        for hit in &raw {
            if !stamps.contains_key(&hit.key) && !unstamped.contains(&hit.key) {
                unstamped.push(hit.key.clone());
            }
        }
        let read = memory_stamps_wire(state, vis_byte, owner, &unstamped).await?;
        stamps.extend(unstamped.into_iter().zip(read));
        let current = current_keys(&raw, &stamps);
        // A note read whole may lack the tag asked for: read further down only while short.
        let mut wanted = limit;
        let joined = loop {
            let unread: Vec<String> = current
                .iter()
                .take(wanted)
                .filter(|key| !notes.contains_key(*key))
                .cloned()
                .collect();
            let read = memory_get_many_wire(state, vis_byte, owner, unread.clone()).await?;
            notes.extend(unread.into_iter().zip(read));
            let joined = truth_join(decidable(&raw, &stamps, &notes), &notes, params.tag.as_deref(), limit);
            if joined.hits.len() >= limit || wanted >= current.len() {
                break joined;
            }
            wanted = read_further(wanted, limit, joined.hits.len());
        };
        if joined.hits.len() >= limit || exhausted || fetch >= FETCH_MAX {
            break joined;
        }
        fetch = fetch.saturating_mul(4).min(FETCH_MAX);
    };
    if joined.dropped > 0 {
        // Rows without a current note are in the table: the next search reconciles again.
        not_level(state, vis_byte, owner);
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
