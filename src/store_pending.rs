//! The files whose vector rows the file map cannot vouch for yet.
//!
//! A scan records a file's entry, flushes the file map, and only then writes the file's vector rows
//! to LanceDB. A process killed in between, or a LanceDB write that fails, would leave an entry that
//! reads as embedded over rows that do not exist, and every later scan would skip the file as
//! unchanged. So the paths whose rows are still owed are kept in a small file beside the file map,
//! [`PENDING_FILE`]: written (temp file, fsync, rename) before the file map that claims those
//! files, and rewritten, or removed once empty, after the rows are in. A scan re-queues whatever it
//! finds there from the cached blobs before it writes its own rows.
//!
//! The set is its own file, not a field of the file map, so settling a path costs one small write
//! and never a second seal of the index and rewrite of the whole map.
//!
//! Beside it, [`STORE_ID_FILE`] names the LanceDB store the view's rows were last written to
//! ([`crate::lance::store_id`]). LanceDB commits are not fsynced by basemind and the store can be
//! wiped or deleted on its own, so two events put every embedded path back in the set, to be
//! rewritten from the cached blobs: a scan that finds another store, or none, where that one was,
//! and the scan that verifies blobs after the machine went down.

use std::io::Write;
use std::path::Path;

use ahash::AHashSet;
use serde::{Deserialize, Serialize};

use crate::path::RelPath;
use crate::store::StoreError;

/// File name of the owed set inside a view directory, beside `index.msgpack`.
pub const PENDING_FILE: &str = "pending-vectors.msgpack";

/// File name, inside a view directory, of the id of the LanceDB store the view's rows are in.
pub const STORE_ID_FILE: &str = "vector-store-id";

