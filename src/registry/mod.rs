//! Machine-wide repo / worktree / branch / workspace registry.
//!
//! The daemon keeps a cheap, always-on map of every workspace it has seen: which git repos exist,
//! their linked worktrees and local branches, and the non-git ("plain") directories registered for
//! document indexing. Rows are populated from git *plumbing only* (refs + the `worktrees/` layout,
//! via [`crate::git`]) — no working-tree walk — so registering a workspace costs microseconds and
//! scales to every repo on a machine.
//!
//! The registry is a single msgpack snapshot under `$BASEMIND_DATA_HOME/registry/registry.msgpack`,
//! rewritten atomically (temp + rename) after every mutation. The daemon is the sole writer, so no
//! in-file locking is needed; readers load a fresh [`Registry`] or share one behind the daemon's own
//! synchronization. This module is the persistence + population layer only — daemon wiring, watchers,
//! and MCP tools live in later slices.

use std::path::{Path, PathBuf};

use ahash::AHashMap;
use serde::{Deserialize, Serialize};

use crate::git::{self, scope_key};

/// Stable per-workspace key: the hex blake3 of the canonical worktree root (see
/// [`crate::store::workspace_key`]). Reused verbatim so a workspace's registry row and its cache
/// directory share one identity.
pub type WorkspaceKey = String;

/// Stable per-repository id: the normalized remote URL, else `path:<root>` (see
/// [`crate::git::scope_key`]). Shared by every worktree + branch of one clone.
pub type RepoId = String;

/// Snapshot schema version. Bumped only on an incompatible on-disk shape change; a mismatch is
/// treated as an empty registry (the daemon re-populates from git on the next `register`), mirroring
/// the wipe-on-mismatch story of the index + blob stores.
const REGISTRY_SCHEMA_VER: u16 = 1;

/// Snapshot file name under the registry directory.
const SNAPSHOT_FILE: &str = "registry.msgpack";

/// Whether a registered workspace is backed by a git repository or is a plain directory (media /
/// docs) that still gets `list_files` / document indexing but no repo/worktree/branch rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    /// A git worktree (main or linked).
    Git,
    /// A non-git directory registered for indexing.
    Plain,
}

/// One registered workspace root. Keyed by [`WorkspaceKey`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    /// Stable workspace key (blake3 of the canonical root).
    pub key: WorkspaceKey,
    /// Git-backed or plain.
    pub kind: WorkspaceKind,
    /// Canonical workspace root.
    pub root: PathBuf,
    /// Owning repo id, `None` for a plain workspace.
    pub repo_id: Option<RepoId>,
    /// Main-worktree root of the owning clone, `None` for a plain workspace.
    pub main_worktree: Option<PathBuf>,
    /// Unix micros of the last time this workspace was registered/refreshed.
    pub last_seen: i64,
}

/// One git clone: its main worktree, remote, linked-worktree names, and coordination dependencies.
/// Keyed by [`RepoId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRecord {
    /// Repo id (= [`scope_key`]).
    pub repo_id: RepoId,
    /// Main-worktree root.
    pub main_root: PathBuf,
    /// `origin` remote URL, if any.
    pub remote: Option<String>,
    /// Names of every worktree (`"(main)"` + linked names).
    pub worktrees: Vec<String>,
    /// Repo ids this clone depends on (coordination-awareness only; populated by a later slice).
    pub deps: Vec<RepoId>,
    /// Unix micros of the last refresh.
    pub last_seen: i64,
}

/// One worktree of a clone. Keyed by `repo_id ‖ name`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    /// Owning repo id.
    pub repo_id: RepoId,
    /// `"(main)"` or the linked-worktree directory name.
    pub name: String,
    /// Absolute, canonical checkout root.
    pub path: PathBuf,
    /// Head commit sha, `None` on an unborn HEAD.
    pub head_sha: Option<String>,
    /// Checked-out branch, `None` when detached or unresolvable.
    pub branch: Option<String>,
    /// True when HEAD is detached.
    pub detached: bool,
    /// Advisory claimant (an agent/session id) currently holding this worktree, if any.
    pub claimed_by: Option<String>,
    /// Unix micros of the last refresh.
    pub last_seen: i64,
}

/// One local branch of a clone. Keyed by `repo_id ‖ name`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchRecord {
    /// Owning repo id.
    pub repo_id: RepoId,
    /// Short branch name (`refs/heads/` stripped).
    pub name: String,
    /// 40-hex head commit sha.
    pub head_sha: String,
    /// Unix micros of the last refresh.
    pub last_seen: i64,
}

