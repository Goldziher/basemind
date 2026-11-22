//! Process-level engine settings: what the operator chose for this process, as opposed to
//! `basemind.toml`, which the scanned repository authors and so may never grant itself anything.
//!
//! The root layer builds one (the CLI maps the documented `BASEMIND_*` environment variables onto
//! it in `cli::env_config`) and installs it through `crate::engine::Engine::new`. Core, retrieval
//! and engine code read the installed one through [`with`] and never the environment (ADR-0050).
//! A process that installs nothing gets [`EngineConfig::default`]: the platform data dir and the
//! documented defaults.
//!
//! One configuration per process (ADR-0050): the blob store, the daemon registry and the parser
//! pool are process-wide, so a second engine with another configuration in the same process is
//! refused. `update` is a test seam that bypasses that rule.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Git resource caps and the git-history index.
    pub git: GitLimits,
    /// Fjall block cache of an opened index in bytes, replacing the size heuristic
    /// (`BASEMIND_INDEX_CACHE_BYTES`).
    pub index_cache_bytes: Option<u64>,
    /// Let `code_grep` skip files by their trigram bloom; `false` forces the full sweep
    /// (`BASEMIND_GREP_BLOOM=0`).
    pub grep_bloom: bool,
    /// Ceilings the daemon applies to every workspace's `basemind.toml`.
    pub daemon_caps: super::daemon::DaemonCaps,
    /// How many heavy and medium MCP calls a server runs at once, and how long one waits for a slot.
    pub admission: super::daemon::AdmissionLimits,
    /// Package caches outside the repository that `[documents.dependencies]` reads from.
    pub dependency_homes: DependencyHomes,
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
            git: GitLimits::default(),
            index_cache_bytes: None,
            grep_bloom: true,
            daemon_caps: super::daemon::DaemonCaps::default(),
            admission: super::daemon::AdmissionLimits::default(),
            dependency_homes: DependencyHomes::default(),
        }
    }
}

/// The package caches outside any repository that dependency documentation is read from (ADR-0049).
/// `None` is a cache this process does not know of; its packages are listed as not installed. Each
/// is read only under [`TrustPolicy::dependency_roots`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DependencyHomes {
    /// Cargo's home, holding `registry/src/*/<name>-<version>` (`CARGO_HOME`, else `~/.cargo`).
    pub cargo_home: Option<PathBuf>,
    /// The Go module cache, holding `<module>@<version>` (`GOMODCACHE`, else `$GOPATH/pkg/mod`,
    /// else `~/go/pkg/mod`).
    pub go_mod_cache: Option<PathBuf>,
    /// The active Python virtual environment (`VIRTUAL_ENV`).
    pub virtual_env: Option<PathBuf>,
}

/// Git resource caps and the git-history index.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GitLimits {
    /// gix's per-repository delta-base cache in bytes; `0` disables it
    /// (`BASEMIND_GIT_DELTA_CACHE_BYTES`).
    pub delta_cache_bytes: usize,
    /// A file larger than this is not blamed (`BASEMIND_BLAME_MAX_BYTES`).
    pub blame_max_bytes: u64,
    /// A file with more lines than this is not blamed (`BASEMIND_BLAME_MAX_LINES`).
    pub blame_max_lines: u64,
    /// Disk budget of each git-cache subdirectory; `0` disables eviction
    /// (`BASEMIND_GIT_CACHE_LOG_MAX_BYTES`).
    pub cache_max_bytes: u64,
    /// Index an ancestor repository's history for a workspace that is a subdirectory of it
    /// (`BASEMIND_GIT_DISCOVER_PARENTS`).
    pub discover_parents: bool,
    /// Build and read the git-history index (`BASEMIND_GH_INDEX`).
    pub history_index: bool,
}

impl Default for GitLimits {
    fn default() -> Self {
        Self {
            delta_cache_bytes: crate::git::DELTA_BASE_CACHE_BYTES,
            blame_max_bytes: crate::git::BLAME_DEFAULT_MAX_BYTES,
            blame_max_lines: crate::git::BLAME_DEFAULT_MAX_LINES,
            cache_max_bytes: crate::git_cache::LOG_CACHE_DEFAULT_MAX_BYTES,
            discover_parents: false,
            history_index: true,
        }
    }
}

/// What the machine-wide cache sweeps keep.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GcPolicy {
    /// Cache budget, disk-aware unless set; `None` disables enforcement (`BASEMIND_CACHE_BUDGET_MB`, else
    /// `[cache] budget_mb` in the user settings file).
    pub budget: Option<crate::store_gc_budget::CacheBudget>,
    /// A workspace built this recently is not evicted for the budget (`BASEMIND_CACHE_GRACE_MINUTES`).
    pub grace: Duration,
    /// An unreferenced blob this young is kept (`BASEMIND_BLOB_GC_GRACE_SECS`).
    pub blob_grace: Duration,
    /// An older basemind's blob namespace written this recently is kept (`BASEMIND_PRUNE_AFTER_DAYS`).
    pub superseded_hold: Duration,
}

