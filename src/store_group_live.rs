//! Which contents a repository group still holds, for the sweeps of content-keyed index rows.
//!
//! The grep bloom and the BM25 postings are keyed by content, not by path. In a group index
//! (ADR-0028) one such row serves every worktree of the group holding that content, so a sweep run
//! by one worktree may drop a row only when no view of the group holds the content: its own
//! in-memory file map, and the file map on disk of every other view. A file map that cannot be read
//! and is not provably of another schema makes the liveness unknowable, and the sweep is skipped.

use std::path::PathBuf;

use crate::store::{Store, StoreError};
use crate::store_hash_refs::HashRefs;

/// Why the group's liveness could not be established. A sweep given this must delete nothing.
#[derive(Debug, thiserror::Error)]
#[error("file map at {path} is unreadable, so the contents it holds are unknown: {source}")]
pub(crate) struct LivenessUnknown {
    path: PathBuf,
    #[source]
    source: StoreError,
}

/// The contents held by the views of a group other than the sweeping one, read from their file maps.
#[derive(Debug, Default)]
pub(crate) struct SiblingContents(HashRefs);

impl SiblingContents {
    /// The contents named by the file maps under `view_dirs`. A view with no file map holds nothing;
    /// one of an older or newer schema, or of an earlier blob layout, is rebuilt by its next scan
    /// before it is read again, so its rows are not this sweep's to keep.
    pub(crate) fn read(view_dirs: impl IntoIterator<Item = PathBuf>) -> Result<Self, LivenessUnknown> {
        let mut refs = HashRefs::default();
        for view_dir in view_dirs {
            match crate::store::read_index(&view_dir) {
                Ok(Some(index)) => {
                    for hash in index
                        .files
                        .values()
                        .map(|entry| entry.hash_hex.as_str())
                        .chain(index.rebuild_pins.iter().map(String::as_str))
                    {
                        refs.acquire(hash);
                    }
                }
                Ok(None)
                | Err(
                    StoreError::SchemaMismatch { .. }
                    | StoreError::NewerSchema { .. }
                    | StoreError::StaleBlobLayout { .. },
                ) => {}
                Err(source) => return Err(LivenessUnknown { path: view_dir, source }),
            }
        }
        Ok(Self(refs))
    }

    fn holds(&self, hash_hex: &str) -> bool {
        self.0.is_held(hash_hex)
    }
}

/// Liveness of a content for one sweep: held by the sweeping store's own file map or by a sibling.
pub(crate) struct ContentLiveness<'a> {
    store: &'a Store,
    siblings: SiblingContents,
}

impl ContentLiveness<'_> {
    /// Whether any view of the group holds `hash_hex`.
    pub(crate) fn holds(&self, hash_hex: &str) -> bool {
        self.store.hash_is_held(hash_hex) || self.siblings.holds(hash_hex)
    }
}

impl Store {
    /// The liveness a sweep of this store's content-keyed rows must respect. Read at the time of the
    /// sweep: a sibling's file map changes with every scan of it.
    pub(crate) fn content_liveness(&self) -> Result<ContentLiveness<'_>, LivenessUnknown> {
        Ok(ContentLiveness {
            store: self,
            siblings: SiblingContents::read(self.sibling_view_dirs()?)?,
        })
    }

    /// The view directories of every other view sharing this store's index database: none for a
    /// database of its own, since only a group index shares rows across views.
    pub(crate) fn sibling_view_dirs(&self) -> Result<Vec<PathBuf>, LivenessUnknown> {
        let Some(db) = self.index_db.as_ref() else {
            return Ok(Vec::new());
        };
        let views = db.sibling_views().map_err(|error| LivenessUnknown {
            path: self.view_dir.clone(),
            source: error.into(),
        })?;
        let workspaces = crate::store::cache_root()
            .join(crate::store::CACHE_DIR)
            .join(crate::store::WORKSPACES_DIR);
        Ok(views
            .into_iter()
            .map(|(workspace, view)| workspaces.join(workspace).join(crate::store::VIEWS_DIR).join(view))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::store::{FileEntry, INDEX_FILE, Index};

    fn write_map(view_dir: &Path, hashes: &[&str]) {
        std::fs::create_dir_all(view_dir).expect("view dir");
        let mut index = Index::empty();
        for (i, hash) in hashes.iter().enumerate() {
            index.files.insert(
                crate::path::RelPath::from(format!("f{i}.rs").as_str()),
                FileEntry {
                    hash_hex: (*hash).to_string(),
                    language: "rust".into(),
                    size_bytes: 1,
                    mtime: 0,
                    extract_epoch: 0,
                },
            );
        }
        std::fs::write(
            view_dir.join(INDEX_FILE),
            rmp_serde::to_vec_named(&index).expect("encode"),
        )
        .expect("write");
    }

    #[test]
    fn contents_of_every_sibling_file_map_are_held() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (b, c, missing) = (temp.path().join("b"), temp.path().join("c"), temp.path().join("none"));
        write_map(&b, &["only-in-b"]);
        write_map(&c, &["only-in-c", "shared"]);

        let siblings = SiblingContents::read([b, c, missing]).expect("readable");

        assert!(siblings.holds("only-in-b") && siblings.holds("only-in-c") && siblings.holds("shared"));
        assert!(!siblings.holds("in-no-map"));
    }

    #[test]
    fn an_unreadable_sibling_file_map_makes_liveness_unknown() {
        let temp = tempfile::tempdir().expect("tempdir");
        let damaged = temp.path().join("damaged");
        std::fs::create_dir_all(&damaged).expect("view dir");
        std::fs::write(damaged.join(INDEX_FILE), b"\xc1 not msgpack").expect("write");

        assert!(SiblingContents::read([damaged]).is_err(), "a sweep must not guess");
    }

    #[test]
    fn a_sibling_of_another_schema_holds_nothing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let older = temp.path().join("older");
        write_map(&older, &["h"]);
        let mut index = crate::store::read_index(&older).expect("read").expect("present");
        index.schema_ver -= 1;
        std::fs::write(older.join(INDEX_FILE), rmp_serde::to_vec_named(&index).expect("encode")).expect("write");

        let siblings = SiblingContents::read([older]).expect("another schema is not unknown");

        assert!(!siblings.holds("h"));
    }
}
