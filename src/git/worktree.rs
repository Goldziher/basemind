//! Read-only branch + worktree enumeration for the machine-wide repo registry.
//!
//! Lives in its own file to keep `src/git/mod.rs` under the 1000-line per-file cap
//! (`module-size-cap`). The public API (`BranchInfo`, `WorktreeInfo`, and the two `Repo`
//! methods) is re-exported through `super::*`.
//!
//! Everything here is git *plumbing* only — refs and the `<common_dir>/worktrees/` layout.
//! No working-tree walk happens, so both methods stay cheap enough to run across every repo
//! on a machine.

use std::path::{Path, PathBuf};

use super::{GitError, Repo, anchor_git_path};

/// A local branch (`refs/heads/<name>`) and the commit it points at.
#[derive(Debug, Clone)]
pub struct BranchInfo {
    /// Short branch name (the `refs/heads/` prefix stripped), e.g. `main`.
    pub name: String,
    /// 40-hex sha of the commit the branch resolves to.
    pub head_sha: String,
}

/// One worktree of a clone: the main worktree plus every linked worktree registered under
/// `<common_dir>/worktrees/`.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    /// `"(main)"` for the main worktree, otherwise the linked-worktree directory name under
    /// `<common_dir>/worktrees/`.
    pub name: String,
    /// Absolute, canonicalized path to the worktree root (the checkout directory).
    pub path: PathBuf,
    /// Checked-out branch (`HEAD` → `refs/heads/<branch>`), or `None` when HEAD is detached
    /// or unresolvable.
    pub branch: Option<String>,
    /// Head commit sha (40-hex), or `None` when it could not be resolved (e.g. an unborn
    /// HEAD on a freshly added worktree).
    pub head_sha: Option<String>,
    /// True when this worktree has a detached HEAD (points directly at a commit, not a
    /// branch ref).
    pub detached: bool,
}

impl Repo {
    /// Enumerate every local branch (`refs/heads/*`) with its target commit sha.
    ///
    /// Backs a future machine-wide repo/branch registry, so it is deliberately cheap: it
    /// iterates refs via gix (`local.references()?.local_branches()`) and peels each to a
    /// commit id — no working-tree access. Refs that fail to peel to a commit (a dangling or
    /// unresolvable ref) are skipped rather than failing the whole call. Results are sorted
    /// by branch name for deterministic output.
    ///
    /// # Errors
    /// Returns [`GitError::Read`] if the reference database cannot be opened or the branch
    /// iterator cannot be constructed. Individual unresolvable refs are skipped, not surfaced.
    pub fn list_local_branches(&self) -> Result<Vec<BranchInfo>, GitError> {
        let local = self.local();
        let platform = local.references().map_err(|e| GitError::Read {
            what: "references".to_string(),
            msg: e.to_string(),
        })?;
        let branches = platform.local_branches().map_err(|e| GitError::Read {
            what: "local branches".to_string(),
            msg: e.to_string(),
        })?;
        let mut out = Vec::new();
        for reference in branches {
            let Ok(mut reference) = reference else { continue };
            let name = String::from_utf8_lossy(reference.name().shorten()).into_owned();
            let Ok(id) = reference.peel_to_id() else {
                continue;
            };
            out.push(BranchInfo {
                name,
                head_sha: id.to_string(),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Enumerate this clone's worktrees: the main worktree first, then every linked worktree
    /// (`git worktree add`) sorted by name.
    ///
    /// Backs a future machine-wide repo/worktree registry. The main worktree is derived from
    /// [`Repo::main_worktree_root`] + [`Repo::info`] (branch + head sha via HEAD). Linked
    /// worktrees are read straight from the git plumbing directory `<common_dir>/worktrees/*`
    /// — the intended cheap path: each entry has a `gitdir` file pointing at the worktree's
    /// `.git` (whose parent is the checkout root) and a `HEAD` file giving the checked-out
    /// branch or a detached sha. Stored paths may be relative, so they are anchored with
    /// [`anchor_git_path`] and canonicalized. Malformed entries (missing/garbled `gitdir` or
    /// `HEAD`) are skipped rather than failing the whole call.
    ///
    /// # Errors
    /// Returns [`GitError::Read`] only if the main worktree's identity snapshot ([`Repo::info`])
    /// fails. A missing or unreadable `worktrees/` directory yields just the main entry.
    pub fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>, GitError> {
        let mut out = vec![self.main_worktree_info()?];

        let common = anchor_git_path(&self.workdir, self.local().common_dir());
        let worktrees_dir = common.join("worktrees");
        if let Ok(entries) = std::fs::read_dir(&worktrees_dir) {
            let mut linked: Vec<WorktreeInfo> = entries
                .filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .filter_map(|e| self.linked_worktree_info(&e.path()))
                .collect();
            linked.sort_by(|a, b| a.name.cmp(&b.name));
            out.extend(linked);
        }
        Ok(out)
    }

    /// Build the [`WorktreeInfo`] for the main worktree from its identity snapshot.
    fn main_worktree_info(&self) -> Result<WorktreeInfo, GitError> {
        let info = self.info()?;
        Ok(WorktreeInfo {
            name: "(main)".to_string(),
            path: self.main_worktree_root(),
            detached: info.head_sha.is_some() && info.branch.is_none(),
            branch: info.branch,
            head_sha: info.head_sha,
        })
    }

    /// Parse one linked-worktree plumbing directory (`<common_dir>/worktrees/<name>`) into a
    /// [`WorktreeInfo`]. Returns `None` when the directory is malformed (no resolvable root).
    fn linked_worktree_info(&self, plumbing_dir: &Path) -> Option<WorktreeInfo> {
        let name = plumbing_dir.file_name()?.to_string_lossy().into_owned();
        let root = self.worktree_root_from_gitdir(plumbing_dir)?;
        let (branch, head_sha, detached) = self.read_worktree_head(plumbing_dir);
        Some(WorktreeInfo {
            name,
            path: root,
            branch,
            head_sha,
            detached,
        })
    }

    /// Resolve a linked worktree's checkout root from its `gitdir` file, which stores the path
    /// to the worktree's `.git` file; the parent of that path is the checkout root. The stored
    /// path may be relative, so it is anchored + canonicalized via [`anchor_git_path`].
    fn worktree_root_from_gitdir(&self, plumbing_dir: &Path) -> Option<PathBuf> {
        let raw = std::fs::read_to_string(plumbing_dir.join("gitdir")).ok()?;
        let dot_git = Path::new(raw.trim());
        let anchored = anchor_git_path(&self.workdir, dot_git);
        anchored.parent().map(Path::to_path_buf)
    }

    /// Read a worktree's `HEAD` file and classify it as an attached branch or a detached sha.
    /// Returns `(branch, head_sha, detached)`; a `ref: refs/heads/<b>` head resolves the branch
    /// to a sha through gix, a bare sha is reported detached, and an unreadable HEAD yields all
    /// empty/false.
    fn read_worktree_head(&self, plumbing_dir: &Path) -> (Option<String>, Option<String>, bool) {
        let Ok(head) = std::fs::read_to_string(plumbing_dir.join("HEAD")) else {
            return (None, None, false);
        };
        let head = head.trim();
        if let Some(refname) = head.strip_prefix("ref: ") {
            let branch = refname.strip_prefix("refs/heads/").unwrap_or(refname).to_string();
            let sha = self.resolve_rev(refname).ok();
            (Some(branch), sha, false)
        } else if !head.is_empty() {
            (None, Some(head.to_string()), true)
        } else {
            (None, None, false)
        }
    }
}
