//! Reconciling memory vector rows with the notes they were embedded from.
//!
//! The Fjall `memory_by_key` record is the truth for a note. Its vector row is written afterwards,
//! in a second store, by a delete and an insert that commit separately, and a delete of the note
//! removes the row on a best-effort basis. So a row can outlive its note or hold the text of an
//! earlier write. Nothing read from a row is served: a hit is kept only when the note still exists
//! and carries the `updated_at` the row was embedded from, and it is answered from the note.
//!
//! The rows are also rebuilt from the notes. The table is wiped whenever its `(dim, model,
//! schema_ver)` changes, which every release minor and every preset change does, and nothing but a
//! fresh save used to put a row back: after an upgrade a search by meaning found nothing while the
//! notes were all still there. The first search of a namespace in a process embeds every note whose
//! row is missing or stale ([`backfill_once`]).
//!
//! The table lives beside the notes, in the repo group directory ([`memory_lance_dir`]), so a note
//! saved from one worktree is found from every other.

#![cfg(feature = "memory")]

use std::path::{Path, PathBuf};

use ahash::AHashMap;
use rmcp::ErrorData as McpError;

use super::ServerState;
use super::memory::{embed_query, lance_visibility, memory_get_wire, memory_lance_store, memory_list_page};
use super::memory_ops::WireMemoryEntry;
use super::types_memory::{MemorySearchHit, Visibility};
use crate::lance::{MemoryHit, MemoryRow};

/// Directory of the memory vector store under a repo group dir.
const MEMORY_LANCE_DIR: &str = "memory.lance";
/// Notes listed per page while looking for the ones without a current row.
const BACKFILL_PAGE: usize = 500;

/// The repo group's memory vector store: `<repo_group_dir>/memory.lance`, beside `memory.fjall`.
/// Per group, not per workspace, because the notes are: a row written from one worktree must be
/// found from another. Like everything under the group directory, no garbage collector lists it.
pub(super) fn memory_lance_dir(root: &Path) -> PathBuf {
    crate::store::repo_group_dir(root).join(MEMORY_LANCE_DIR)
}

/// The notes whose vector row is missing or was embedded from an earlier write, given the
/// `key -> updated_at` of the rows the table holds.
pub(super) fn notes_behind(notes: Vec<WireMemoryEntry>, stamps: &AHashMap<String, i64>) -> Vec<WireMemoryEntry> {
    notes
        .into_iter()
        .filter(|note| stamps.get(&note.key) != Some(&note.updated_at))
        .collect()
}

/// Bring one namespace's vector rows up to date with its notes, once per process. Best effort: a
/// failure is logged and the next search tries again, and the search that triggered it still runs
/// over whatever rows exist.
pub(super) async fn backfill_once(state: &ServerState, visibility: Visibility, vis_byte: u8, owner: &str) {
    let namespace = (vis_byte, owner.to_string());
    let first = state
        .shared
        .memory_backfilled
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(namespace.clone());
    if !first {
        return;
    }
    match backfill(state, visibility, vis_byte, owner).await {
        Ok(0) => {}
        Ok(embedded) => tracing::info!(embedded, "re-embedded memory notes that had no current vector row"),
        Err(error) => {
            tracing::warn!(error = %error.message, "memory vector backfill failed; the next search retries");
            state
                .shared
                .memory_backfilled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&namespace);
        }
    }
}

