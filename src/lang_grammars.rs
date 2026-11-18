//! The tree-sitter-language-pack grammar cache: where it lives, the one-shot bootstrap of the
//! override languages, extra preloads, and cache listing and cleaning.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::lang::{LangError, LangId, OVERRIDE_LANGUAGES};

/// Whether grammars may not be downloaded ([`crate::config::engine::GrammarPolicy::offline`]).
fn grammars_offline() -> bool {
    crate::config::engine::with(|config| config.grammars.offline)
}

/// Point the language pack at this process's grammar cache, once, before its first use: the
/// configured [`crate::config::engine::GrammarPolicy::cache_dir`], else
/// [`crate::data_root::DataRoot::grammars_dir`]. Every entry into the pack that can load or
/// download a grammar calls it.
pub(crate) fn configure_grammar_dir() {
    static CONFIGURED: OnceLock<()> = OnceLock::new();
    CONFIGURED.get_or_init(|| {
        let dir = crate::config::engine::with(|config| {
            config
                .grammars
                .cache_dir
                .clone()
                .unwrap_or_else(|| config.data_root.grammars_dir())
        });
        let pack = tree_sitter_language_pack::PackConfig {
            cache_dir: Some(dir.clone()),
            ..Default::default()
        };
        if let Err(error) = tree_sitter_language_pack::configure(&pack) {
            tracing::warn!(dir = %dir.display(), %error, "grammar cache directory not applied");
        }
    });
}

/// Result of the one-shot grammar bootstrap.
#[derive(Debug, Clone)]
pub struct BootstrapSummary {
    /// Languages that were already on disk before this run.
    pub already_cached: Vec<String>,
    /// Languages we just downloaded.
    pub downloaded: Vec<String>,
    /// tslp cache directory (where grammar `.so/.dylib`s live).
    pub cache_dir: Option<PathBuf>,
}

impl BootstrapSummary {
    pub fn did_download(&self) -> bool {
        !self.downloaded.is_empty()
    }
}

/// OnceLock holding the bootstrap outcome. `Arc` so callers can inspect without re-running.
static GRAMMAR_BOOTSTRAP: OnceLock<Result<Arc<BootstrapSummary>, Arc<LangError>>> = OnceLock::new();

/// Ensure all `OVERRIDE_LANGUAGES` grammars are present in the tslp cache, downloading any
/// missing ones. Idempotent across the process — runs at most once.
///
/// Only the override-supported set is pre-warmed; dynamic-path languages are pulled on first
/// use of a file with that extension. Keeps cold-start small while still guaranteeing the
/// common cases parse instantly.
///
/// Uses tslp's `prefetch` (1.12.0+): one pass that downloads + loads the missing grammars and
/// probes real on-disk loadability. This replaces the old `DownloadManager::ensure_languages`
/// workaround for the pre-1.12 `download()` bug, where in-memory registry membership
/// short-circuited the actual download (returned Ok with no disk side-effect).
pub fn ensure_grammars() -> Result<Arc<BootstrapSummary>, Arc<LangError>> {
    configure_grammar_dir();
    GRAMMAR_BOOTSTRAP
        .get_or_init(|| {
            let cache_dir_str = tree_sitter_language_pack::cache_dir()
                .map_err(|e| Arc::new(LangError::Pack(format!("resolve cache dir: {e}"))))?;
            let cache_dir = PathBuf::from(&cache_dir_str);

            let installed = tree_sitter_language_pack::downloaded_languages();
            let mut already_cached: Vec<String> = Vec::new();
            let mut missing: Vec<&'static str> = Vec::new();
            for &name in OVERRIDE_LANGUAGES {
                if installed.iter().any(|n| n == name) {
                    already_cached.push(name.to_string());
                } else {
                    missing.push(name);
                }
            }
            if !missing.is_empty() {
                if grammars_offline() {
                    return Err(Arc::new(LangError::Download(format!(
                        "offline mode: missing grammars {missing:?} and \
                         BASEMIND_GRAMMAR_OFFLINE is set",
                    ))));
                }
                tree_sitter_language_pack::prefetch(&missing)
                    .map_err(|e| Arc::new(LangError::Download(format!("{e}"))))?;
            }
            Ok(Arc::new(BootstrapSummary {
                already_cached,
                downloaded: missing.into_iter().map(str::to_string).collect(),
                cache_dir: Some(cache_dir),
            }))
        })
        .clone()
}

/// Fetch `names` (configured via `[languages.<name>] preload = true`) that are not yet in the tslp
/// cache. The shipped `OVERRIDE_LANGUAGES` are [`ensure_grammars`]'s job and are skipped here.
/// Returns the names that were downloaded.
pub fn ensure_extra_grammars(names: &[LangId]) -> Result<Vec<String>, LangError> {
    configure_grammar_dir();
    let installed = tree_sitter_language_pack::downloaded_languages();
    let mut missing: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| !OVERRIDE_LANGUAGES.contains(n) && !installed.iter().any(|i| i == n))
        .collect();
    missing.sort_unstable();
    missing.dedup();
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    if grammars_offline() {
        return Err(LangError::Download(format!(
            "offline mode: missing grammars {missing:?} and BASEMIND_GRAMMAR_OFFLINE is set",
        )));
    }
    tree_sitter_language_pack::prefetch(&missing).map_err(|e| LangError::Download(format!("{e}")))?;
    Ok(missing.into_iter().map(str::to_string).collect())
}

/// Languages currently downloaded in the tslp cache (does not hit the network).
pub fn downloaded_languages() -> Vec<String> {
    configure_grammar_dir();
    tree_sitter_language_pack::downloaded_languages()
}

/// Path to the tslp cache directory, if it can be resolved.
pub fn grammar_cache_dir() -> Option<PathBuf> {
    configure_grammar_dir();
    tree_sitter_language_pack::cache_dir().ok().map(PathBuf::from)
}

/// Clear the tslp grammar cache. Forces re-download on next use.
pub fn clean_grammar_cache() -> Result<(), LangError> {
    configure_grammar_dir();
    tree_sitter_language_pack::clean_cache().map_err(|e| LangError::Pack(format!("{e}")))
}