/// The id of the LanceDB store the rows of the view at `view_dir` were last written to, or `None`
/// when no scan of the view has opened one.
#[cfg(any(test, feature = "documents", feature = "code-search"))]
pub(crate) fn recorded_store(view_dir: &Path) -> Option<String> {
    let id = std::fs::read_to_string(view_dir.join(STORE_ID_FILE)).ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Record `id` as the store the view's rows are in. Best effort: unrecorded, a later loss of the
/// store goes unnoticed for this view, which is where every view was before the record existed.
#[cfg(any(test, feature = "documents", feature = "code-search"))]
pub(crate) fn record_store(view_dir: &Path, id: &str) {
    if recorded_store(view_dir).as_deref() == Some(id) {
        return;
    }
    let path = view_dir.join(STORE_ID_FILE);
    let tmp = view_dir.join(format!("{STORE_ID_FILE}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, id).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Paths owed vector rows, per LanceDB table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PendingVectors {
    /// Documents owed `documents` rows.
    #[serde(default)]
    docs: AHashSet<RelPath>,
    /// Code files owed `code_chunks` rows.
    #[serde(default)]
    code: AHashSet<RelPath>,
    /// Whether the set differs from what [`PENDING_FILE`] holds.
    #[serde(skip)]
    dirty: bool,
}

impl PendingVectors {
    /// Read the owed set of a view. A missing file is the empty set. So is one that cannot be
    /// read: it is written whole or not at all, and failing every open over it would cost more
    /// than the rows it might have named.
    pub(crate) fn load(view_dir: &Path) -> Self {
        let path = view_dir.join(PENDING_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "owed vector rows could not be read");
                return Self::default();
            }
        };
        rmp_serde::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(path = %path.display(), %error, "owed vector rows could not be decoded");
            Self::default()
        })
    }

    /// Make the file say what the set says: nothing to do when they agree, the file removed when
    /// the set is empty, else a whole new file renamed into place after an fsync.
    pub(crate) fn persist(&mut self, view_dir: &Path) -> Result<(), StoreError> {
        if !self.dirty {
            return Ok(());
        }
        let path = view_dir.join(PENDING_FILE);
        let io = |path: &Path| {
            let path = path.to_path_buf();
            move |source| StoreError::Io { path, source }
        };
        if self.is_empty() {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io(&path)(error)),
            }
        } else {
            // Named by pid: a store scanned in place holds no workspace lock, so two processes can
            // be here at once.
            let tmp = view_dir.join(format!("{PENDING_FILE}.{}.tmp", std::process::id()));
            let bytes = rmp_serde::to_vec_named(self)?;
            let mut file = std::fs::File::create(&tmp).map_err(io(&tmp))?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(io(&tmp))?;
            std::fs::rename(&tmp, &path).map_err(io(&path))?;
        }
        self.dirty = false;
        Ok(())
    }

    /// Take in what `other` owes, keeping what this set owes. For a store that was opened without
    /// the workspace lock and is now scanned in place: the set it loaded at open may be behind
    /// what another process has recorded since, and persisting it as it is would drop those debts.
    /// A debt the other process settled since may come back with this; the next flush settles it
    /// again.
    pub(crate) fn absorb(&mut self, other: Self) {
        for rel in other.docs {
            self.dirty |= self.docs.insert(rel);
        }
        for rel in other.code {
            self.dirty |= self.code.insert(rel);
        }
    }

    /// The documents owed rows.
    pub fn docs(&self) -> &AHashSet<RelPath> {
        &self.docs
    }

    /// The code files owed rows.
    pub fn code(&self) -> &AHashSet<RelPath> {
        &self.code
    }

    /// How many paths are owed rows across both tables.
    pub fn len(&self) -> usize {
        self.docs.len() + self.code.len()
    }

    /// True when no path is owed rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Record that the document at `rel` is owed rows.
    #[cfg_attr(not(feature = "documents"), allow(dead_code))]
    pub(crate) fn owe_doc(&mut self, rel: &str) {
        self.dirty |= self.docs.insert(rel.into());
    }

    /// Record that the code file at `rel` is owed rows.
    #[cfg_attr(not(feature = "code-search"), allow(dead_code))]
    pub(crate) fn owe_code(&mut self, rel: &str) {
        self.dirty |= self.code.insert(rel.into());
    }

    /// Drop `rels` from the documents set: their rows are written, or they have none to write.
    #[cfg_attr(not(feature = "documents"), allow(dead_code))]
    pub(crate) fn settle_docs<S: AsRef<str>>(&mut self, rels: &[S]) {
        for rel in rels {
            self.dirty |= self.docs.remove(bstr::BStr::new(rel.as_ref().as_bytes()));
        }
    }

    /// Drop `rels` from the code set: their rows are written, or they have none to write.
    #[cfg_attr(not(feature = "code-search"), allow(dead_code))]
    pub(crate) fn settle_code<S: AsRef<str>>(&mut self, rels: &[S]) {
        for rel in rels {
            self.dirty |= self.code.remove(bstr::BStr::new(rel.as_ref().as_bytes()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owed_set_round_trips_and_an_empty_set_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(PENDING_FILE);
        let mut pending = PendingVectors::load(dir.path());
        assert!(pending.is_empty());
        pending.persist(dir.path()).unwrap();
        assert!(!file.exists(), "an unchanged empty set writes nothing");

        pending.owe_doc("docs/a.pdf");
        pending.owe_code("src/a.rs");
        pending.persist(dir.path()).unwrap();
        let reread = PendingVectors::load(dir.path());
        assert_eq!((reread.docs().len(), reread.code().len()), (1, 1));
        assert!(reread.docs().contains(bstr::BStr::new("docs/a.pdf")));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temp file left");

        pending.settle_docs(&["docs/a.pdf"]);
        pending.settle_code(&["src/a.rs"]);
        pending.persist(dir.path()).unwrap();
        assert!(!file.exists(), "nothing owed, no file");
    }

    #[test]
    fn an_undecodable_file_reads_as_nothing_owed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PENDING_FILE), b"\xc1 not msgpack").unwrap();
        assert!(PendingVectors::load(dir.path()).is_empty());
    }

    /// A set loaded before another process recorded a debt must not, once persisted, drop it.
    #[test]
    fn absorbing_what_is_on_disk_keeps_the_debts_of_both() {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded_early = PendingVectors::load(dir.path());
        let mut other_process = PendingVectors::default();
        other_process.owe_code("src/theirs.rs");
        other_process.persist(dir.path()).unwrap();
        loaded_early.owe_doc("docs/ours.pdf");

        loaded_early.absorb(PendingVectors::load(dir.path()));
        loaded_early.persist(dir.path()).unwrap();

        let reread = PendingVectors::load(dir.path());
        assert!(reread.code().contains(bstr::BStr::new("src/theirs.rs")));
        assert!(reread.docs().contains(bstr::BStr::new("docs/ours.pdf")));
    }

    #[test]
    fn the_recorded_store_round_trips_and_is_absent_until_recorded() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(recorded_store(dir.path()), None);
        record_store(dir.path(), "abc-1");
        assert_eq!(recorded_store(dir.path()).as_deref(), Some("abc-1"));
        record_store(dir.path(), "def-2");
        assert_eq!(recorded_store(dir.path()).as_deref(), Some("def-2"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temp file left");
    }
}
