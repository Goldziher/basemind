//! The documented `BASEMIND_*` environment variables, mapped onto the engine configuration.
//!
//! This is the one place the library reads them: the binary installs the result before any command
//! runs, and core, retrieval and engine code read the installed [`EngineConfig`] (ADR-0039). Each
//! variable keeps the meaning, defaults and parsing it had when the code that uses it read it
//! directly.

use std::ffi::OsString;

use crate::config::engine::{ExtraRootsGrant, is_truthy};
use crate::config::root_guard::ALLOW_ANY_ROOT_ENV;
use crate::config::trust::ALLOW_FOLLOW_SYMLINKS_ENV;
use crate::engine::{DataRoot, Engine, EngineConfig, EngineError};
use crate::scanner_candidates::ALLOW_EXTRA_ROOTS_ENV;
use crate::scanner_filter::ALLOW_REPO_CREDENTIALS_ENV;
use crate::store_layout::DATA_HOME_ENV;

/// `lookup` with `std::env::var`'s reading: a value that is not Unicode counts as unset.
fn text(lookup: &impl Fn(&str) -> Option<OsString>, name: &str) -> Option<String> {
    lookup(name).and_then(|value| value.into_string().ok())
}

fn grant(lookup: &impl Fn(&str) -> Option<OsString>, name: &str) -> bool {
    text(lookup, name).is_some_and(|value| is_truthy(&value))
}

/// The engine configuration the process environment describes.
pub fn engine_config_from_env() -> EngineConfig {
    crate::config::trust::warn_removed_variables(|name| std::env::var_os(name).is_some());
    engine_config_from(|name| std::env::var_os(name))
}

/// The engine configuration `lookup` describes, `lookup` standing in for `std::env::var_os`.
pub fn engine_config_from(lookup: impl Fn(&str) -> Option<OsString>) -> EngineConfig {
    let mut config = EngineConfig::default();
    if let Some(home) = lookup(DATA_HOME_ENV) {
        config.data_root = DataRoot::new(home);
    }
    config.trust.allow_repo_credentials = grant(&lookup, ALLOW_REPO_CREDENTIALS_ENV);
    config.trust.allow_any_root = grant(&lookup, ALLOW_ANY_ROOT_ENV);
    config.trust.allow_follow_symlinks = grant(&lookup, ALLOW_FOLLOW_SYMLINKS_ENV);
    if let Some(value) = text(&lookup, ALLOW_EXTRA_ROOTS_ENV) {
        config.trust.extra_roots = ExtraRootsGrant::parse(&value, ALLOW_EXTRA_ROOTS_ENV);
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

    #[test]
    fn each_trust_variable_grants_only_on_a_grant_word() {
        for name in [
            ALLOW_REPO_CREDENTIALS_ENV,
            ALLOW_ANY_ROOT_ENV,
            ALLOW_FOLLOW_SYMLINKS_ENV,
        ] {
            let read = |config: &EngineConfig| match name {
                ALLOW_REPO_CREDENTIALS_ENV => config.trust.allow_repo_credentials,
                ALLOW_ANY_ROOT_ENV => config.trust.allow_any_root,
                _ => config.trust.allow_follow_symlinks,
            };
            for word in ["1", "true", " TRUE ", "yes"] {
                assert!(read(&config_of(&[(name, word)])), "{name}={word:?} grants");
            }
            for word in ["0", "false", "no", "on", ""] {
                assert!(!read(&config_of(&[(name, word)])), "{name}={word:?} does not grant");
            }
            assert!(!read(&config_of(&[])), "{name} unset does not grant");
        }
    }

    #[test]
    fn the_extra_roots_variable_grants_every_workspace_or_a_list() {
        assert_eq!(config_of(&[]).trust.extra_roots, ExtraRootsGrant::Denied);
        assert_eq!(
            config_of(&[(ALLOW_EXTRA_ROOTS_ENV, "yes")]).trust.extra_roots,
            ExtraRootsGrant::Every
        );
        // An absolute path is spelled differently on Windows. ~keep
        #[cfg(unix)]
        assert_eq!(
            config_of(&[(ALLOW_EXTRA_ROOTS_ENV, "/work/a:relative:/work/b")])
                .trust
                .extra_roots,
            ExtraRootsGrant::Workspaces(vec!["/work/a".into(), "/work/b".into()])
        );
    }
}
