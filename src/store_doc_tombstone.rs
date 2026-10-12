//! Tombstones for documents whose extraction timed out.
//!
//! A document that overruns `[documents] extraction_timeout_secs` leaves no blob and no
//! [`crate::store::DocEntry`], so without a record every scan would pay the full timeout for it again
//! (and strand another abandoned thread). A tombstone remembers "this exact content, under these
//! extraction limits, timed out", so the next scan skips it while nothing relevant changed.
//!
//! A tombstone only applies when both halves match:
//!
//! - the **content hash**: editing the file retries it;
//! - the **guard digest** ([`crate::config::rules::doc_guard_digest`]): the per-file timeout, the
//!   chunker cutovers, `max_pages` and the document extraction settings. Raising the timeout (or
//!   changing any of the others) retries it; so does lowering it, which is harmless.
//!
//! Stored in `Index::doc_tombstones`, keyed by path so a deleted file's tombstone can be pruned with it.
//! The field is `#[serde(default)]`: indexes written before it existed deserialize with no tombstones
//! (additive, no schema bump), and an older basemind reading a newer index ignores the field.

use serde::{Deserialize, Serialize};

use crate::path::RelPath;
use crate::store::Store;

/// Why an unchanged document is not re-extracted: it already timed out under these limits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocTombstone {
    /// Content hash (hex) of the document that timed out.
    pub hash_hex: String,
    /// [`crate::config::rules::doc_guard_digest`] of the limits it timed out under.
    pub guard_digest: String,
    /// The per-file budget it overran, in seconds. Informational: shown in logs, not compared.
    pub timeout_secs: u64,
}

impl Store {
    /// Record (or replace) the tombstone for `rel`.
    pub fn upsert_doc_tombstone(&mut self, rel: impl Into<RelPath>, tombstone: DocTombstone) {
        self.index.doc_tombstones.insert(rel.into(), tombstone);
    }

    /// Drop the tombstone for `rel`; true when there was one.
    pub fn remove_doc_tombstone(&mut self, rel: impl AsRef<[u8]>) -> bool {
        self.index
            .doc_tombstones
            .remove(bstr::BStr::new(rel.as_ref()))
            .is_some()
    }

    /// True when `rel` has a tombstone, whatever content it was recorded for.
    pub fn has_doc_tombstone(&self, rel: impl AsRef<[u8]>) -> bool {
        self.index.doc_tombstones.contains_key(bstr::BStr::new(rel.as_ref()))
    }

    /// The tombstone for `rel`, if any.
    pub fn lookup_doc_tombstone(&self, rel: impl AsRef<[u8]>) -> Option<&DocTombstone> {
        self.index.doc_tombstones.get(bstr::BStr::new(rel.as_ref()))
    }

    /// True when `rel` holding content `hash_hex` already timed out under the limits `guard_digest`
    /// describes, so extracting it again would only repeat the timeout.
    pub fn doc_tombstoned(&self, rel: impl AsRef<[u8]>, hash_hex: &str, guard_digest: &str) -> bool {
        self.lookup_doc_tombstone(rel)
            .is_some_and(|t| t.hash_hex == hash_hex && t.guard_digest == guard_digest)
    }

    /// Keep only the tombstones whose path satisfies `keep` (the full scan drops the ones whose file
    /// no longer exists).
    pub fn retain_doc_tombstones(&mut self, mut keep: impl FnMut(&RelPath) -> bool) {
        self.index.doc_tombstones.retain(|rel, _| keep(rel));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Index, VIEW_WORKING};

    fn tombstone(hash: &str, digest: &str) -> DocTombstone {
        DocTombstone {
            hash_hex: hash.to_string(),
            guard_digest: digest.to_string(),
            timeout_secs: 120,
        }
    }

    fn store() -> (tempfile::TempDir, Store) {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), VIEW_WORKING).unwrap();
        (dir, store)
    }

    #[test]
    fn a_tombstone_applies_only_to_the_same_content_and_limits() {
        let (_dir, mut store) = store();
        store.upsert_doc_tombstone("big.csv", tombstone("aa", "g1"));
        assert!(store.doc_tombstoned("big.csv", "aa", "g1"));
        assert!(!store.doc_tombstoned("big.csv", "bb", "g1"), "edited file retries");
        assert!(!store.doc_tombstoned("big.csv", "aa", "g2"), "changed limits retry");
        assert!(!store.doc_tombstoned("other.csv", "aa", "g1"));
    }

    #[test]
    fn tombstones_are_removed_and_pruned() {
        let (_dir, mut store) = store();
        store.upsert_doc_tombstone("keep.csv", tombstone("aa", "g"));
        store.upsert_doc_tombstone("gone.csv", tombstone("bb", "g"));
        assert!(store.remove_doc_tombstone("keep.csv"));
        assert!(!store.remove_doc_tombstone("keep.csv"));
        store.upsert_doc_tombstone("keep.csv", tombstone("aa", "g"));
        store.retain_doc_tombstones(|rel| rel.to_str_lossy() != "gone.csv");
        assert!(store.has_doc_tombstone("keep.csv"));
        assert!(!store.has_doc_tombstone("gone.csv"));
    }

    #[test]
    fn tombstones_survive_a_flush_and_old_indexes_read_without_them() {
        let (dir, mut store) = store();
        store.upsert_doc_tombstone("big.csv", tombstone("aa", "g1"));
        store.flush().unwrap();
        drop(store);
        let reopened = Store::open(dir.path(), VIEW_WORKING).unwrap();
        assert!(reopened.doc_tombstoned("big.csv", "aa", "g1"));

        // An index written before the field existed has no `doc_tombstones` key at all.
        let legacy = serde_json::json!({ "schema_ver": crate::extract::SCHEMA_VER, "files": {} });
        let index: Index = serde_json::from_value(legacy).unwrap();
        assert!(index.doc_tombstones.is_empty());
    }
}
