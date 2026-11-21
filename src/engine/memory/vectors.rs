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
//! The table is empty after a change of its layout version (`lance::VECTOR_STORE_VER`, which no
//! longer follows the release minor) and for every embedding preset opened for the first time.
//!
//! A row holds a note's vector and tags, never its text.
//!
//! The store lives beside the notes, in the repo group directory, one directory per embedding model
//! and dimension ([`memory_lance_dir`]): a note saved from one worktree is found from every other,
//! and two worktrees configured with different presets each keep their own table.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ahash::{AHashMap, AHashSet};

use super::lance_visibility;
use super::ops::{MEMORY_PREVIEW_CHARS, WireMemoryEntry};
use crate::engine::timing::elapsed_us;
use crate::engine::types::memory::{
    MemoryPutParams, MemorySearchHit, MemorySearchParams, MemorySearchResponse, Visibility,
};
use crate::engine::{EngineError, Workspace};
use crate::lance::{LanceStore, MemoryHit, MemoryRow, MemoryStamps};

/// Notes listed per page while looking for the ones without a current row.
const BACKFILL_PAGE: usize = 500;
/// Notes embedded by one model call and written by one upsert.
const BACKFILL_BATCH: usize = 32;
/// Most notes one search embeds before it answers; the next search continues.
pub(crate) const BACKFILL_NOTES_PER_CALL: usize = if cfg!(test) { 5 } else { 256 };
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
pub(crate) fn rows_owed(root: &Path, table: &str) -> u64 {
    let group = std::fs::read(rows_owed_path(root)).unwrap_or_default();
    let own = std::fs::read(table_owed_path(root, table)).unwrap_or_default();
    if group.is_empty() && own.is_empty() {
        return 0;
    }
    let hash = crate::hashing::hash_bytes(&[group.as_slice(), b"\n", own.as_slice()].concat());
    u64::from_le_bytes([hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7]]) | 1
}

/// [`rows_owed`] of the table this process searches.
pub(crate) fn rows_owed_here(ws: &Workspace) -> u64 {
    rows_owed(&ws.root, &table_name(ws).unwrap_or_default())
}

/// The directory name of the table this process's embedder writes, or `None` without an embedder.
fn table_name(ws: &Workspace) -> Option<String> {
    let embedder = ws.shared_embedder().ok()?;
    let dir = memory_lance_dir(&ws.root, embedder.model(), embedder.dim());
    dir.file_name().and_then(|name| name.to_str()).map(str::to_string)
}

