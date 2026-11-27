//! Writing one content's symbol or call rows (ADR-0028 rule 2), shared by every view of a group.
//!
//! The rows of a content are a pure function of its content key only while the extractor is
//! unchanged. A grammar or query change ships without an extract-epoch bump, and a content extracted
//! again under it must not leave the earlier extraction's rows served beside the new ones. So a
//! write reconciles: it reads the rows stored under the key (one prefix seek), writes nothing when
//! they equal the fresh ones (the common case: another worktree, or a file re-scanned unchanged),
//! and otherwise removes every stored row, with the name row derived from it, that the fresh set
//! does not hold before writing the fresh set.

use ahash::{AHashMap, AHashSet};
use fjall::OwnedWriteBatch;

use super::IndexError;
use super::keys::ContentKey;
use super::scoped::Scoped;
use super::writer::{stage_insert, stage_remove};

/// One fresh row of a family: its key (`content key ‖ start`) and its msgpack value.
pub(super) struct FreshRow {
    pub(super) row: Vec<u8>,
    pub(super) value: Vec<u8>,
}

/// The fresh rows of one content, with the name rows that point at them as `(key, value)`.
#[derive(Default)]
pub(super) struct Fresh {
    pub(super) rows: Vec<FreshRow>,
    pub(super) names: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Stored `(key, value)` rows of one content.
pub(super) type StoredRows = [(Vec<u8>, Vec<u8>)];

/// The keys of the name rows stored rows point from, and how many rows did not decode.
pub(super) type NameKeys = (AHashSet<Vec<u8>>, usize);

/// One row family of a content: the rows keyed `content key ‖ start` and the name-led rows that
/// point at them.
pub(super) struct Family<'a> {
    pub(super) rows: &'a Scoped,
    pub(super) names: &'a Scoped,
    /// The keys of the name rows stored values point from, re-derived to remove them, and how many
    /// values did not decode.
    pub(super) names_of: fn(&ContentKey, &StoredRows) -> NameKeys,
}

impl Family<'_> {
    /// Stage `fresh` as the rows of `content` (see the module docs).
    pub(super) fn stage(
        &self,
        batch: &mut OwnedWriteBatch,
        staged_bytes: &mut u64,
        content: &ContentKey,
        fresh: Fresh,
    ) -> Result<(), IndexError> {
        let mut stored: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for row in self.rows.prefix(content) {
            let (key, value) = row?;
            stored.push((key.to_vec(), value.to_vec()));
        }
        // A content new to the group, as on a cold scan: nothing to reconcile, so no set is built. ~keep
        if stored.is_empty() {
            self.insert(batch, staged_bytes, fresh);
            return Ok(());
        }
        // A later row at the same start replaces an earlier one, as the batch would. ~keep
        let fresh_rows: AHashMap<&[u8], &[u8]> = fresh
            .rows
            .iter()
            .map(|row| (row.row.as_slice(), row.value.as_slice()))
            .collect();
        if stored.len() == fresh_rows.len()
            && stored
                .iter()
                .all(|(key, value)| fresh_rows.get(key.as_slice()) == Some(&value.as_slice()))
        {
            return Ok(());
        }
        let fresh_names: AHashSet<&[u8]> = fresh.names.iter().map(|(key, _)| key.as_slice()).collect();
        for (key, _) in &stored {
            if !fresh_rows.contains_key(key.as_slice()) {
                stage_remove(batch, staged_bytes, self.rows, key.clone());
            }
        }
        let (stored_names, undecoded) = (self.names_of)(content, &stored);
        if undecoded > 0 {
            tracing::warn!(
                undecoded,
                "index: stored content rows do not decode; their name rows stay"
            );
        }
        for name_key in stored_names {
            if !fresh_names.contains(name_key.as_slice()) {
                stage_remove(batch, staged_bytes, self.names, name_key);
            }
        }
        drop(fresh_rows);
        drop(fresh_names);
        self.insert(batch, staged_bytes, fresh);
        Ok(())
    }

    /// Stage every row of `fresh` and every name row.
    fn insert(&self, batch: &mut OwnedWriteBatch, staged_bytes: &mut u64, fresh: Fresh) {
        for row in fresh.rows {
            stage_insert(batch, staged_bytes, self.rows, row.row, row.value);
        }
        for (key, value) in fresh.names {
            stage_insert(batch, staged_bytes, self.names, key, value);
        }
    }
}

/// The `symbols_by_name` keys of stored `symbols_by_path` rows: one per symbol.
pub(super) fn symbol_name_keys(content: &ContentKey, stored: &StoredRows) -> NameKeys {
    let mut undecoded = 0;
    let mut keys = AHashSet::with_capacity(stored.len());
    for (_, value) in stored {
        match rmp_serde::from_slice::<crate::extract::Symbol>(value) {
            Ok(symbol) => keys.extend(super::keys::symbol_by_name(
                &symbol.name,
                symbol.kind,
                content,
                symbol.start_byte,
            )),
            Err(_) => undecoded += 1,
        }
    }
    (keys, undecoded)
}

/// The `calls_by_callee` keys of stored `calls_by_path` rows: one per callee, its row packing every
/// call of it in the content.
pub(super) fn callee_name_keys(content: &ContentKey, stored: &StoredRows) -> NameKeys {
    let mut undecoded = 0;
    let mut keys = AHashSet::new();
    for (_, value) in stored {
        match rmp_serde::from_slice::<crate::extract::Call>(value) {
            Ok(call) => keys.extend(super::keys::call_name_row(&call.callee, content)),
            Err(_) => undecoded += 1,
        }
    }
    (keys, undecoded)
}
