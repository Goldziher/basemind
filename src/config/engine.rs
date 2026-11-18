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

use std::sync::{Arc, OnceLock, PoisonError, RwLock};

use crate::data_root::DataRoot;

/// The settings an embedding program (or the CLI, from the environment) gives the engine.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct EngineConfig {
    /// Root of every piece of machine-global state (`BASEMIND_DATA_HOME`).
    pub data_root: DataRoot,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            data_root: DataRoot::platform_default(),
        }
    }
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

/// Point the installed configuration (the default when none is) at `data_root`, keeping every other
/// setting. The test-isolation helper's seam; it never reads the configuration through [`with`],
/// which would re-enter it.
#[cfg(any(feature = "test-support", test))]
pub(crate) fn set_data_root(data_root: DataRoot) {
    let mut installed = INSTALLED.write().unwrap_or_else(PoisonError::into_inner);
    let mut config = installed
        .as_deref()
        .cloned()
        .unwrap_or_else(|| EngineConfig::clone(default_config()));
    config.data_root = data_root;
    *installed = Some(Arc::new(config));
}