/// The on-disk snapshot: every keyspace flattened to a sorted vector for a compact, stable encoding.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistrySnapshot {
    /// Schema version guard (see [`REGISTRY_SCHEMA_VER`]).
    schema_ver: u16,
    workspaces: Vec<WorkspaceRecord>,
    repos: Vec<RepoRecord>,
    worktrees: Vec<WorktreeRecord>,
    branches: Vec<BranchRecord>,
}

/// Failure opening, decoding, encoding, or populating the registry.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// A filesystem operation on the snapshot or its directory failed.
    #[error("registry io at {path}: {source}")]
    Io {
        /// The path being read/written.
        path: PathBuf,
        /// The underlying io error.
        source: std::io::Error,
    },
    /// The snapshot could not be msgpack-encoded.
    #[error("registry encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    /// Enumerating a repo's worktrees/branches failed after discovery succeeded.
    #[error("registry git enumeration: {0}")]
    Git(#[from] git::GitError),
}

/// The in-memory registry, backed by an atomic msgpack snapshot on disk.
///
/// Keyspaces are `AHashMap`s (hot-path convention) keyed by [`WorkspaceKey`] / [`RepoId`] /
/// `repo_id ‖ name`. Every mutating method persists the whole snapshot before returning, so an
/// abrupt daemon exit never leaves a torn registry.
pub struct Registry {
    /// Directory holding the snapshot (created on open).
    dir: PathBuf,
    workspaces: AHashMap<WorkspaceKey, WorkspaceRecord>,
    repos: AHashMap<RepoId, RepoRecord>,
    /// Keyed by [`composite_key`]`(repo_id, name)`.
    worktrees: AHashMap<String, WorktreeRecord>,
    /// Keyed by [`composite_key`]`(repo_id, name)`.
    branches: AHashMap<String, BranchRecord>,
}

/// Composite `repo_id ‖ name` key. The `\0` separator can appear in neither a scope-key nor a git
/// ref name, so `repo` + `id-a` never collides with `repo-id` + `a`.
fn composite_key(repo_id: &str, name: &str) -> String {
    format!("{repo_id}\u{0}{name}")
}

/// Current time as unix micros. Saturates to `0` before the epoch (never on a real clock).
fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

impl Registry {
    /// Open (or create) the registry rooted at `registry_dir`. A missing or version-mismatched
    /// snapshot yields an empty registry; a present, current snapshot is loaded.
    pub fn open(registry_dir: &Path) -> Result<Self, RegistryError> {
        std::fs::create_dir_all(registry_dir).map_err(|source| RegistryError::Io {
            path: registry_dir.to_path_buf(),
            source,
        })?;
        let snapshot = load_snapshot(&registry_dir.join(SNAPSHOT_FILE))?;
        let mut registry = Self {
            dir: registry_dir.to_path_buf(),
            workspaces: AHashMap::new(),
            repos: AHashMap::new(),
            worktrees: AHashMap::new(),
            branches: AHashMap::new(),
        };
        for record in snapshot.workspaces {
            registry.workspaces.insert(record.key.clone(), record);
        }
        for record in snapshot.repos {
            registry.repos.insert(record.repo_id.clone(), record);
        }
        for record in snapshot.worktrees {
            registry
                .worktrees
                .insert(composite_key(&record.repo_id, &record.name), record);
        }
        for record in snapshot.branches {
            registry
                .branches
                .insert(composite_key(&record.repo_id, &record.name), record);
        }
        Ok(registry)
    }

    /// Open the registry under the machine-global cache (`cache_root()/registry`).
    pub fn from_data_home() -> Result<Self, RegistryError> {
        Self::open(&crate::store::cache_root().join("registry"))
    }

    /// Register (or refresh) the workspace rooted at `root`, populating repo / worktree / branch rows
    /// from git plumbing. A non-git `root` registers as [`WorkspaceKind::Plain`] with no repo rows.
    /// Idempotent: re-registering refreshes `last_seen` and reconciles worktree/branch rows. Returns
    /// the workspace key.
    pub fn register_workspace(&mut self, root: &Path) -> Result<WorkspaceKey, RegistryError> {
        let key = crate::store::workspace_key(root);
        let now = now_micros();
        let record = match git::Repo::discover(root) {
            Ok(repository) => {
                let repo_id = scope_key(&repository);
                self.populate_git(&repository, &repo_id, now)?;
                WorkspaceRecord {
                    key: key.clone(),
                    kind: WorkspaceKind::Git,
                    root: root.to_path_buf(),
                    repo_id: Some(repo_id),
                    main_worktree: Some(repository.main_worktree_root()),
                    last_seen: now,
                }
            }
            Err(_) => WorkspaceRecord {
                key: key.clone(),
                kind: WorkspaceKind::Plain,
                root: root.to_path_buf(),
                repo_id: None,
                main_worktree: None,
                last_seen: now,
            },
        };
        self.workspaces.insert(key.clone(), record);
        self.persist()?;
        Ok(key)
    }

