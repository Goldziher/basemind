use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use ahash::AHashMap;
use thiserror::Error;
use tree_sitter::{Language, Parser, Query};

#[derive(Debug, Error)]
pub enum LangError {
    #[error("language pack error: {0}")]
    Pack(String),
    #[error("grammar download failed: {0}")]
    Download(String),
    #[error("query compile error for {lang}/{kind}: {msg}")]
    QueryCompile {
        lang: &'static str,
        kind: &'static str,
        msg: String,
    },
    #[error("failed to set language {0} on parser")]
    ParserSetLanguage(String),
}

/// Stable language identifier used as the key everywhere (parser pool, query pool, FileMap.language).
///
/// This enum is the boundary between "tree-sitter-language-pack can parse this" and "gitmind
/// has a hand-written query for this". Adding a language requires a new `.scm` and a new variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
}

impl Lang {
    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Go => "go",
        }
    }

    pub fn all() -> &'static [Lang] {
        &[
            Lang::Rust,
            Lang::Python,
            Lang::TypeScript,
            Lang::Tsx,
            Lang::JavaScript,
            Lang::Go,
        ]
    }

    /// Map a tree-sitter-language-pack identifier back to our enum.
    pub fn from_pack_name(name: &str) -> Option<Self> {
        Some(match name {
            "rust" => Lang::Rust,
            "python" => Lang::Python,
            "typescript" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "javascript" => Lang::JavaScript,
            "go" => Lang::Go,
            _ => return None,
        })
    }
}

/// Languages we ship queries for — used as the bootstrap download set.
pub const SUPPORTED_LANGUAGES: &[&str] =
    &["rust", "python", "typescript", "tsx", "javascript", "go"];

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

/// Parse the tslp version out of its `cache_dir()` (`.../v<version>/libs`).
/// Returns `None` if the path is shaped unexpectedly — caller falls back gracefully.
fn tslp_version_from_cache_dir(p: &Path) -> Option<String> {
    let parent = p.parent()?;
    let leaf = parent.file_name()?.to_str()?;
    leaf.strip_prefix('v').map(str::to_string)
}

