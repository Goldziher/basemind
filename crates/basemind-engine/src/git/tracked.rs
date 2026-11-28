//! The files the staging index tracks, for `find_files`. In its own file to keep `src/git/mod.rs`
//! under the 1000-line per-file cap.

use super::{GitError, Repo};

/// The files the staging index tracks, with the index checksum they were read at.
#[derive(Debug, Clone, Default)]
pub struct TrackedFiles {
    /// The index file's trailing checksum, which changes whenever the index is written (`git add`,
    /// `git rm`, a checkout). `None` for an index written without one (`index.skipHash`) or no index.
    pub checksum: Option<gix::ObjectId>,
    /// Paths relative to [`Repo::workdir`], in the index's path order.
    pub files: Vec<crate::path::RelPath>,
}

impl Repo {
    /// Every file the staging index tracks and that is in the working tree, as `git ls-files` lists
    /// them: no submodule, no sparse directory entry, and no entry a sparse checkout left out
    /// (`SKIP_WORKTREE`). `None` when the index still has the checksum `known`: the caller holds
    /// that listing, and the entries are not walked again.
    pub fn tracked_files_since(&self, known: Option<&gix::ObjectId>) -> Result<Option<TrackedFiles>, GitError> {
        let local = self.local();
        let index = local.index_or_empty().map_err(|e| GitError::Read {
            what: "git index".to_string(),
            msg: e.to_string(),
        })?;
        let checksum = index.checksum();
        if checksum.is_some() && checksum.as_ref() == known {
            return Ok(None);
        }
        let mut files: Vec<crate::path::RelPath> = index
            .entries()
            .iter()
            .filter(|entry| {
                !entry.mode.is_submodule()
                    && !entry.mode.is_sparse()
                    && !entry.flags.contains(gix::index::entry::Flags::SKIP_WORKTREE)
            })
            .map(|entry| crate::path::RelPath::from(<gix::bstr::BStr as AsRef<[u8]>>::as_ref(entry.path(&index))))
            .collect();
        // A conflicted path has an entry per stage, next to each other in the index's path order.
        files.dedup();
        Ok(Some(TrackedFiles { checksum, files }))
    }
}
