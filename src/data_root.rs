//! The data home: the directory every piece of basemind's machine-global state lives under (the
//! blob store, the per-workspace caches, the group indexes, the daemon registry).
//!
//! The library never resolves it from the environment. The process's [`EngineConfig`] carries it
//! (`crate::engine::Engine::new` installs one; the CLI resolves `BASEMIND_DATA_HOME` into it), and a
//! process that installs none gets [`DataRoot::platform_default`].
//!
//! [`EngineConfig`]: crate::config::engine::EngineConfig

use std::path::{Path, PathBuf};

use crate::store_layout::{
    BLOBS_DIR, CACHE_DIR, GROUPS_DIR, MEMORY_STORE_DIR, MEMORY_VECTORS_DIR, REPOS_DIR, WORKSPACES_DIR, repo_group_key,
    workspace_key,
};

const GRAMMARS_DIR: &str = "grammars";

/// Root of basemind's global on-disk state, shared by every workspace on the machine. Everything
/// rebuildable sits under [`DataRoot::cache_dir`]: a global `blobs/` (content-addressed) plus
/// per-workspace state under `workspaces/<workspace_key>/`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DataRoot(PathBuf);

impl DataRoot {
    /// A data home at `path`, taken verbatim.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// The platform data dir: `~/.local/share/basemind` on Linux (or under `$XDG_DATA_HOME`),
    /// `~/Library/Application Support/basemind` on macOS, `%APPDATA%\basemind\data` on Windows; the
    /// current directory when no home directory resolves.
    pub fn platform_default() -> Self {
        Self(
            directories::ProjectDirs::from("", "", "basemind")
                .map(|dirs| dirs.data_dir().to_path_buf())
                .unwrap_or_else(|| PathBuf::from(".")),
        )
    }

    /// The data home of this process's installed [`EngineConfig`](crate::config::engine::EngineConfig).
    pub fn current() -> Self {
        crate::config::engine::with(|config| config.data_root.clone())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Whether `self` and `other` are one directory: equal paths, or paths that canonicalise to the
    /// same one, so a symlinked spelling of a home is that home.
    pub fn same_home(&self, other: &DataRoot) -> bool {
        if self.0 == other.0 {
            return true;
        }
        match (self.0.canonicalize(), other.0.canonicalize()) {
            (Ok(mine), Ok(theirs)) => mine == theirs,
            _ => false,
        }
    }

    /// `<data home>/grammars`: the base of the downloaded tree-sitter grammar cache, unless the
    /// operator names another (`EngineConfig::grammars`).
    pub fn grammars_dir(&self) -> PathBuf {
        self.0.join(GRAMMARS_DIR)
    }

    /// `<data home>/cache`: all rebuildable state.
    pub fn cache_dir(&self) -> PathBuf {
        self.0.join(CACHE_DIR)
    }

    /// `<data home>/cache/workspaces`: one directory per workspace, keyed by [`workspace_key`].
    pub fn workspaces_dir(&self) -> PathBuf {
        self.cache_dir().join(WORKSPACES_DIR)
    }

    /// `<data home>/cache/workspaces/<workspace_key>/`: views, the per-workspace lock and markers.
    pub fn workspace_cache_dir(&self, root: &Path) -> PathBuf {
        self.workspaces_dir().join(workspace_key(root))
    }

    /// `<data home>/cache/repos/<group>/`: the user-authored state of the repository group `group`.
    pub fn group_repo_dir(&self, group: &str) -> PathBuf {
        self.cache_dir().join(REPOS_DIR).join(group)
    }

    /// `<data home>/cache/repos/<repo_group_key>/` of `root`.
    pub fn repo_group_dir(&self, root: &Path) -> PathBuf {
        self.group_repo_dir(&repo_group_key(root))
    }

    /// `<data home>/cache/groups/<repo_group_key>/`: the group index of `root`'s repository group.
    pub fn group_index_dir(&self, root: &Path) -> PathBuf {
        self.cache_dir().join(GROUPS_DIR).join(repo_group_key(root))
    }

    /// The group's user-memory Fjall database.
    pub fn memory_store_dir(&self, root: &Path) -> PathBuf {
        self.repo_group_dir(root).join(MEMORY_STORE_DIR)
    }

    /// The user-memory Fjall database of the group keyed `group`, for a root that is gone.
    pub fn group_memory_store_dir(&self, group: &str) -> PathBuf {
        self.group_repo_dir(group).join(MEMORY_STORE_DIR)
    }

    /// The memory notes' vector stores of every embedding model.
    pub fn memory_vectors_dir(&self, root: &Path) -> PathBuf {
        self.repo_group_dir(root).join(MEMORY_VECTORS_DIR)
    }

    /// `<data home>/cache/blobs/`: one namespace directory per blob schema.
    pub fn global_blobs_root(&self) -> PathBuf {
        self.cache_dir().join(BLOBS_DIR)
    }

    /// `<data home>/cache/blobs/v<SCHEMA_VER>/`: the namespace this binary reads and writes.
    pub fn global_blobs_dir(&self) -> PathBuf {
        self.global_blobs_root()
            .join(format!("v{}", crate::extract::SCHEMA_VER))
    }
}

impl AsRef<Path> for DataRoot {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_directory_sits_under_the_data_home() {
        let data = DataRoot::new("/data");
        let root = Path::new("/no/such/repo");
        let key = workspace_key(root);
        let group = repo_group_key(root);

        assert_eq!(
            data.workspace_cache_dir(root),
            Path::new("/data/cache/workspaces").join(&key)
        );
        assert_eq!(data.group_index_dir(root), Path::new("/data/cache/groups").join(&group));
        assert_eq!(
            data.memory_store_dir(root),
            Path::new("/data/cache/repos").join(&group).join(MEMORY_STORE_DIR)
        );
        assert_eq!(
            data.global_blobs_dir(),
            Path::new(&format!("/data/cache/blobs/v{}", crate::extract::SCHEMA_VER))
        );
    }
}