/// Ensure all `SUPPORTED_LANGUAGES` grammars are present in the tslp cache, downloading any
/// missing ones. Idempotent across the process — runs at most once.
///
/// Uses `DownloadManager::ensure_languages` directly rather than the top-level
/// `tree_sitter_language_pack::download()` because the latter has a bug in 1.9.0-rc.22 where
/// in-memory REGISTRY membership short-circuits the actual download (returns Ok with no
/// disk side-effect).
pub fn ensure_grammars() -> Result<Arc<BootstrapSummary>, Arc<LangError>> {
    GRAMMAR_BOOTSTRAP
        .get_or_init(|| {
            let cache_dir_str = tree_sitter_language_pack::cache_dir()
                .map_err(|e| Arc::new(LangError::Pack(format!("resolve cache dir: {e}"))))?;
            let cache_dir = PathBuf::from(&cache_dir_str);
            let version = tslp_version_from_cache_dir(&cache_dir).ok_or_else(|| {
                Arc::new(LangError::Pack(format!(
                    "could not parse tslp version out of {cache_dir_str:?}"
                )))
            })?;

            let dm = tree_sitter_language_pack::DownloadManager::with_cache_dir(
                &version,
                cache_dir.clone(),
            );

            let installed: Vec<String> = dm.installed_languages();
            let mut already_cached: Vec<String> = Vec::new();
            let mut missing: Vec<&'static str> = Vec::new();
            for &name in SUPPORTED_LANGUAGES {
                if installed.iter().any(|n| n == name) {
                    already_cached.push(name.to_string());
                } else {
                    missing.push(name);
                }
            }
            if !missing.is_empty() {
                dm.ensure_languages(&missing)
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

/// Languages currently downloaded in the tslp cache (does not hit the network).
pub fn downloaded_languages() -> Vec<String> {
    // tslp's `downloaded_languages()` reads via a DownloadManager keyed by its own
    // CARGO_PKG_VERSION, which matches the cache layout — same source-of-truth either way.
    tree_sitter_language_pack::downloaded_languages()
}

/// Path to the tslp cache directory, if it can be resolved.
pub fn grammar_cache_dir() -> Option<PathBuf> {
    tree_sitter_language_pack::cache_dir()
        .ok()
        .map(PathBuf::from)
}

/// Clear the tslp grammar cache. Forces re-download on next use.
pub fn clean_grammar_cache() -> Result<(), LangError> {
    tree_sitter_language_pack::clean_cache().map_err(|e| LangError::Pack(format!("{e}")))
}

/// Detect the language for a path. Returns None for files we don't have queries for yet.
pub fn detect(path: &Path) -> Option<Lang> {
    let pack_name = tree_sitter_language_pack::detect_language(path.to_str()?)?;
    Lang::from_pack_name(pack_name)
}

/// Fetch the underlying tree-sitter Language for a given Lang.
pub fn language(lang: Lang) -> Result<Language, LangError> {
    tree_sitter_language_pack::get_language(lang.name())
        .map_err(|e| LangError::Pack(format!("{e}")))
}

// ─── Parser pool ──────────────────────────────────────────────────────────────
//
// Parser is !Sync and stateful — one per thread per language, kept hot in TLS.

thread_local! {
    static PARSERS: RefCell<AHashMap<Lang, Parser>> = RefCell::new(AHashMap::new());
}

/// Run a closure with a per-thread Parser for the given language.
/// The parser is reused across calls on the same thread.
pub fn with_parser<F, R>(lang: Lang, f: F) -> Result<R, LangError>
where
    F: FnOnce(&mut Parser) -> R,
{
    PARSERS.with(|cell| {
        let mut map = cell.borrow_mut();
        if !map.contains_key(&lang) {
            let mut p = Parser::new();
            let ts_lang = language(lang)?;
            p.set_language(&ts_lang)
                .map_err(|_| LangError::ParserSetLanguage(lang.name().to_string()))?;
            map.insert(lang, p);
        }
        Ok(f(map.get_mut(&lang).expect("just inserted")))
    })
}

// ─── Query pool ───────────────────────────────────────────────────────────────
//
// Query is Send + Sync and not Clone; one Arc<Query> per (lang, kind) globally.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryKind {
    /// Captures: @symbol.name, @symbol.kind, @symbol.range, @symbol.signature
    Symbols,
    /// Captures: @import.module, @import.alias, @import.range
    Imports,
    /// Captures: @call.callee, @call.range  (L2)
    Calls,
    /// Captures: @doc.text, @doc.target  (L2)
    Docs,
}

impl QueryKind {
    pub fn name(self) -> &'static str {
        match self {
            QueryKind::Symbols => "symbols",
            QueryKind::Imports => "imports",
            QueryKind::Calls => "calls",
            QueryKind::Docs => "docs",
        }
    }
}

static QUERIES: OnceLock<RwLock<AHashMap<(Lang, QueryKind), Arc<Query>>>> = OnceLock::new();

fn query_source(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => include_str!("queries/rust.scm"),
        Lang::Python => include_str!("queries/python.scm"),
        Lang::TypeScript => include_str!("queries/typescript.scm"),
        Lang::Tsx => include_str!("queries/typescript.scm"),
        Lang::JavaScript => include_str!("queries/javascript.scm"),
        Lang::Go => include_str!("queries/go.scm"),
    }
}

/// Extract a single named query (S-expression `;; @section name`) from the .scm source.
///
/// Convention: each .scm file is divided into sections marked by `;; section: <name>` lines.
/// Sections we look for: `symbols`, `imports`, `calls`, `docs`.
fn extract_section(source: &str, name: &str) -> Option<String> {
    let marker_open = format!(";; section: {name}");
    let mut out = String::new();
    let mut in_section = false;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with(";; section:") {
            in_section = trimmed.starts_with(&marker_open);
            continue;
        }
        if in_section {
            out.push_str(line);
            out.push('\n');
        }
    }
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

pub fn get_query(lang: Lang, kind: QueryKind) -> Result<Arc<Query>, LangError> {
    let lock = QUERIES.get_or_init(|| RwLock::new(AHashMap::new()));
    if let Some(q) = lock.read().expect("query pool poisoned").get(&(lang, kind)) {
        return Ok(Arc::clone(q));
    }
    let source = extract_section(query_source(lang), kind.name()).ok_or_else(|| {
        LangError::QueryCompile {
            lang: lang.name(),
            kind: kind.name(),
            msg: format!("no `;; section: {}` in {}.scm", kind.name(), lang.name()),
        }
    })?;
    let ts_lang = language(lang)?;
    let query = Query::new(&ts_lang, &source).map_err(|e| LangError::QueryCompile {
        lang: lang.name(),
        kind: kind.name(),
        msg: format!("{e}"),
    })?;
    let arc = Arc::new(query);
    lock.write()
        .expect("query pool poisoned")
        .insert((lang, kind), Arc::clone(&arc));
    Ok(arc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_known_extensions() {
        assert_eq!(detect(Path::new("foo.rs")), Some(Lang::Rust));
        assert_eq!(detect(Path::new("foo.py")), Some(Lang::Python));
        assert_eq!(detect(Path::new("foo.go")), Some(Lang::Go));
    }

    #[test]
    fn extract_section_basic() {
        let src = ";; section: a\n(foo)\n;; section: b\n(bar)\n";
        assert_eq!(extract_section(src, "a").unwrap().trim(), "(foo)");
        assert_eq!(extract_section(src, "b").unwrap().trim(), "(bar)");
        assert_eq!(extract_section(src, "c"), None);
    }

    #[test]
    fn supported_set_matches_enum() {
        let from_enum: Vec<&str> = Lang::all().iter().map(|l| l.name()).collect();
        assert_eq!(from_enum, SUPPORTED_LANGUAGES);
    }
}
