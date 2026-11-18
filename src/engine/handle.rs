//! The process's engine handle: it installs the [`EngineConfig`] every layer below reads in place
//! of the environment (ADR-0050).

use std::sync::{Arc, Mutex, PoisonError};

use crate::config::engine::{self as engine_config, EngineConfig};
use crate::data_root::DataRoot;
use crate::engine::EngineError;

/// The configuration the first engine of this process installed. The blob store, the parser pool,
/// the daemon registry and the resource ceilings are process-wide, so every later engine shares it.
static BUILT: Mutex<Option<Arc<EngineConfig>>> = Mutex::new(None);

/// The engine of this process: the configuration every workspace it serves is read under.
///
/// A process has one configuration. The first `Engine::new` installs it; a later one with the same
/// configuration is another handle on it, and one with any other configuration is refused, so the
/// [`Engine::config`] of every handle is the configuration the library uses.
#[derive(Debug, Clone)]
pub struct Engine {
    config: Arc<EngineConfig>,
}

impl Engine {
    /// Install `config` as this process's configuration, or return a handle on the installed one
    /// when `config` equals it (the data home compared as a directory, so a symlinked or trailing-`/`
    /// spelling of it is the same home).
    ///
    /// # Errors
    ///
    /// [`EngineError::Unavailable`] when this process already built an engine with another
    /// configuration, or when its data home differs from the one already installed.
    pub fn new(config: EngineConfig) -> Result<Self, EngineError> {
        let mut built = BUILT.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(first) = built.as_ref() {
            if !same_config(first, &config) {
                return Err(EngineError::unavailable(refusal(first, &config)));
            }
            return Ok(Self {
                config: Arc::clone(first),
            });
        }
        let installed = engine_config::current();
        if engine_config::is_installed() && !installed.data_root.same_home(&config.data_root) {
            return Err(EngineError::unavailable(refusal(&installed, &config)));
        }
        let config = engine_config::install(config);
        *built = Some(Arc::clone(&config));
        Ok(Self { config })
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    pub fn data_root(&self) -> &DataRoot {
        &self.config.data_root
    }
}

/// `a` and `b` are one configuration: every setting equal, the data homes one directory.
fn same_config(a: &EngineConfig, b: &EngineConfig) -> bool {
    a.data_root.same_home(&b.data_root)
        && *a
            == EngineConfig {
                data_root: a.data_root.clone(),
                ..b.clone()
            }
}

fn refusal(installed: &EngineConfig, wanted: &EngineConfig) -> String {
    if installed.data_root.same_home(&wanted.data_root) {
        "this process already runs an engine with another configuration; every engine of a process \
         shares the first one's"
            .to_string()
    } else {
        format!(
            "this process already serves the data home {}; an engine for {} needs its own process",
            installed.data_root.path().display(),
            wanted.data_root.path().display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, because the engine is process-wide: the steps would race as separate tests.
    #[test]
    fn a_process_has_one_engine_configuration() {
        let current = engine_config::current();
        let engine = Engine::new(EngineConfig::clone(&current)).expect("the process's own configuration");
        assert_eq!(engine.data_root(), &current.data_root);

        let again = Engine::new(EngineConfig::clone(&current)).expect("the same configuration again");
        assert!(
            Arc::ptr_eq(&engine.config, &again.config),
            "a handle on the installed one"
        );

        let mut spelled = EngineConfig::clone(&current);
        spelled.data_root = DataRoot::new(current.data_root.path().join("."));
        Engine::new(spelled).expect("another spelling of the same home");

        let mut granted = EngineConfig::clone(&current);
        granted.trust.allow_follow_symlinks = !granted.trust.allow_follow_symlinks;
        let refused = Engine::new(granted);
        assert!(matches!(refused, Err(EngineError::Unavailable(_))), "{refused:?}");

        let elsewhere = tempfile::tempdir().expect("tempdir");
        let refused = Engine::new(EngineConfig::new(DataRoot::new(elsewhere.path())));
        assert!(matches!(refused, Err(EngineError::Unavailable(_))), "{refused:?}");
        assert_eq!(
            DataRoot::current(),
            current.data_root,
            "a refused engine installs nothing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_home_is_the_same_home() {
        let home = tempfile::tempdir().expect("tempdir");
        let link = home.path().join("link");
        std::os::unix::fs::symlink(home.path(), &link).expect("symlink");
        let mut first = EngineConfig::new(DataRoot::new(home.path()));
        first.parse_timeout = std::time::Duration::from_millis(7);
        let mut second = first.clone();
        second.data_root = DataRoot::new(&link);
        assert!(same_config(&first, &second));
        second.parse_timeout = std::time::Duration::from_millis(8);
        assert!(!same_config(&first, &second));
    }
}
