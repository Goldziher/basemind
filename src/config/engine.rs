//! Process-level engine settings: what the operator chose for this process, as opposed to
//! `basemind.toml`, which the scanned repository authors and so may never grant itself anything.
//!
//! The root layer builds one (the CLI maps the documented `BASEMIND_*` environment variables onto
//! it in `cli::env_config`) and installs it through `crate::engine::Engine::new`. Core, retrieval
//! and engine code read the installed one through [`with`] and never the environment (ADR-0039).
//! A process that installs nothing gets [`EngineConfig::default`]: the platform data dir and the
//! documented defaults.
//!
//! One configuration per process: the blob store, the daemon registry and the parser pool are
//! process-wide, so a second engine with another data home in the same process is refused.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};
use std::time::Duration;

use crate::data_root::DataRoot;

/// The settings an embedding program (or the CLI, from the environment) gives the engine.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct EngineConfig {
    /// Root of every piece of machine-global state (`BASEMIND_DATA_HOME`).
    pub data_root: DataRoot,
    /// What the operator lets a repository's `basemind.toml` reach outside the repository.
    pub trust: TrustPolicy,
    /// Where tree-sitter grammars may come from.
    pub grammars: GrammarPolicy,
    /// How long one parse may run before it is abandoned (`BASEMIND_PARSE_TIMEOUT_MS`).
    pub parse_timeout: Duration,
    /// What the machine-wide cache sweeps keep.
    pub gc: GcPolicy,
    /// Free bytes a scan leaves on the index volume beyond its size estimate
    /// (`BASEMIND_MIN_FREE_MB`).
    pub min_free_bytes: u64,
    /// Live daemons per kind before a spawn is refused (`BASEMIND_MAX_DAEMONS`).
    pub max_live_daemons: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            data_root: DataRoot::platform_default(),
            trust: TrustPolicy::default(),
            grammars: GrammarPolicy::default(),
            parse_timeout: crate::lang::DEFAULT_PARSE_TIMEOUT,
            gc: GcPolicy::default(),
            min_free_bytes: crate::disk_space::headroom_bytes(None),
            max_live_daemons: crate::daemon_lock::MAX_LIVE_DAEMONS,
        }
    }
}

/// What the machine-wide cache sweeps keep.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GcPolicy {
    /// Cache byte budget; `None` disables enforcement (`BASEMIND_CACHE_BUDGET_MB`).
    pub budget_bytes: Option<u64>,
    /// A workspace built this recently is not evicted for the budget (`BASEMIND_CACHE_GRACE_MINUTES`).
    pub grace: Duration,
    /// An unreferenced blob this young is kept (`BASEMIND_BLOB_GC_GRACE_SECS`).
    pub blob_grace: Duration,
}

impl Default for GcPolicy {
    fn default() -> Self {
        Self {
            budget_bytes: crate::store_gc_budget::cache_budget_from(None),
            grace: crate::store_gc_protect::cache_grace_from(None),
            blob_grace: crate::store_gc::grace_from(None),
        }
    }
}

/// Where tree-sitter grammars may come from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GrammarPolicy {
    /// Never download a missing grammar; a scan that needs one fails (`BASEMIND_GRAMMAR_OFFLINE`).
    pub offline: bool,
}

/// Operator grants. A repository's own `basemind.toml` can ask for each of these but never grant
/// it, because the scanned repository authors that file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TrustPolicy {
    /// Let `[scan] floor_allow` re-include credential material (`.env`, keys)
    /// (`BASEMIND_ALLOW_REPO_CREDENTIALS`).
    pub allow_repo_credentials: bool,
    /// Admit a workspace root that is neither a git work tree nor a basemind project
    /// (`BASEMIND_ALLOW_ANY_ROOT`).
    pub allow_any_root: bool,
    /// Honour `[scan] follow_symlinks = true` (`BASEMIND_ALLOW_FOLLOW_SYMLINKS`).
    pub allow_follow_symlinks: bool,
    /// The workspaces whose `[scan] extra_roots` are walked (`BASEMIND_ALLOW_EXTRA_ROOTS`).
    pub extra_roots: ExtraRootsGrant,
}

/// Which workspaces may walk the `[scan] extra_roots` their `basemind.toml` names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ExtraRootsGrant {
    /// None: the entries are ignored with a warning.
    #[default]
    Denied,
    /// Every workspace this process scans.
    Every,
    /// These workspace roots (absolute) and their descendants. The form a daemon, one process
    /// serving many repositories, needs: the blanket grant would let any later workspace through.
    Workspaces(Vec<PathBuf>),
}

