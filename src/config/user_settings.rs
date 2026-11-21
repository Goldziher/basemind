//! The operator's machine-wide settings: `settings.toml` in basemind's user-level configuration
//! directory ([`super::agents_policy::user_config_dir`]), beside `agents.toml`.
//!
//! What it sets belongs to the machine, not to a repository, so a repository's `basemind.toml`
//! cannot set it. Unlike a `BASEMIND_*` variable, the file reaches every basemind process the same
//! way, the daemon an MCP host spawns included. A variable naming the same setting wins over the
//! file. The CLI reads the file once, when it builds the engine configuration (`cli::env_config`).

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The settings file's name inside basemind's user-level configuration directory. Not
/// `config.toml`: on Linux that directory is `~/.config/basemind/`, where a repository rooted at
/// the home directory keeps its own `.config/basemind/config.toml`.
pub const USER_SETTINGS_FILE: &str = "settings.toml";

/// What the settings file holds. An absent table or key leaves the setting to its variable or its
/// default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct UserSettings {
    /// `[cache]`: the machine-global cache.
    #[serde(default)]
    pub cache: CacheSettings,
}

/// The `[cache]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CacheSettings {
    /// Cache budget in MiB, enforced by eviction; `0` disables enforcement. Unset, the budget is
    /// disk-aware. `BASEMIND_CACHE_BUDGET_MB` wins when set.
    pub budget_mb: Option<u64>,
}

impl UserSettings {
    /// The settings in [`settings_path`]. No file, or no user configuration directory, is the
    /// defaults; so is a file that cannot be read or parsed, which is warned about once.
    pub fn load() -> Self {
        settings_path().map_or_else(Self::default, |path| Self::read(&path))
    }

    /// The settings in the file at `path`, read as [`Self::load`] documents.
    pub fn read(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(error) => return ignored(path, &error),
        };
        toml::from_str(&text).unwrap_or_else(|error| ignored(path, &error))
    }
}

/// The settings file: [`USER_SETTINGS_FILE`] in basemind's user-level configuration directory.
pub fn settings_path() -> Option<PathBuf> {
    super::agents_policy::user_config_dir().map(|dir| dir.join(USER_SETTINGS_FILE))
}

fn ignored(path: &Path, error: &dyn std::fmt::Display) -> UserSettings {
    super::trust::warn_once(format!(
        "ignoring {}: {error}; using the environment and the defaults",
        path.display()
    ));
    UserSettings::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_in(text: &str) -> UserSettings {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(USER_SETTINGS_FILE);
        std::fs::write(&path, text).expect("write settings");
        UserSettings::read(&path)
    }

    #[test]
    fn the_cache_table_sets_the_budget() {
        assert_eq!(settings_in("[cache]\nbudget_mb = 8192\n").cache.budget_mb, Some(8192));
        assert_eq!(settings_in("[cache]\nbudget_mb = 0\n").cache.budget_mb, Some(0));
        assert_eq!(settings_in("[cache]\n").cache.budget_mb, None);
        assert_eq!(settings_in("").cache.budget_mb, None);
    }

    #[test]
    fn a_missing_or_malformed_file_is_the_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            UserSettings::read(&dir.path().join(USER_SETTINGS_FILE)),
            UserSettings::default()
        );
        for text in [
            "[cache]\nbudget_mb = \"big\"\n",
            "[cache]\nbudget = 8192\n",
            "[cash]\nbudget_mb = 8192\n",
            "[cache]\nbudget_mb = -1\n",
            "not toml",
        ] {
            assert_eq!(settings_in(text), UserSettings::default(), "{text:?}");
        }
    }

    #[test]
    fn the_settings_file_lies_beside_the_agents_policy() {
        let settings = settings_path().expect("this user has a home directory");
        let policy = super::super::agents_policy::policy_path().expect("this user has a home directory");
        assert_eq!(settings.parent(), policy.parent());
        assert_eq!(settings.file_name(), Some(std::ffi::OsStr::new(USER_SETTINGS_FILE)));
    }
}
