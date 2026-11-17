//! Token budgeting of the rows a list writes under their file ([`super::by_path`]). The budget
//! itself and the plain-list form are the engine's ([`crate::engine::budget`]), re-exported here.

pub(super) use crate::engine::budget::{Budgeted, apply_budget};

/// [`apply_budget`] for rows written under their file ([`super::by_path`]). A row costs what it
/// writes: its own fields with its column, plus its file's key the first time a kept row is in that
/// file, since the key is written once per file and not on the row.
pub(super) fn apply_budget_by_path<T: super::by_path::Located>(items: Vec<T>, max_tokens: Option<u32>) -> Budgeted<T> {
    let mut files: ahash::AHashSet<crate::path::RelPath> = ahash::AHashSet::new();
    crate::engine::budget::keep_prefix(items, max_tokens, |row| {
        let mut bytes = Vec::new();
        let row_bytes = match row.serialize_row(&mut serde_json::Serializer::new(&mut bytes), true) {
            Ok(()) => bytes.len() as u64 + 1,
            Err(_) => 0,
        };
        let key_bytes = if files.insert(row.located().clone()) {
            // `"<path>":[` and `],`: the quoted key plus four bytes of punctuation.
            serde_json::to_vec(&row.located().to_str_lossy()).map_or(0, |key| key.len() as u64 + 4)
        } else {
            0
        };
        // Rounded up, so the costs of many small rows do not sum below what they write.
        (row_bytes + key_bytes).div_ceil(4)
    })
}