/// Embed every note of the namespace that has no current row. Returns how many rows were written.
async fn backfill(state: &ServerState, visibility: Visibility, vis_byte: u8, owner: &str) -> Result<usize, McpError> {
    let lance = memory_lance_store(state).await?;
    let scope = state.shared.scope.clone();
    let lance_vis = lance_visibility(visibility);
    let stamps = {
        let (lance, scope, agent_id) = (lance.clone(), scope.clone(), owner.to_string());
        tokio::task::spawn_blocking(move || lance.memory_stamps(&scope, lance_vis, &agent_id))
            .await
            .map_err(|e| McpError::internal_error(format!("spawn_blocking: {e}"), None))?
            .map_err(|e| McpError::internal_error(format!("memory_stamps: {e}"), None))?
    };
    let mut behind = Vec::new();
    let mut cursor = None;
    loop {
        let page = memory_list_page(state, vis_byte, owner, "", None, BACKFILL_PAGE, cursor).await?;
        behind.extend(notes_behind(page.entries, &stamps));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    let mut embedded = 0;
    for listed in behind {
        // A listed value is a preview; the row is embedded from the whole note, read again so a
        // note rewritten or deleted since the listing is embedded as it now stands, or not at all.
        let Some(note) = memory_get_wire(state, vis_byte, owner, &listed.key).await? else {
            continue;
        };
        let embedding = match embed_query(state, &note.value).await {
            Ok(embedding) => embedding,
            Err(error) => {
                tracing::warn!(key = %note.key, error = %error.message, "memory note could not be embedded");
                continue;
            }
        };
        let row = MemoryRow {
            scope: scope.clone(),
            key: note.key,
            value: note.value,
            tags: note.tags,
            visibility: lance_vis.to_string(),
            agent_id: owner.to_string(),
            embedding,
            created_at: note.created_at,
            updated_at: note.updated_at,
        };
        let lance = lance.clone();
        tokio::task::spawn_blocking(move || lance.upsert_memory(row))
            .await
            .map_err(|e| McpError::internal_error(format!("spawn_blocking: {e}"), None))?
            .map_err(|e| McpError::internal_error(format!("upsert_memory: {e}"), None))?;
        embedded += 1;
    }
    Ok(embedded)
}

/// Join vector hits against the notes read for their keys (`notes[i]` is the note for `hits[i]`),
/// dropping every hit whose note is gone or was rewritten since the row was embedded.
pub(super) fn truth_join(
    hits: Vec<MemoryHit>,
    notes: Vec<Option<WireMemoryEntry>>,
    limit: usize,
) -> Vec<MemorySearchHit> {
    hits.into_iter()
        .zip(notes)
        .filter_map(|(hit, note)| {
            let note = note.filter(|note| note.updated_at == hit.updated_at)?;
            Some(MemorySearchHit {
                key: note.key,
                value: note.value,
                tags: note.tags,
                distance: hit.distance,
            })
        })
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(key: &str, value: &str, updated_at: i64) -> MemoryHit {
        MemoryHit {
            key: key.into(),
            value: value.into(),
            tags: vec!["row".into()],
            updated_at,
            distance: 0.5,
        }
    }

    fn note(key: &str, value: &str, updated_at: i64) -> Option<WireMemoryEntry> {
        Some(WireMemoryEntry {
            key: key.into(),
            value: value.into(),
            tags: vec!["note".into()],
            created_at: 1,
            updated_at,
        })
    }

    /// After the table is wiped (a release minor, a preset change) no note has a row, and every one
    /// of them must be embedded again; so must a note rewritten since its row was embedded.
    #[test]
    fn notes_without_a_current_row_are_the_ones_to_embed() {
        let notes = || {
            vec![
                note("fresh", "a", 1),
                note("rewritten", "b", 9),
                note("rowless", "c", 3),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
        };
        let keys = |behind: Vec<WireMemoryEntry>| behind.into_iter().map(|note| note.key).collect::<Vec<_>>();

        let wiped = AHashMap::new();
        assert_eq!(keys(notes_behind(notes(), &wiped)), ["fresh", "rewritten", "rowless"]);

        let stamps: AHashMap<String, i64> = [("fresh".to_string(), 1), ("rewritten".to_string(), 5)].into();
        assert_eq!(keys(notes_behind(notes(), &stamps)), ["rewritten", "rowless"]);
    }

    /// The vectors live with the notes, per repo group: under a workspace directory a note saved
    /// from one worktree could not be found from another.
    #[test]
    fn the_memory_vector_store_lives_in_the_repo_group_directory() {
        crate::store::init_isolated_cache();
        let root = tempfile::tempdir().unwrap();
        let dir = memory_lance_dir(root.path());
        assert!(
            dir.starts_with(crate::store::repo_group_dir(root.path())),
            "{}",
            dir.display()
        );
        assert_eq!(dir.parent(), crate::store::memory_store_dir(root.path()).parent());
        assert!(!dir.starts_with(crate::store::workspace_cache_dir(root.path())));
    }

    #[test]
    fn a_row_whose_note_was_deleted_is_not_served() {
        let joined = truth_join(vec![hit("gone", "resurrected", 5)], vec![None], 10);
        assert!(
            joined.is_empty(),
            "a deleted note must not come back through its vector row"
        );
    }

    #[test]
    fn a_row_left_behind_by_a_newer_write_is_not_served() {
        let joined = truth_join(vec![hit("k", "old text", 5)], vec![note("k", "new text", 9)], 10);
        assert!(joined.is_empty(), "the row holds the text of an earlier write");
    }

    #[test]
    fn a_current_row_is_answered_from_the_note() {
        let joined = truth_join(vec![hit("k", "row text", 9)], vec![note("k", "note text", 9)], 10);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].value, "note text");
        assert_eq!(joined[0].tags, ["note"]);
        assert_eq!(joined[0].distance, 0.5);
    }

    #[test]
    fn dropped_rows_do_not_count_against_the_limit() {
        let hits = vec![hit("stale", "x", 1), hit("a", "x", 2), hit("b", "x", 3)];
        let notes = vec![note("stale", "y", 8), note("a", "a", 2), note("b", "b", 3)];
        let joined = truth_join(hits, notes, 1);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].key, "a");
    }
}
