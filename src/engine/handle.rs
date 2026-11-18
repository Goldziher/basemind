//! The process's engine handle: it installs the [`EngineConfig`] every layer below reads in place
//! of the environment.

use std::sync::{Arc, OnceLock};

use crate::config::engine::{self as engine_config, EngineConfig};
use crate::data_root::DataRoot;
use crate::engine::EngineError;

/// The data home of the first engine this process built. The blob store, the parser pool and the
/// daemon registry are process-wide, so a second engine may not point them elsewhere.
static FIRST_DATA_ROOT: OnceLock<DataRoot> = OnceLock::new();

/// The engine of this process: the configuration every workspace it serves is read under.
#[derive(Debug, Clone)]
pub struct Engine {
    config: Arc<EngineConfig>,
}

impl Engine {
    /// Install `config` as this process's configuration. A later engine of the same process may
    /// change any setting but the data home.
    pub fn new(config: EngineConfig) -> Result<Self, EngineError> {
        let first = FIRST_DATA_ROOT.get_or_init(|| config.data_root.clone());
        if *first != config.data_root {
            return Err(EngineError::unavailable(format!(
                "this process already serves the data home {}; an engine for {} needs its own process",
                first.path().display(),
                config.data_root.path().display()
            )));
        }
        Ok(Self {
            config: engine_config::install(config),
        })
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    pub fn data_root(&self) -> &DataRoot {
        &self.config.data_root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_engine_keeps_the_data_home_of_the_first() {
        let current = engine_config::current();
        let engine = Engine::new(EngineConfig::clone(&current)).expect("the process's own data home");
        assert_eq!(engine.data_root(), &current.data_root);

        let elsewhere = tempfile::tempdir().expect("tempdir");
        let refused = Engine::new(EngineConfig::new(DataRoot::new(elsewhere.path())));
        assert!(matches!(refused, Err(EngineError::Unavailable(_))), "{refused:?}");
        assert_eq!(
            DataRoot::current(),
            current.data_root,
            "a refused engine installs nothing"
        );
    }
}
