//! The documented `BASEMIND_*` environment variables, mapped onto the engine configuration.
//!
//! This is the one place the library reads them: the binary installs the result before any command
//! runs, and core, retrieval and engine code read the installed [`EngineConfig`] (ADR-0039). Each
//! variable keeps the meaning, defaults and parsing it had when the code that uses it read it
//! directly.

use std::ffi::OsString;

use crate::engine::{DataRoot, Engine, EngineConfig, EngineError};
use crate::store_layout::DATA_HOME_ENV;

/// The engine configuration the process environment describes.
pub fn engine_config_from_env() -> EngineConfig {
    engine_config_from(|name| std::env::var_os(name))
}

/// The engine configuration `lookup` describes, `lookup` standing in for `std::env::var_os`.
pub fn engine_config_from(lookup: impl Fn(&str) -> Option<OsString>) -> EngineConfig {
    let mut config = EngineConfig::default();
    if let Some(home) = lookup(DATA_HOME_ENV) {
        config.data_root = DataRoot::new(home);
    }
    config
}

/// Build this process's engine from its environment.
pub fn install_from_env() -> Result<Engine, EngineError> {
    Engine::new(engine_config_from_env())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config_of(vars: &[(&str, &str)]) -> EngineConfig {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(name, value)| ((*name).to_string(), OsString::from(value)))
            .collect();
        engine_config_from(|name| vars.get(name).cloned())
    }

    #[test]
    fn the_data_home_variable_sets_the_data_root_verbatim() {
        assert_eq!(
            config_of(&[(DATA_HOME_ENV, "/srv/basemind")]).data_root,
            DataRoot::new("/srv/basemind")
        );
        assert_eq!(config_of(&[]).data_root, DataRoot::platform_default());
    }
}
