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

/// One fresh row of a family: its key (`content key ‖ start`), its msgpack value, and the key of
/// its name row, when the name fits a key.
pub(super) struct FreshRow {
    pub(super) row: Vec<u8>,
    pub(super) value: Vec<u8>,
    pub(super) name_key: Option<Vec<u8>>,
}

/// One row family of a content: the rows keyed `content key ‖ start` and the name-led rows that
/// point at them.
pub(super) struct Family<'a> {
    pub(super) rows: &'a Scoped,
    pub(super) names: &'a Scoped,
    /// The name row a stored value points from, re-derived to remove it.
    pub(super) name_of: fn(&ContentKey, &[u8]) -> Option<Vec<u8>>,
}

impl Family<'_> {
    /// Stage `fresh` as the rows of `content` (see the module docs).
    pub(super) fn stage(
        &self,
        batch: &mut OwnedWriteBatch,
        staged_bytes: &mut u64,
        content: &ContentKey,
        fresh: Vec<FreshRow>,
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
        let fresh_names: AHashSet<&[u8]> = fresh.iter().filter_map(|row| row.name_key.as_deref()).collect();
        for (key, value) in &stored {
            if !fresh_rows.contains_key(key.as_slice()) {
                stage_remove(batch, staged_bytes, self.rows, key.clone());
            }
            match (self.name_of)(content, value) {
                Some(name_key) if !fresh_names.contains(name_key.as_slice()) => {
                    stage_remove(batch, staged_bytes, self.names, name_key);
                }
                Some(_) => {}
                None => tracing::warn!("index: a stored content row does not decode; its name row stays"),
            }
        }
        self.insert(batch, staged_bytes, fresh);
        Ok(())
    }

    /// Stage every row of `fresh` and its name row.
    fn insert(&self, batch: &mut OwnedWriteBatch, staged_bytes: &mut u64, fresh: Vec<FreshRow>) {
        for row in fresh {
            stage_insert(batch, staged_bytes, self.rows, row.row, row.value);
            if let Some(name_key) = row.name_key {
                stage_insert(batch, staged_bytes, self.names, name_key, Vec::<u8>::new());
            }
        }
    }
}

/// The `symbols_by_name` key of a stored `symbols_by_path` value.
pub(super) fn symbol_name_key(content: &ContentKey, value: &[u8]) -> Option<Vec<u8>> {
    let symbol: crate::extract::Symbol = rmp_serde::from_slice(value).ok()?;
    super::keys::symbol_by_name(&symbol.name, symbol.kind, content, symbol.start_byte)
}

/// The `calls_by_callee` key of a stored `calls_by_path` value.
pub(super) fn callee_key(content: &ContentKey, value: &[u8]) -> Option<Vec<u8>> {
    let call: crate::extract::Call = rmp_serde::from_slice(value).ok()?;
    super::keys::call_by_callee(&call.callee, content, call.start_byte)
}
