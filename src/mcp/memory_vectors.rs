//! Reconciling memory vector rows with the notes they were embedded from.
//!
//! The Fjall `memory_by_key` record is the truth for a note. Its vector row is written afterwards,
//! in a second store, by a delete and an insert that commit separately, and a delete of the note
//! removes the row on a best-effort basis. So a row can outlive its note or hold the text of an
//! earlier write. Nothing read from a row is served: a hit is kept only when the note still exists
//! and carries the `updated_at` the row was embedded from, and it is answered from the note.

#![cfg(feature = "memory")]

use super::memory_ops::WireMemoryEntry;
use super::types_memory::MemorySearchHit;
use crate::lance::MemoryHit;

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
