//! The documented `BASEMIND_*` environment variables, mapped onto the engine configuration.
//!
//! This is the one place the library reads them: the binary installs the result before any command
//! runs, and core, retrieval and engine code read the installed [`EngineConfig`] (ADR-0039). Each
//! variable keeps the meaning, defaults and parsing it had when the code that uses it read it
//! directly.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::daemon::DaemonCaps;
use crate::config::engine::{ExtraRootsGrant, is_truthy};
use crate::config::root_guard::ALLOW_ANY_ROOT_ENV;
use crate::config::trust::ALLOW_FOLLOW_SYMLINKS_ENV;
use crate::daemon_lock::{MAX_LIVE_DAEMONS_ENV, max_live_daemons_from};
use crate::disk_space::{MIN_FREE_ENV, headroom_bytes};
use crate::engine::{DataRoot, Engine, EngineConfig, EngineError};
use crate::git_history::{DISCOVER_PARENTS_ENV, HISTORY_INDEX_ENV};
use crate::index::INDEX_CACHE_BYTES_ENV;
use crate::scanner_candidates::ALLOW_EXTRA_ROOTS_ENV;
use crate::scanner_filter::ALLOW_REPO_CREDENTIALS_ENV;
use crate::store_gc::{BLOB_GC_GRACE_ENV, grace_from};
use crate::store_gc_budget::{CACHE_BUDGET_ENV, cache_budget_from};
use crate::store_gc_protect::{CACHE_GRACE_ENV, cache_grace_from};
use crate::store_layout::DATA_HOME_ENV;

pub const PARSE_TIMEOUT_MS_ENV: &str = "BASEMIND_PARSE_TIMEOUT_MS";
pub const GRAMMAR_OFFLINE_ENV: &str = "BASEMIND_GRAMMAR_OFFLINE";
pub const GIT_DELTA_CACHE_BYTES_ENV: &str = "BASEMIND_GIT_DELTA_CACHE_BYTES";
pub const BLAME_MAX_BYTES_ENV: &str = "BASEMIND_BLAME_MAX_BYTES";
pub const BLAME_MAX_LINES_ENV: &str = "BASEMIND_BLAME_MAX_LINES";
pub const GIT_CACHE_MAX_BYTES_ENV: &str = "BASEMIND_GIT_CACHE_LOG_MAX_BYTES";
pub const GREP_BLOOM_ENV: &str = "BASEMIND_GREP_BLOOM";

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
        config.data_root = data_root_from(home);
    }
    config.trust.allow_repo_credentials = grant(&lookup, ALLOW_REPO_CREDENTIALS_ENV);
    config.trust.allow_any_root = grant(&lookup, ALLOW_ANY_ROOT_ENV);
    config.trust.allow_follow_symlinks = grant(&lookup, ALLOW_FOLLOW_SYMLINKS_ENV);
    if let Some(value) = text(&lookup, ALLOW_EXTRA_ROOTS_ENV) {
        config.trust.extra_roots = ExtraRootsGrant::parse(&value, ALLOW_EXTRA_ROOTS_ENV);
    }
    if let Some(millis) = text(&lookup, PARSE_TIMEOUT_MS_ENV).and_then(|raw| raw.parse::<u64>().ok()) {
        config.parse_timeout = Duration::from_millis(millis);
    }
    config.grammars.offline = text(&lookup, GRAMMAR_OFFLINE_ENV).is_some_and(|v| v != "0" && !v.is_empty());
    config.gc.budget_bytes = cache_budget_from(text(&lookup, CACHE_BUDGET_ENV).as_deref());
    config.gc.grace = cache_grace_from(text(&lookup, CACHE_GRACE_ENV).as_deref());
    config.gc.blob_grace = grace_from(text(&lookup, BLOB_GC_GRACE_ENV).as_deref());
    config.min_free_bytes = headroom_bytes(text(&lookup, MIN_FREE_ENV).as_deref());
    config.max_live_daemons = max_live_daemons_from(text(&lookup, MAX_LIVE_DAEMONS_ENV).as_deref());
    git(&lookup, &mut config);
    config.index_cache_bytes = text(&lookup, INDEX_CACHE_BYTES_ENV).and_then(|raw| raw.parse::<u64>().ok());
    config.grep_bloom = lookup(GREP_BLOOM_ENV).is_none_or(|value| value != "0");
    config.daemon_caps = DaemonCaps::from_vars(|name| text(&lookup, name));
    config
}

