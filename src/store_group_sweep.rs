//! The group sweep from the store's side (ADR-0028 rule 10): which views of the group are gone and
//! which contents its views still name, read from the file system; the index deletes the rest
//! ([`crate::index::group_sweep`]).
//!
//! It runs at the end of a full pass, never on a read. A view is gone when its view directory is:
//! the workspace reaper removes the workspace of a deleted worktree, and the cache budget a cold
//! one's views. A content is live while any view's file map names it under its recorded key, and a
//! dead content stays while a file map names the same file content under another extract epoch: a
//! build of the same release minor that extracts under that epoch shares the group and the blobs
//! ([`Store::group_other_epoch_keys`]).

use ahash::AHashSet;

use crate::index::IndexError;
use crate::index::group_sweep::GroupSweepReport;
use crate::index::keys::ContentKey;
use crate::store::{Store, VIEWS_DIR};
use crate::store_group_live::LivenessUnknown;

impl Store {
    /// Retire the group's views that are gone, then, when the group owes it, delete the symbol and
    /// call rows of every content no view names. `None` when this store holds no group index or
    /// another sweep of the group is running. A file map that cannot be read leaves every content
    /// in place, and the debt owed; so does a failed lookup of the rows another build retains.
    pub(crate) fn sweep_group(&self) -> Result<Option<GroupSweepReport>, IndexError> {
        self.sweep_group_retaining(|dead| self.group_other_epoch_keys(dead))
    }

    /// [`Self::sweep_group`] with the lookup of the dead contents other builds retain given.
    fn sweep_group_retaining(
        &self,
        other_epoch_keys: impl FnOnce(&[ContentKey]) -> Result<AHashSet<ContentKey>, LivenessUnknown>,
    ) -> Result<Option<GroupSweepReport>, IndexError> {
        let Some(db) = self.index_db.as_ref() else {
            return Ok(None);
        };
        let Some(mut sweep) = db.begin_group_sweep()? else {
            return Ok(None);
        };
        let workspaces = self.data_root.workspaces_dir();
        for (workspace, view, view_id) in sweep.other_views()? {
            let view_dir = workspaces.join(&workspace).join(VIEWS_DIR).join(&view);
            // A directory whose existence cannot be told is kept. ~keep
            if matches!(view_dir.try_exists(), Ok(false)) {
                sweep.retire_view(&workspace, &view, view_id)?;
            }
        }
        if !sweep.owes_contents() {
            return sweep.finish(false).map(Some);
        }
        match self.group_live_content_keys() {
            Ok(live) => {
                let retention_known = std::cell::Cell::new(true);
                sweep.sweep_contents_retaining(
                    |content| live.contains(content),
                    |dead| {
                        other_epoch_keys(dead).unwrap_or_else(|error| {
                            retention_known.set(false);
                            tracing::warn!(%error, "group sweep could not tell other builds' rows apart; it keeps every dead content and the debt");
                            dead.iter().copied().collect()
                        })
                    },
                )?;
                sweep.finish(retention_known.get()).map(Some)
            }
            Err(error) => {
                tracing::warn!(%error, "group sweep skipped the content rows; they stay until the next full pass");
                sweep.finish(false).map(Some)
            }
        }
    }

    /// [`Self::sweep_group`] for the end of a full pass: a failure is logged, and what was not
    /// deleted stays owed to the next full pass.
    pub(crate) fn sweep_group_logged(&self) {
        match self.sweep_group() {
            Ok(Some(report)) if report != GroupSweepReport::default() => {
                tracing::debug!(?report, "group sweep");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "group sweep failed; the next full pass retries it"),
        }
    }
}

#[cfg(test)]
#[path = "store_group_sweep_tests.rs"]
mod tests;