    /// Re-enumerate a repo from any of its roots and reconcile its worktree/branch rows (add new,
    /// update shas, drop rows no longer present). No-op-safe when `root` is not a git repo.
    pub fn refresh_from_root(&mut self, root: &Path) -> Result<(), RegistryError> {
        let repository = match git::Repo::discover(root) {
            Ok(repository) => repository,
            Err(_) => return Ok(()),
        };
        let repo_id = scope_key(&repository);
        self.populate_git(&repository, &repo_id, now_micros())?;
        self.persist()
    }

    /// Refresh a repo already known by id, using its recorded main root. No-op when the id is unknown
    /// or its main root has vanished.
    pub fn refresh_repo(&mut self, repo_id: &RepoId) -> Result<(), RegistryError> {
        let main_root = match self.repos.get(repo_id) {
            Some(repo) => repo.main_root.clone(),
            None => return Ok(()),
        };
        self.refresh_from_root(&main_root)
    }

    /// Enumerate `repository`'s worktrees + local branches and upsert the repo/worktree/branch rows,
    /// preserving any existing advisory `claimed_by`. Rows for this repo that are no longer present
    /// upstream are pruned so a removed worktree or deleted branch does not linger.
    fn populate_git(&mut self, repository: &git::Repo, repo_id: &RepoId, now: i64) -> Result<(), RegistryError> {
        let worktrees = repository.list_worktrees()?;
        let branches = repository.list_local_branches()?;

        let seen_worktrees: ahash::AHashSet<String> = worktrees.iter().map(|w| w.name.clone()).collect();
        let seen_branches: ahash::AHashSet<String> = branches.iter().map(|b| b.name.clone()).collect();
        self.worktrees
            .retain(|_, record| record.repo_id != *repo_id || seen_worktrees.contains(&record.name));
        self.branches
            .retain(|_, record| record.repo_id != *repo_id || seen_branches.contains(&record.name));

        for worktree in &worktrees {
            let composite = composite_key(repo_id, &worktree.name);
            let claimed_by = self.worktrees.get(&composite).and_then(|r| r.claimed_by.clone());
            self.worktrees.insert(
                composite,
                WorktreeRecord {
                    repo_id: repo_id.clone(),
                    name: worktree.name.clone(),
                    path: worktree.path.clone(),
                    head_sha: worktree.head_sha.clone(),
                    branch: worktree.branch.clone(),
                    detached: worktree.detached,
                    claimed_by,
                    last_seen: now,
                },
            );
        }
        for branch in &branches {
            self.branches.insert(
                composite_key(repo_id, &branch.name),
                BranchRecord {
                    repo_id: repo_id.clone(),
                    name: branch.name.clone(),
                    head_sha: branch.head_sha.clone(),
                    last_seen: now,
                },
            );
        }

        let existing_deps = self.repos.get(repo_id).map(|r| r.deps.clone()).unwrap_or_default();
        self.repos.insert(
            repo_id.clone(),
            RepoRecord {
                repo_id: repo_id.clone(),
                main_root: repository.main_worktree_root(),
                remote: repository.remote_url(),
                worktrees: worktrees.iter().map(|w| w.name.clone()).collect(),
                deps: existing_deps,
                last_seen: now,
            },
        );
        Ok(())
    }

    /// All registered workspaces, sorted by key for deterministic output.
    pub fn workspaces(&self) -> Vec<WorkspaceRecord> {
        sorted_by(self.workspaces.values().cloned().collect(), |r| r.key.clone())
    }

    /// All known repos, sorted by id.
    pub fn repos(&self) -> Vec<RepoRecord> {
        sorted_by(self.repos.values().cloned().collect(), |r| r.repo_id.clone())
    }

    /// Worktrees of `repo_id`, sorted by name.
    pub fn worktrees(&self, repo_id: &RepoId) -> Vec<WorktreeRecord> {
        let rows = self
            .worktrees
            .values()
            .filter(|r| r.repo_id == *repo_id)
            .cloned()
            .collect();
        sorted_by(rows, |r| r.name.clone())
    }

    /// Local branches of `repo_id`, sorted by name.
    pub fn branches(&self, repo_id: &RepoId) -> Vec<BranchRecord> {
        let rows = self
            .branches
            .values()
            .filter(|r| r.repo_id == *repo_id)
            .cloned()
            .collect();
        sorted_by(rows, |r| r.name.clone())
    }