fn git(lookup: &impl Fn(&str) -> Option<OsString>, config: &mut EngineConfig) {
    let git = &mut config.git;
    if let Some(bytes) = text(lookup, GIT_DELTA_CACHE_BYTES_ENV).and_then(|raw| raw.trim().parse::<usize>().ok()) {
        git.delta_cache_bytes = bytes;
    }
    if let Some(bytes) = text(lookup, BLAME_MAX_BYTES_ENV).and_then(|raw| raw.parse::<u64>().ok()) {
        git.blame_max_bytes = bytes;
    }
    if let Some(lines) = text(lookup, BLAME_MAX_LINES_ENV).and_then(|raw| raw.parse::<u64>().ok()) {
        git.blame_max_lines = lines;
    }
    if let Some(bytes) = text(lookup, GIT_CACHE_MAX_BYTES_ENV).and_then(|raw| raw.parse::<u64>().ok()) {
        git.cache_max_bytes = bytes;
    }
    git.discover_parents = lookup(DISCOVER_PARENTS_ENV).is_some_and(|value| value != "0");
    git.history_index = text(lookup, HISTORY_INDEX_ENV).is_none_or(|value| value != "0");
}

/// `BASEMIND_DATA_HOME`'s value as a data home. A relative value is resolved against the current
/// directory here, once: the comms daemon runs in the comms dir, so a child that resolved it again
/// would use another home. An empty value is the current directory, as it was when the relative
/// value was joined as is.
fn data_root_from(value: OsString) -> DataRoot {
    DataRoot::new(absolute(value))
}

/// `value` as a path, a relative one resolved against the current directory.
fn absolute(value: OsString) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        return path;
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path,
    }
}

/// Build this process's engine from its environment. A variable with a value it cannot use, or
/// one of a removed setting, is warned about once per process.
pub fn install_from_env() -> Result<Engine, EngineError> {
    Engine::new(engine_config_from_env())
}

/// [`install_from_env`] without its warnings, for a command that runs on every prompt or tool
/// call (the status line, the hooks): a misset variable would print on each one. The warnings are
/// still spent, so a later install in the process does not print them either.
pub fn install_from_env_quietly() -> Result<Engine, EngineError> {
    tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), install_from_env)
}

/// Refuse to run an in-process entry point (`mcp::serve_in_memory`, a harness, a bench) whose
/// installed data home is not the one `BASEMIND_DATA_HOME` names. Without it, a caller that exported
/// the variable but never installed the engine writes into the platform default, the user's live
/// cache, while believing it is isolated.
pub fn ensure_env_data_home_installed() -> Result<(), String> {
    check_data_home_installed(std::env::var_os(DATA_HOME_ENV), &DataRoot::current())
}