impl ExtraRootsGrant {
    /// Read a grant written as `1` / `true` / `yes` (every workspace) or a path list (`:`-separated
    /// on Unix, `;` on Windows) of absolute workspace roots. A relative entry is ignored with a
    /// warning: canonicalised against a daemon's cwd (often `/`) it could cover every workspace.
    pub fn parse(value: &str, name: &str) -> Self {
        if is_truthy(value) {
            return Self::Every;
        }
        let roots = std::env::split_paths(value.trim())
            .filter(|entry| {
                let absolute = entry.is_absolute();
                if !absolute && !entry.as_os_str().is_empty() {
                    super::trust::warn_once(format!(
                        "{name} entry {entry:?} ignored: use 1/true/yes or a list of absolute workspace paths"
                    ));
                }
                absolute
            })
            .collect();
        Self::Workspaces(roots)
    }

    /// Whether the grant covers the workspace at `repo_root`.
    pub fn covers(&self, repo_root: &Path) -> bool {
        match self {
            Self::Denied => false,
            Self::Every => true,
            Self::Workspaces(roots) => {
                let repo_root = repo_root.canonicalize().unwrap_or_else(|_| repo_root.to_path_buf());
                roots
                    .iter()
                    .any(|entry| repo_root.starts_with(entry.canonicalize().unwrap_or_else(|_| entry.clone())))
            }
        }
    }
}

/// `1` / `true` / `yes`, case- and whitespace-insensitive: the grant words of every `BASEMIND_ALLOW_*`
/// variable.
pub fn is_truthy(value: &str) -> bool {
    let value = value.trim();
    value.eq_ignore_ascii_case("1") || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
}

impl EngineConfig {
    /// The defaults, with the data home at `data_root`.
    pub fn new(data_root: DataRoot) -> Self {
        Self {
            data_root,
            ..Self::default()
        }
    }
}

static INSTALLED: RwLock<Option<Arc<EngineConfig>>> = RwLock::new(None);

fn default_config() -> &'static Arc<EngineConfig> {
    static DEFAULT: OnceLock<Arc<EngineConfig>> = OnceLock::new();
    DEFAULT.get_or_init(|| Arc::new(EngineConfig::default()))
}

/// Run `read` against the installed configuration, or the default when none is installed.
pub fn with<R>(read: impl FnOnce(&EngineConfig) -> R) -> R {
    // A lib test reads the configuration only after the isolated cache is in place, whether or not
    // it asked for it: one that read it first would see the developer's data home. ~keep
    #[cfg(test)]
    crate::store_layout::init_isolated_cache();
    let installed = INSTALLED.read().unwrap_or_else(PoisonError::into_inner);
    match installed.as_ref() {
        Some(config) => read(config),
        None => read(default_config()),
    }
}

/// The installed configuration, or the default when none is installed.
pub fn current() -> Arc<EngineConfig> {
    #[cfg(test)]
    crate::store_layout::init_isolated_cache();
    INSTALLED
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| Arc::clone(default_config()))
}

/// Make `config` this process's configuration. `crate::engine::Engine::new` is the public way in.
pub(crate) fn install(config: EngineConfig) -> Arc<EngineConfig> {
    let config = Arc::new(config);
    *INSTALLED.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&config));
    config
}

/// Apply `change` to the installed configuration (the default when none is) and install the
/// result, for a test that flips one setting. The read and the install are one step, so concurrent
/// updates do not lose each other's changes.
#[cfg(any(feature = "test-support", test))]
pub fn update(change: impl FnOnce(&mut EngineConfig)) {
    let mut installed = INSTALLED.write().unwrap_or_else(PoisonError::into_inner);
    let mut config = installed
        .as_deref()
        .cloned()
        .unwrap_or_else(|| EngineConfig::clone(default_config()));
    change(&mut config);
    *installed = Some(Arc::new(config));
}

/// Point the installed configuration (the default when none is) at `data_root`, keeping every other
/// setting. The test-isolation helper's seam; it never reads the configuration through [`with`],
/// which would re-enter it.
#[cfg(any(feature = "test-support", test))]
pub(crate) fn set_data_root(data_root: DataRoot) {
    update(|config| config.data_root = data_root);
}