/// Tell every process with the group's notes open that a note has no row of its latest write.
/// Best effort: a stamp that cannot be written leaves other processes to find the note when their
/// belief lapses.
pub(crate) fn owe_rows(root: &Path) {
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
pub(crate) fn owe_rows_elsewhere(ws: &Workspace) {
    let Some(own) = table_name(ws) else {
        return;
    };
    let Ok(tables) = std::fs::read_dir(crate::store_layout::memory_vectors_dir(&ws.root)) else {
        return;
    };
    for table in tables.flatten() {
        let name = table.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name != own && table.file_type().is_ok_and(|kind| kind.is_dir()) {
            write_owed_stamp(&table_owed_path(&ws.root, name));
        }
    }
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
pub(crate) fn not_level(ws: &Workspace, vis_byte: u8, owner: &str) {
    let mut marks = backfill_marks(ws);
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
pub(crate) fn memory_lance_dir(root: &Path, model: &str, dim: u16) -> PathBuf {
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
    crate::store_layout::memory_vectors_dir(root).join(format!("{slug}-{dim}-{}", &hash[..8]))
}

/// Open the repo group's memory vector store for the configured embedder.
pub(crate) fn memory_lance_store(ws: &Workspace) -> Result<Arc<LanceStore>, crate::engine::lance_cell::Unavailable> {
    ws.memory_lance.get_or_open(|| {
        let embedder = ws.shared_embedder().map_err(|e| format!("embedder init: {e}"))?;
        let (model, dim) = (embedder.model(), embedder.dim());
        let dir = memory_lance_dir(&ws.root, model, dim);
        LanceStore::open(&dir, dim, model)
            .map(Arc::new)
            .map_err(|e| format!("open LanceStore: {e}"))
    })
}

/// The store, when a row could be in it: already open in this process, or present on disk. A
/// store that was never created holds no row and is not created to find that out.
fn memory_lance_store_if_any(ws: &Workspace) -> Option<Arc<LanceStore>> {
    if let Some(lance) = ws.memory_lance.get() {
        return Some(Arc::clone(lance));
    }
    let embedder = ws.shared_embedder().ok()?;
    if !memory_lance_dir(&ws.root, embedder.model(), embedder.dim()).is_dir() {
        return None;
    }
    match memory_lance_store(ws) {
        Ok(lance) => Some(lance),
        Err(error) => {
            tracing::warn!(%error, "memory vector store could not be opened");
            None
        }
    }
}

/// Run `work` against the store, naming the store call `what` in its error.
fn on_store<T>(
    lance: &LanceStore,
    what: &'static str,
    work: impl FnOnce(&LanceStore) -> anyhow::Result<T>,
) -> Result<T, EngineError> {
    work(lance).map_err(|e| EngineError::internal(format!("{what}: {e}")))
}

/// Embed a saved note and upsert its vector row.
pub(crate) fn embed_note(
    ws: &Workspace,
    params: &MemoryPutParams,
    owner: &str,
    tags: Vec<String>,
    created_at: i64,
    updated_at: i64,
) -> Result<(), String> {
    let embedding = ws.embed_passage(&params.value).map_err(|error| error.to_string())?;
    let lance = memory_lance_store(ws).map_err(|error| error.to_string())?;
    let row = MemoryRow {
        scope: ws.scope.clone(),
        key: params.key.clone(),
        tags,
        visibility: lance_visibility(params.visibility).to_string(),
        agent_id: owner.to_string(),
        embedding,
        created_at,
        updated_at,
    };
    on_store(&lance, "upsert_memory", move |lance| lance.upsert_memory(row)).map_err(|error| error.to_string())
}

/// Best-effort removal of one note's vector row from the configured model's store. The fjall
/// record is the source of truth; a row that cannot be removed is logged, never an error, and the
/// join keeps it from being served.
pub(crate) fn delete_vector_row(ws: &Workspace, visibility: Visibility, owner: &str, key: &str) {
    let Some(lance) = memory_lance_store_if_any(ws) else {
        return;
    };
    let visibility = lance_visibility(visibility);
    let deleted = on_store(&lance, "delete_memory", |lance| {
        lance.delete_memory(&ws.scope, visibility, owner, key)
    });
    if let Err(error) = deleted {
        tracing::warn!(%key, %error, "the note's vector row was not removed");
    }
}

/// How many of the repo group's vector stores a purge reached.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PurgeOutcome {
    /// Stores the purge completed on.
    pub purged: usize,
    /// Stores that could not be opened or purged; their rows may remain, and the purge can be run again.
    pub failed: usize,
}

/// Removal of a deleted note's rows from every model's store of the repo group, each table purged
/// after ([`LanceStore::purge_memory`]): a store of a preset no longer configured still holds the
/// rows embedded under it, and a delete only hides a row until the table's files are rewritten. A
/// store that fails is logged and counted in the outcome, never an error: the note is already gone.
pub(crate) fn purge_vector_rows(ws: &Workspace, visibility: Visibility, owner: &str, key: &str) -> PurgeOutcome {
    let visibility = lance_visibility(visibility);
    on_every_store(ws, "purge_memory", |lance| {
        lance.purge_memory(&ws.scope, visibility, owner, key)
    })
}

/// Removal of every row of every model's store of the repo group, each purged after
/// ([`LanceStore::purge_all_memory`]).
pub(crate) fn purge_every_vector_row(ws: &Workspace) -> PurgeOutcome {
    on_every_store(ws, "purge_all_memory", LanceStore::purge_all_memory)
}

/// Run `work` on every model's store under the group's
/// [`crate::store_layout::memory_vectors_dir`], the one this process has open through its handle.
/// A store that cannot be opened, or on which `work` fails, is logged and counted as failed.
fn on_every_store(
    ws: &Workspace,
    what: &'static str,
    work: impl Fn(&LanceStore) -> anyhow::Result<u64>,
) -> PurgeOutcome {
    let Ok(tables) = std::fs::read_dir(crate::store_layout::memory_vectors_dir(&ws.root)) else {
        return PurgeOutcome::default();
    };
    let mut outcome = PurgeOutcome::default();
    for table in tables.flatten() {
        let dir = table.path();
        if !table.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let lance = match ws.memory_lance.get().filter(|lance| lance.dir() == dir.as_path()) {
            Some(lance) => Arc::clone(lance),
            None => match LanceStore::open_recorded(&dir) {
                Ok(Some(lance)) => Arc::new(lance),
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(%error, store = %dir.display(), what, "a memory vector store was not opened");
                    outcome.failed += 1;
                    continue;
                }
            },
        };
        match on_store(&lance, what, &work) {
            Ok(_) => outcome.purged += 1,
            Err(error) => {
                tracing::warn!(%error, store = %dir.display(), "a memory vector store was not purged");
                outcome.failed += 1;
            }
        }
    }
    outcome
}

/// Forget which namespaces are level with their rows, so the next search of each looks again. A
/// clear purges rows after it removes the notes; a save that lands between the two keeps its note
/// but loses its row, which a `Level` mark would hide for up to its lifetime.
pub(crate) fn forget_backfill_marks(ws: &Workspace) {
    backfill_marks(ws).clear();
}

/// The notes to embed, given the rows the table holds: those with no row, a row embedded from an
/// earlier write, or more than one row. A note with no text cannot be embedded and is never behind.
pub(crate) fn notes_behind(notes: Vec<WireMemoryEntry>, rows: &MemoryStamps) -> Vec<WireMemoryEntry> {
    notes
        .into_iter()
        .filter(|note| !note.value.is_empty())
        .filter(|note| rows.stamps.get(&note.key) != Some(&note.updated_at) || rows.duplicated.contains(&note.key))
        .collect()
}

/// Drops the `Running` mark of a backfill that did not reach `Level`, however it ended.
struct RunningMark<'a> {
    ws: &'a Workspace,
    namespace: (u8, String),
    /// How long the namespace is level for, once the pass found it level.
    level: Option<Duration>,
    /// The group's [`rows_owed`] read before the pass looked.
    owed: u64,
}