fn check_data_home_installed(value: Option<OsString>, installed: &DataRoot) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    let named = data_root_from(value);
    if installed.same_home(&named) {
        Ok(())
    } else {
        Err(format!(
            "{DATA_HOME_ENV} names {} but this process uses the data home {}; install the engine \
             first (basemind::cli::env_config::install_from_env)",
            named.path().display(),
            installed.path().display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::daemon_lock::MAX_LIVE_DAEMONS;

    fn config_of(vars: &[(&str, &str)]) -> EngineConfig {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(name, value)| ((*name).to_string(), OsString::from(value)))
            .collect();
        engine_config_from(|name| vars.get(name).cloned())
    }

    #[test]
    fn the_data_home_variable_sets_an_absolute_data_root() {
        // An absolute path is spelled differently on Windows. ~keep
        #[cfg(unix)]
        assert_eq!(
            config_of(&[(DATA_HOME_ENV, "/srv/basemind")]).data_root,
            DataRoot::new("/srv/basemind")
        );
        let cwd = std::env::current_dir().expect("cwd");
        assert_eq!(
            config_of(&[(DATA_HOME_ENV, "bm/data")]).data_root,
            DataRoot::new(cwd.join("bm/data")),
            "a relative value is resolved against the current directory once"
        );
        let empty = config_of(&[(DATA_HOME_ENV, "")]).data_root;
        assert!(empty.path().is_absolute(), "{empty:?}");
        assert!(
            empty.same_home(&DataRoot::new(&cwd)),
            "an empty value is the current directory"
        );
        assert_eq!(config_of(&[]).data_root, DataRoot::platform_default());
    }

    #[test]
    fn an_entry_point_refuses_a_data_home_it_did_not_install() {
        let home = tempfile::tempdir().expect("tempdir");
        let installed = DataRoot::new(home.path());
        assert_eq!(check_data_home_installed(None, &installed), Ok(()));
        assert_eq!(
            check_data_home_installed(Some(home.path().as_os_str().to_owned()), &installed),
            Ok(())
        );
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let refused = check_data_home_installed(Some(elsewhere.path().as_os_str().to_owned()), &installed);
        assert!(refused.is_err_and(|message| message.contains("install_from_env")));
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
    fn the_parse_timeout_variable_takes_whole_milliseconds() {
        let timeout = |vars: &[(&str, &str)]| config_of(vars).parse_timeout;
        assert_eq!(timeout(&[]), crate::lang::DEFAULT_PARSE_TIMEOUT);
        assert_eq!(timeout(&[(PARSE_TIMEOUT_MS_ENV, "250")]), Duration::from_millis(250));
        assert_eq!(
            timeout(&[(PARSE_TIMEOUT_MS_ENV, " 250")]),
            crate::lang::DEFAULT_PARSE_TIMEOUT
        );
        assert_eq!(
            timeout(&[(PARSE_TIMEOUT_MS_ENV, "soon")]),
            crate::lang::DEFAULT_PARSE_TIMEOUT
        );
    }

    #[test]
    fn the_grammar_offline_variable_is_on_unless_empty_or_zero() {
        let offline = |vars: &[(&str, &str)]| config_of(vars).grammars.offline;
        assert!(!offline(&[]));
        assert!(!offline(&[(GRAMMAR_OFFLINE_ENV, "")]));
        assert!(!offline(&[(GRAMMAR_OFFLINE_ENV, "0")]));
        assert!(offline(&[(GRAMMAR_OFFLINE_ENV, "1")]));
        assert!(offline(&[(GRAMMAR_OFFLINE_ENV, "false")]));
    }

    #[test]
    fn the_cache_and_disk_variables_reach_the_gc_policy() {
        let config = config_of(&[
            (CACHE_BUDGET_ENV, "512"),
            (CACHE_GRACE_ENV, "30"),
            (BLOB_GC_GRACE_ENV, "0"),
            (MIN_FREE_ENV, "64"),
            (MAX_LIVE_DAEMONS_ENV, "3"),
        ]);
        assert_eq!(config.gc.budget_bytes, Some(512 << 20));
        assert_eq!(config.gc.grace, Duration::from_secs(30 * 60));
        assert_eq!(config.gc.blob_grace, Duration::ZERO);
        assert_eq!(config.min_free_bytes, 64 << 20);
        assert_eq!(config.max_live_daemons, 3);

        let defaults = config_of(&[]);
        assert_eq!(defaults, EngineConfig::default());
        assert_eq!(config_of(&[(CACHE_BUDGET_ENV, "0")]).gc.budget_bytes, None);
        assert_eq!(
            config_of(&[(MAX_LIVE_DAEMONS_ENV, "0")]).max_live_daemons,
            MAX_LIVE_DAEMONS
        );
    }

    #[test]
    fn the_git_index_and_grep_variables_reach_the_config() {
        let config = config_of(&[
            (GIT_DELTA_CACHE_BYTES_ENV, " 0 "),
            (BLAME_MAX_BYTES_ENV, "4096"),
            (BLAME_MAX_LINES_ENV, "10"),
            (GIT_CACHE_MAX_BYTES_ENV, "0"),
            (DISCOVER_PARENTS_ENV, "1"),
            (HISTORY_INDEX_ENV, "0"),
            (INDEX_CACHE_BYTES_ENV, "1048576"),
            (GREP_BLOOM_ENV, "0"),
        ]);
        assert_eq!(config.git.delta_cache_bytes, 0);
        assert_eq!(config.git.blame_max_bytes, 4096);
        assert_eq!(config.git.blame_max_lines, 10);
        assert_eq!(config.git.cache_max_bytes, 0);
        assert!(config.git.discover_parents);
        assert!(!config.git.history_index);
        assert_eq!(config.index_cache_bytes, Some(1 << 20));
        assert!(!config.grep_bloom);

        let defaults = config_of(&[
            (DISCOVER_PARENTS_ENV, "0"),
            (HISTORY_INDEX_ENV, ""),
            (GREP_BLOOM_ENV, "1"),
        ]);
        assert!(!defaults.git.discover_parents);
        assert!(defaults.git.history_index, "only `0` turns the history index off");
        assert!(defaults.grep_bloom);
        assert_eq!(
            config_of(&[(BLAME_MAX_BYTES_ENV, " 4096")]).git.blame_max_bytes,
            1 << 20
        );
    }

    #[test]
    fn the_daemon_cap_variables_reach_the_daemon_caps() {
        use crate::config::daemon::{MAX_FOOTPRINT_MB_ENV, MAX_SCAN_THREADS_ENV, MIN_DEBOUNCE_MS_ENV};
        let caps = config_of(&[
            (MAX_SCAN_THREADS_ENV, " 12 "),
            (MAX_FOOTPRINT_MB_ENV, "0"),
            (MIN_DEBOUNCE_MS_ENV, "500"),
        ])
        .daemon_caps;
        let defaults = DaemonCaps::default();
        assert_eq!(caps.scan_threads, 12);
        assert_eq!(caps.footprint_mb, defaults.footprint_mb, "zero keeps the default");
        assert_eq!(caps.min_debounce_ms, 500);
        assert_eq!(caps.embed_threads, defaults.embed_threads);
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
