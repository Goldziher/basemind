//! Query-time reconciliation of the [`Registry`] with the repositories on disk.
//!
//! Registration enumerates a workspace once; branches and worktrees then come and go without the
//! daemon hearing of it. A query reconciles the rows it reads first: rows whose path is gone are
//! dropped, and a clone is re-enumerated when its [`RepoStamp`] moved, which costs a few `stat`
//! calls when nothing changed. The snapshot is rewritten only when a row changed.

use std::path::{Path, PathBuf};

use super::stamp::RepoStamp;
use super::{
    BranchRecord, Registry, RegistryError, RepoId, RepoRecord, WorkspaceKind, WorkspaceRecord, WorktreeRecord,
    now_micros, scope_key,
};
use crate::git;

/// Every row of one repo with its `last_seen` cleared, to tell a re-enumeration that changed a row
/// from one that only refreshed the times.
type RepoRows = (
    Vec<WorkspaceRecord>,
    Option<RepoRecord>,
    Vec<WorktreeRecord>,
    Vec<BranchRecord>,
);

impl Registry {
    /// Bring the rows of `repo_id`, or with `None` every row, up to date with the repositories on
    /// disk: [`Self::refresh_stale_workspaces`], then [`Self::refresh_stale_repo`] for each repo.
    ///
    /// # Errors
    /// The first [`RegistryError`] met; every other repo is still reconciled and persisted.
    pub fn refresh_stale(&mut self, repo_id: Option<&str>) -> Result<(), RegistryError> {
        let (ids, mut outcome) = match repo_id {
            Some(id) => (vec![id.to_string()], Ok(())),
            None => {
                let outcome = self.refresh_stale_workspaces();
                (self.repo_ids(), outcome)
            }
        };
        for id in ids {
            let refreshed = self.refresh_stale_repo(&id);
            if outcome.is_ok() {
                outcome = refreshed;
            }
        }
        outcome
    }

    /// Every repo id a workspace or repo row names, sorted.
    pub fn repo_ids(&self) -> Vec<RepoId> {
        let mut ids: Vec<RepoId> = self
            .workspaces
            .values()
            .filter_map(|row| row.repo_id.clone())
            .chain(self.repos.keys().cloned())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Drop the rows whose path is gone and re-register every plain workspace a repository now
    /// contains, persisting the snapshot when a row changed.
    ///
    /// # Errors
    /// The first [`RegistryError`] met; the other workspaces are still reconciled and persisted.
    pub fn refresh_stale_workspaces(&mut self) -> Result<(), RegistryError> {
        let mut changed = self.retain_present() > 0;
        let roots: Vec<PathBuf> = self
            .workspaces
            .values()
            .filter(|row| row.kind == WorkspaceKind::Plain && inside_git(&row.root))
            .map(|row| row.root.clone())
            .collect();
        let mut outcome = Ok(());
        for root in roots {
            match self.upsert_workspace(&root, &mut ahash::AHashSet::new()) {
                Ok(key) => {
                    changed |= self
                        .workspaces
                        .get(&key)
                        .is_some_and(|row| row.kind == WorkspaceKind::Git);
                }
                Err(error) if outcome.is_ok() => outcome = Err(error),
                Err(error) => tracing::warn!(%error, root = %root.display(), "registry: workspace refresh failed"),
            }
        }
        let persisted = if changed { self.persist() } else { Ok(()) };
        outcome.and(persisted)
    }

    /// Drop the rows whose path is gone and re-enumerate `repo_id` when one of its clones' stamps
    /// moved, persisting the snapshot when a row changed.
    ///
    /// # Errors
    /// Returns [`RegistryError`] when the repo cannot be enumerated or the snapshot cannot be
    /// written; the rows changed before an enumeration failure are still persisted.
    pub fn refresh_stale_repo(&mut self, repo_id: &str) -> Result<(), RegistryError> {
        let mut changed = self.retain_present() > 0;
        let mut outcome = Ok(());
        if self.repo_is_stale(repo_id) {
            let before = self.rows_of(repo_id);
            outcome = self.reenumerate(repo_id);
            changed |= self.rows_of(repo_id) != before;
        }
        let persisted = if changed { self.persist() } else { Ok(()) };
        outcome.and(persisted)
    }

    /// Whether `repo_id` has no stamp, or one of its clones' stamps is unsettled or moved.
    fn repo_is_stale(&self, repo_id: &str) -> bool {
        self.stamps.get(repo_id).is_none_or(|clones| {
            clones.is_empty()
                || clones
                    .iter()
                    .any(|(common_dir, stamp)| stamp.as_ref() != Some(&RepoStamp::of(common_dir)))
        })
    }

    /// Re-enumerate `repo_id` from its workspaces' roots, each clone once, or from its main root when
    /// it has none. A repo none of its roots resolve to any more (its remote changed, or it is no
    /// repository) is dropped.
    fn reenumerate(&mut self, repo_id: &str) -> Result<(), RegistryError> {
        let roots: Vec<PathBuf> = self
            .workspaces
            .values()
            .filter(|row| row.repo_id.as_deref() == Some(repo_id))
            .map(|row| row.root.clone())
            .collect();
        self.stamps.remove(repo_id);
        if roots.is_empty() {
            let Some(main_root) = self.repos.get(repo_id).map(|repo| repo.main_root.clone()) else {
                return Ok(());
            };
            match git::Repo::discover(&main_root) {
                Ok(repository) => {
                    let current = scope_key(&repository);
                    self.populate_git(&repository, &current, now_micros())?;
                    if current != repo_id {
                        self.drop_repo(repo_id);
                    }
                }
                Err(_) => self.drop_repo(repo_id),
            }
            return Ok(());
        }
        let mut enumerated = ahash::AHashSet::new();
        let mut outcome = Ok(());
        for root in roots {
            match self.upsert_workspace(&root, &mut enumerated) {
                Ok(_) => {}
                Err(error) if outcome.is_ok() => outcome = Err(error),
                Err(error) => tracing::warn!(%error, root = %root.display(), "registry: workspace refresh failed"),
            }
        }
        if !self
            .workspaces
            .values()
            .any(|row| row.repo_id.as_deref() == Some(repo_id))
        {
            self.drop_repo(repo_id);
        }
        outcome
    }

    /// The rows of `repo_id` with their `last_seen` cleared.
    fn rows_of(&self, repo_id: &str) -> RepoRows {
        let id = repo_id.to_string();
        let mut workspaces: Vec<WorkspaceRecord> = self
            .workspaces()
            .into_iter()
            .filter(|row| row.repo_id.as_deref() == Some(repo_id))
            .collect();
        workspaces.iter_mut().for_each(|row| row.last_seen = 0);
        let mut repo = self.get_repo(repo_id);
        if let Some(repo) = repo.as_mut() {
            repo.last_seen = 0;
        }
        let mut worktrees = self.worktrees(&id);
        worktrees.iter_mut().for_each(|row| row.last_seen = 0);
        let mut branches = self.branches(&id);
        branches.iter_mut().for_each(|row| row.last_seen = 0);
        (workspaces, repo, worktrees, branches)
    }

    /// Drop every row of `repo_id`.
    fn drop_repo(&mut self, repo_id: &str) {
        self.repos.remove(repo_id);
        self.worktrees.retain(|_, row| row.repo_id != repo_id);
        self.branches.retain(|_, row| row.repo_id != repo_id);
        self.stamps.remove(repo_id);
    }
}

/// Whether `root` or a directory above it holds a `.git`.
fn inside_git(root: &Path) -> bool {
    root.ancestors().any(|dir| dir.join(".git").exists())
}