impl Default for GcPolicy {
    fn default() -> Self {
        Self {
            budget: crate::store_gc_budget::cache_budget_from(None),
            grace: crate::store_gc_protect::cache_grace_from(None),
            blob_grace: crate::store_gc::grace_from(None),
            superseded_hold: crate::store_gc_namespaces::prune_hold_from(None),
        }
    }
}

/// Where tree-sitter grammars may come from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GrammarPolicy {
    /// Never download a missing grammar; a scan that needs one fails (`BASEMIND_OFFLINE`,
    /// `BASEMIND_GRAMMAR_OFFLINE`).
    pub offline: bool,
    /// Base directory of the downloaded grammar cache (`TREE_SITTER_LANGUAGE_PACK_CACHE_DIR`).
    /// `None` keeps it under the data home, at [`DataRoot::grammars_dir`].
    pub cache_dir: Option<PathBuf>,
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
    /// The workspaces whose `[documents.dependencies]` may read package roots outside the
    /// repository: [`DependencyHomes`], or an in-tree root that resolves outside it
    /// (`BASEMIND_ALLOW_DEPENDENCY_ROOTS`, same forms as `BASEMIND_ALLOW_EXTRA_ROOTS`).
    pub dependency_roots: ExtraRootsGrant,
}

/// Which workspaces an operator grant covers: the `[scan] extra_roots` their `basemind.toml` names,
/// or the dependency package roots outside their tree.
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
    /// Read a grant written as a boolean ([`parse_bool`]: true is every workspace, false none) or a
    /// path list (`:`-separated on Unix, `;` on Windows) of absolute workspace roots. A relative
    /// entry is ignored with a warning: canonicalised against a daemon's cwd (often `/`) it could
    /// cover every workspace.
    pub fn parse(value: &str, name: &str) -> Self {
        match parse_bool(value) {
            Some(true) => return Self::Every,
            Some(false) => return Self::Denied,
            None => {}
        }
        let roots = std::env::split_paths(value.trim())
            .filter(|entry| {
                let absolute = entry.is_absolute();
                if !absolute && !entry.as_os_str().is_empty() {
                    super::trust::warn_once(format!(
                        "{name} entry {entry:?} ignored: use {BOOL_SPELLINGS}, or a list of absolute workspace paths"
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

/// The spellings every boolean `BASEMIND_*` variable accepts, as error messages name them.
pub const BOOL_SPELLINGS: &str = "1, true, yes, on or 0, false, no, off";

/// A boolean `BASEMIND_*` value: `1` / `true` / `yes` / `on` or `0` / `false` / `no` / `off`, case-
/// and whitespace-insensitive. `None` for any other value. The one parser every boolean variable
/// goes through.
pub fn parse_bool(value: &str) -> Option<bool> {
    let value = value.trim();
    let is = |words: [&str; 4]| words.iter().any(|word| value.eq_ignore_ascii_case(word));
    if is(["1", "true", "yes", "on"]) {
        Some(true)
    } else if is(["0", "false", "no", "off"]) {
        Some(false)
    } else {
        None
    }
}

/// The boolean variable `name` set to `value`: [`parse_bool`]'s answer, or an error naming the
/// variable when `value` is not one of its spellings.
pub fn bool_var(name: &str, value: &str) -> Result<bool, String> {
    parse_bool(value).ok_or_else(|| format!("{name}={value:?} is not a boolean: use {BOOL_SPELLINGS}"))
}

/// The boolean variable `name` as the environment holds it: `Ok(None)` when it is unset or blank,
/// else [`bool_var`]'s answer.
pub fn bool_env(name: &str, value: Option<&str>) -> Result<Option<bool>, String> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(|value| bool_var(name, value))
        .transpose()
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

/// Bumped after every install, so a thread's [`SNAPSHOT`] knows when to refresh.
static GENERATION: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// This thread's copy of the installed configuration and the [`GENERATION`] it was taken at.
    /// [`with`] runs on the parse hot path (twice per file on every scan worker); reading the
    /// shared lock there costs an atomic read-modify-write on one cache line, about 0.7 µs a read
    /// with 14 threads contending, where the snapshot costs one shared atomic load.
    static SNAPSHOT: RefCell<Option<(u64, Arc<EngineConfig>)>> = const { RefCell::new(None) };
}

fn default_config() -> &'static Arc<EngineConfig> {
    static DEFAULT: OnceLock<Arc<EngineConfig>> = OnceLock::new();
    DEFAULT.get_or_init(|| Arc::new(EngineConfig::default()))
}

fn locked() -> Arc<EngineConfig> {
    INSTALLED
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| Arc::clone(default_config()))
}

/// Run `read` against the installed configuration, or the default when none is installed.
pub fn with<R>(read: impl FnOnce(&EngineConfig) -> R) -> R {
    // A lib test reads the configuration only after the isolated cache is in place, whether or not
    // it asked for it: one that read it first would see the developer's data home. ~keep
    #[cfg(test)]
    crate::store_layout::init_isolated_cache();
    // Loaded before the lock is read: an install between the two leaves a newer configuration
    // under an older generation, which the next call refreshes. ~keep
    let generation = GENERATION.load(Ordering::Acquire);
    let mut read = Some(read);
    let answered = SNAPSHOT.try_with(|cell| {
        let fresh = matches!(&*cell.borrow(), Some((taken, _)) if *taken == generation);
        if !fresh {
            // A nested read inside a stale snapshot's `read` cannot replace it; it takes the lock.
            let Ok(mut slot) = cell.try_borrow_mut() else {
                return read.take().map(|read| read(&locked()));
            };
            *slot = Some((generation, locked()));
        }
        let slot = cell.borrow();
        let (_, config) = slot.as_ref()?;
        read.take().map(|read| read(config))
    });
    match (answered, read) {
        (Ok(Some(answer)), _) => answer,
        // The thread is exiting and its snapshot is gone. ~keep
        (_, Some(read)) => read(&locked()),
        (_, None) => unreachable!("`read` ran but returned no answer"),
    }
}

/// The installed configuration, or the default when none is installed.
pub fn current() -> Arc<EngineConfig> {
    #[cfg(test)]
    crate::store_layout::init_isolated_cache();
    locked()
}

/// Whether a configuration was installed, by an engine or a test seam, rather than defaulted.
pub(crate) fn is_installed() -> bool {
    INSTALLED.read().unwrap_or_else(PoisonError::into_inner).is_some()
}

/// Make `config` this process's configuration. `crate::engine::Engine::new` is the public way in.
pub(crate) fn install(config: EngineConfig) -> Arc<EngineConfig> {
    let config = Arc::new(config);
    *INSTALLED.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&config));
    GENERATION.fetch_add(1, Ordering::Release);
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
    GENERATION.fetch_add(1, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bool_reads_every_spelling_and_nothing_else() {
        let cases: &[(&str, Option<bool>)] = &[
            ("1", Some(true)),
            ("true", Some(true)),
            ("yes", Some(true)),
            ("on", Some(true)),
            ("TRUE", Some(true)),
            ("Yes", Some(true)),
            ("ON", Some(true)),
            (" true ", Some(true)),
            ("0", Some(false)),
            ("false", Some(false)),
            ("no", Some(false)),
            ("off", Some(false)),
            ("FALSE", Some(false)),
            ("No", Some(false)),
            ("OFF", Some(false)),
            ("\toff\n", Some(false)),
            ("", None),
            ("  ", None),
            ("2", None),
            ("-1", None),
            ("enabled", None),
            ("disabled", None),
            ("y", None),
            ("n", None),
            ("t", None),
            ("truthy", None),
            ("yes please", None),
            ("o n", None),
        ];
        for (value, expected) in cases {
            assert_eq!(parse_bool(value), *expected, "{value:?}");
        }
    }

    #[test]
    fn bool_var_names_the_variable_it_refuses() {
        assert_eq!(bool_var("BASEMIND_X", "off"), Ok(false));
        assert_eq!(
            bool_var("BASEMIND_X", "maybe"),
            Err("BASEMIND_X=\"maybe\" is not a boolean: use 1, true, yes, on or 0, false, no, off".to_string())
        );
    }

    #[test]
    fn bool_env_treats_unset_and_blank_as_unset() {
        let cases: &[(Option<&str>, Option<bool>)] = &[
            (None, None),
            (Some(""), None),
            (Some(" "), None),
            (Some("on"), Some(true)),
            (Some("false"), Some(false)),
        ];
        for (value, expected) in cases {
            assert_eq!(bool_env("BASEMIND_X", *value), Ok(*expected), "{value:?}");
        }
        assert!(bool_env("BASEMIND_X", Some("nope")).is_err_and(|message| message.starts_with("BASEMIND_X=")));
    }

    #[test]
    fn an_extra_roots_grant_reads_a_boolean_before_a_path_list() {
        let parse = |value: &str| ExtraRootsGrant::parse(value, "BASEMIND_ALLOW_EXTRA_ROOTS");
        for word in ["1", "true", "YES", "on"] {
            assert_eq!(parse(word), ExtraRootsGrant::Every, "{word:?}");
        }
        for word in ["0", "false", "No", "OFF"] {
            assert_eq!(parse(word), ExtraRootsGrant::Denied, "{word:?}");
        }
    }

    /// A thread that read the configuration sees a later install on its next read, nested reads
    /// included.
    #[test]
    fn a_snapshot_is_refreshed_by_the_next_install() {
        // A setting no other lib test reads, and any value of it is valid for the index. ~keep
        let before = with(|config| config.index_cache_bytes);
        update(|config| config.index_cache_bytes = Some(1 << 21));
        assert_eq!(with(|config| config.index_cache_bytes), Some(1 << 21));
        let nested = with(|_| {
            update(|config| config.index_cache_bytes = Some(1 << 22));
            with(|config| config.index_cache_bytes)
        });
        assert_eq!(
            nested,
            Some(1 << 22),
            "a nested read inside a stale snapshot takes the lock"
        );
        assert_eq!(with(|config| config.index_cache_bytes), Some(1 << 22));
        update(|config| config.index_cache_bytes = before);
    }
}