impl Drop for RunningMark<'_> {
    fn drop(&mut self) {
        let mut marks = backfill_marks(self.ws);
        if let Some(ttl) = self.level {
            marks.insert(self.namespace.clone(), Backfill::level(ttl, self.owed));
        } else {
            marks.remove(&self.namespace);
        }
    }
}

fn backfill_marks(ws: &Workspace) -> std::sync::MutexGuard<'_, AHashMap<(u8, String), Backfill>> {
    ws.memory_backfill
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Bring one namespace's rows nearer its notes, unless they are level or another search is at it.
/// Returns whether the rows are still behind. Best effort: a failure is logged, the namespace stays
/// unmarked so the next search tries again, and the search that triggered it still runs over
/// whatever rows exist.
pub(crate) fn backfill_step(ws: &Workspace, visibility: Visibility, vis_byte: u8, owner: &str) -> bool {
    let namespace = (vis_byte, owner.to_string());
    // Read before the look: a save that owes a row after this has its note listed by the look or
    // its stamp read by the next search. ~keep
    let owed = rows_owed_here(ws);
    let lapsed = {
        let mut marks = backfill_marks(ws);
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
        ws,
        namespace,
        level: None,
        owed,
    };
    match backfill(ws, visibility, vis_byte, owner) {
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
        Err(error) => tracing::warn!(%error, "memory vector backfill failed; the next search retries"),
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
fn backfill(ws: &Workspace, visibility: Visibility, vis_byte: u8, owner: &str) -> Result<Progress, EngineError> {
    let lance = memory_lance_store(ws)?;
    let scope = &ws.scope;
    let lance_vis = lance_visibility(visibility);
    let listed_at = crate::lance::now_micros();
    let rows = on_store(&lance, "memory_stamps", |lance| {
        lance.memory_stamps(scope, lance_vis, owner)
    })?;
    let mut live: AHashSet<String> = AHashSet::new();
    let mut behind = Vec::new();
    let mut cursor = None;
    loop {
        let page = ws.memory_list_page(vis_byte, owner, "", &[], BACKFILL_PAGE, cursor)?;
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
        // Bounded by the instant the rows were listed: a note saved since has a newer row.
        on_store(&lance, "prune memory rows", |lance| {
            let keys: Vec<&str> = orphans.iter().map(String::as_str).collect();
            lance.delete_memory_keys(scope, lance_vis, owner, &keys, Some(listed_at))
        })?
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
        let notes = whole_notes(ws, vis_byte, owner, batch)?;
        let wanted = notes.len();
        #[cfg(test)]
        test_hooks::while_embedding(ws);
        let rows: Vec<MemoryRow> = embed_notes(ws, notes)
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
        on_store(&lance, "upsert_memory_rows", |lance| lance.upsert_memory_rows(&rows))?;
        level &= still_current(ws, vis_byte, owner, written)?;
    }
    Ok(Progress {
        embedded,
        pruned,
        level,
    })
}

/// Whether every note of `written` (key and the `updated_at` its row was embedded from) is still
/// that write. A note saved again or deleted since has a row here that is behind it.
fn still_current(ws: &Workspace, vis_byte: u8, owner: &str, written: Vec<(String, i64)>) -> Result<bool, EngineError> {
    let keys: Vec<String> = written.iter().map(|(key, _)| key.clone()).collect();
    let now = ws.memory_stamps_wire(vis_byte, owner, &keys)?;
    Ok(written
        .iter()
        .zip(&now)
        .all(|((_, embedded_from), stamp)| *stamp == Some(*embedded_from)))
}

/// A seam inside a backfill pass, between reading a batch's notes and embedding them, where a
/// test saves one of them again.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::sync::Mutex;

    use crate::engine::Workspace;

    pub(crate) type Hook = Box<dyn FnOnce(&Workspace) + Send + Sync>;

    static WHILE_EMBEDDING: Mutex<Option<(String, Hook)>> = Mutex::new(None);

    /// Run `hook` once, in the next backfill pass of the workspace whose scope is `scope`.
    pub(crate) fn once_while_embedding(scope: &str, hook: Hook) {
        *WHILE_EMBEDDING.lock().unwrap() = Some((scope.to_string(), hook));
    }

    pub(super) fn while_embedding(ws: &Workspace) {
        let hook = WHILE_EMBEDDING.lock().unwrap().take_if(|(scope, _)| *scope == ws.scope);
        if let Some((_, hook)) = hook {
            hook(ws);
        }
    }
}

/// The notes of `listed` with their whole text. A listing truncates a long value to a preview;
/// only those notes are read again, and one deleted since the listing is left out.
pub(crate) fn whole_notes(
    ws: &Workspace,
    vis_byte: u8,
    owner: &str,
    listed: &[WireMemoryEntry],
) -> Result<Vec<WireMemoryEntry>, EngineError> {
    let clipped: Vec<String> = listed
        .iter()
        .filter(|note| note.value.len() > MEMORY_PREVIEW_CHARS)
        .map(|note| note.key.clone())
        .collect();
    let mut whole = ws.memory_get_many_wire(vis_byte, owner, clipped)?.into_iter();
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
fn embed_notes(ws: &Workspace, notes: Vec<WireMemoryEntry>) -> Vec<(WireMemoryEntry, Vec<f32>)> {
    let batch = ws.shared_embedder().and_then(|embedder| {
        let refs: Vec<&str> = notes.iter().map(|note| note.value.as_str()).collect();
        embedder.embed_batch(&refs).map_err(|e| e.to_string())
    });
    match batch {
        Ok(embeddings) if embeddings.len() == notes.len() => notes.into_iter().zip(embeddings).collect(),
        _ => {
            let mut embedded = Vec::with_capacity(notes.len());
            for note in notes {
                match ws.embed_passage(&note.value) {
                    Ok(embedding) => embedded.push((note, embedding)),
                    Err(error) => {
                        tracing::warn!(key = %note.key, %error, "memory note could not be embedded");
                    }
                }
            }
            embedded
        }
    }
}

/// The hits a search serves, and how many rows the join refused.
pub(crate) struct Joined {
    pub hits: Vec<MemorySearchHit>,
    /// Rows whose note is gone or rewritten, and second rows of a key. Rows the `tags` filter
    /// excluded are not counted: they are current.
    pub dropped: usize,
}

/// Join vector hits against the notes read for their keys. A hit is served once per key, from the
/// note, and only when the note exists, carries the `updated_at` the row was embedded from and has
/// one of `tags` when they are asked for.
pub(crate) fn truth_join(
    hits: Vec<MemoryHit>,
    notes: &AHashMap<String, Option<WireMemoryEntry>>,
    tags: &[String],
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
        if !super::ops::carries_any(&note.tags, tags) {
            continue;
        }
        if joined.hits.len() < limit {
            served.insert(hit.key);
            joined.hits.push(MemorySearchHit {
                key: note.key.clone(),
                value: super::ops::preview(&note.value),
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

/// Squared L2 distance between two unit vectors, which every preset embeds, past which a note is
/// taken to be unrelated to the query: a cosine similarity below one half.
pub(crate) const UNRELATED_DISTANCE: f32 = 1.0;

/// What a search by meaning says when it left out hits past [`UNRELATED_DISTANCE`].
fn unrelated_notice(left_out: usize) -> String {
    format!(
        "{left_out} of the nearest notes are farther from the query than the relevance floor (distance \
         {UNRELATED_DISTANCE:.1}) and are left out: they are unlikely to be about it. A listing by `prefix` or \
         `tag` shows every note"
    )
}

/// `memory_search` by meaning: embed the query, bring the rows nearer the notes, and answer the
/// nearest rows within [`UNRELATED_DISTANCE`] from the notes they were embedded from. Which rows
/// are current is read from the notes' stamps; only the notes that may be served are read whole.
/// A query no model can embed is matched by its words ([`super::lexical`]).
pub(crate) fn search(ws: &Workspace, params: MemorySearchParams) -> Result<MemorySearchResponse, EngineError> {
    let started = Instant::now();
    let limit = params.limit.unwrap_or(10).min(100) as usize;
    let (vis_byte, owner) = ws.memory_namespace(params.visibility);
    let lance_vis = lance_visibility(params.visibility);
    let embedding = match ws.embed_query(&params.query) {
        Ok(embedding) => embedding,
        Err(error @ EngineError::InvalidArgument(_)) => return Err(error),
        Err(error) => return super::lexical::search(ws, params, &error.to_string(), started),
    };
    let lance =
        memory_lance_store(ws).map_err(|unavailable| unavailable.refusal("memory_search", MEMORY_SEARCH_MEANWHILE))?;
    let catching_up = backfill_step(ws, params.visibility, vis_byte, owner);

    // Rows the join refuses must not shrink the answer: the search fetches more than it serves,
    // and more again while refused rows leave it short and the table has rows left.
    let mut fetch = limit.saturating_mul(2).saturating_add(FETCH_SLACK);
    let mut stamps: AHashMap<String, Option<i64>> = AHashMap::new();
    let mut notes: AHashMap<String, Option<WireMemoryEntry>> = AHashMap::new();
    let joined = loop {
        let raw = on_store(&lance, "search_memory", |lance| {
            lance.search_memory(&ws.scope, lance_vis, owner, embedding.clone(), fetch, &params.tags)
        })?;
        // Rows come nearest first: past one beyond the floor, every further row is beyond it too.
        let exhausted = raw.len() < fetch || raw.last().is_some_and(|hit| hit.distance > UNRELATED_DISTANCE);
        let mut unstamped: Vec<String> = Vec::new();
        for hit in &raw {
            if !stamps.contains_key(&hit.key) && !unstamped.contains(&hit.key) {
                unstamped.push(hit.key.clone());
            }
        }
        let read = ws.memory_stamps_wire(vis_byte, owner, &unstamped)?;
        stamps.extend(unstamped.into_iter().zip(read));
        let current = current_keys(&raw, &stamps);
        // A note read whole may lack the tags asked for: read further down only while short.
        let mut wanted = limit;
        let joined = loop {
            let unread: Vec<String> = current
                .iter()
                .take(wanted)
                .filter(|key| !notes.contains_key(*key))
                .cloned()
                .collect();
            let read = ws.memory_get_many_wire(vis_byte, owner, unread.clone())?;
            notes.extend(unread.into_iter().zip(read));
            let joined = truth_join(decidable(&raw, &stamps, &notes), &notes, &params.tags, limit);
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
        not_level(ws, vis_byte, owner);
    }
    let (hits, unrelated): (Vec<MemorySearchHit>, Vec<MemorySearchHit>) = joined
        .hits
        .into_iter()
        .partition(|hit| hit.distance <= UNRELATED_DISTANCE);
    let notices: Vec<String> = [
        catching_up.then(|| CATCHING_UP.to_string()),
        (!unrelated.is_empty()).then(|| unrelated_notice(unrelated.len())),
    ]
    .into_iter()
    .flatten()
    .collect();
    Ok(MemorySearchResponse {
        query: params.query,
        hits,
        lexical: false,
        notice: (!notices.is_empty()).then(|| notices.join(". ")),
        elapsed_us: elapsed_us(started),
    })
}

#[cfg(test)]
#[path = "vectors_tests.rs"]
mod tests;