    /// Look up one workspace by key.
    pub fn get_workspace(&self, key: &str) -> Option<WorkspaceRecord> {
        self.workspaces.get(key).cloned()
    }

    /// Look up one repo by id.
    pub fn get_repo(&self, repo_id: &str) -> Option<RepoRecord> {
        self.repos.get(repo_id).cloned()
    }

    /// Advisory-claim a worktree for `claimant`. Returns `true` when the claim is now held by
    /// `claimant` (freshly taken, or already theirs), `false` when another claimant holds it. Unknown
    /// `(repo_id, name)` returns `false`.
    pub fn claim_worktree(&mut self, repo_id: &RepoId, name: &str, claimant: &str) -> Result<bool, RegistryError> {
        let composite = composite_key(repo_id, name);
        let record = match self.worktrees.get_mut(&composite) {
            Some(record) => record,
            None => return Ok(false),
        };
        match &record.claimed_by {
            Some(holder) if holder != claimant => Ok(false),
            _ => {
                record.claimed_by = Some(claimant.to_string());
                self.persist()?;
                Ok(true)
            }
        }
    }

    /// Release a worktree claim held by `claimant`. Returns `true` when a claim by `claimant` was
    /// cleared, `false` when the worktree is unknown or held by someone else / no one.
    pub fn release_worktree(&mut self, repo_id: &RepoId, name: &str, claimant: &str) -> Result<bool, RegistryError> {
        let composite = composite_key(repo_id, name);
        let record = match self.worktrees.get_mut(&composite) {
            Some(record) => record,
            None => return Ok(false),
        };
        if record.claimed_by.as_deref() == Some(claimant) {
            record.claimed_by = None;
            self.persist()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Drop rows whose on-disk path no longer exists: worktrees + workspaces whose checkout/root is
    /// gone, repos whose main root is gone, and branches orphaned by a pruned repo. Returns the total
    /// number of rows removed. Persists only when something changed.
    pub fn prune_missing(&mut self) -> Result<usize, RegistryError> {
        let before = self.total_rows();

        self.worktrees.retain(|_, r| r.path.exists());
        self.workspaces.retain(|_, r| r.root.exists());
        self.repos.retain(|_, r| r.main_root.exists());
        let live_repos: ahash::AHashSet<RepoId> = self.repos.keys().cloned().collect();
        self.branches.retain(|_, r| live_repos.contains(&r.repo_id));

        let removed = before - self.total_rows();
        if removed > 0 {
            self.persist()?;
        }
        Ok(removed)
    }

    /// Total row count across every keyspace (diagnostics + prune accounting).
    fn total_rows(&self) -> usize {
        self.workspaces.len() + self.repos.len() + self.worktrees.len() + self.branches.len()
    }

    /// Atomically rewrite the snapshot: encode to msgpack, stream to a temp file in the registry
    /// dir, then POSIX-rename it over the target so a reader never observes a half-written file.
    fn persist(&self) -> Result<(), RegistryError> {
        let snapshot = RegistrySnapshot {
            schema_ver: REGISTRY_SCHEMA_VER,
            workspaces: self.workspaces(),
            repos: self.repos(),
            worktrees: sorted_by(self.worktrees.values().cloned().collect(), |r| {
                composite_key(&r.repo_id, &r.name)
            }),
            branches: sorted_by(self.branches.values().cloned().collect(), |r| {
                composite_key(&r.repo_id, &r.name)
            }),
        };
        let bytes = rmp_serde::to_vec_named(&snapshot)?;
        let target = self.dir.join(SNAPSHOT_FILE);
        let tmp = self.dir.join(format!("{SNAPSHOT_FILE}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, &bytes).map_err(|source| RegistryError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, &target).map_err(|source| RegistryError::Io { path: target, source })
    }
}

/// Sort a vector by a derived key. Small helper so every accessor returns deterministic order.
fn sorted_by<T, K: Ord>(mut rows: Vec<T>, key: impl Fn(&T) -> K) -> Vec<T> {
    rows.sort_by_key(|row| key(row));
    rows
}

/// Load and decode the snapshot at `path`. A missing file or a schema/version/decode mismatch yields
/// an empty snapshot (the daemon re-populates on the next register) rather than a hard failure.
fn load_snapshot(path: &Path) -> Result<RegistrySnapshot, RegistryError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(RegistrySnapshot::default()),
        Err(source) => {
            return Err(RegistryError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    match rmp_serde::from_slice::<RegistrySnapshot>(&bytes) {
        Ok(snapshot) if snapshot.schema_ver == REGISTRY_SCHEMA_VER => Ok(snapshot),
        _ => Ok(RegistrySnapshot::default()),
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
