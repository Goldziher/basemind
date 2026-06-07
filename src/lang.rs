use std::cell::RefCell;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use ahash::{AHashMap, AHashSet};
use thiserror::Error;
use tree_sitter::{Language, ParseOptions, Parser, Query, Tree};

/// Hard ceiling on a single tree-sitter parse. Defends against pathological inputs that
/// hang the recovery loop (e.g. multi-megabyte minified bundles with deep arrow chains).
///
/// Override per-process with `GITMIND_PARSE_TIMEOUT_MS`. The default — 5 seconds — sits
/// well above any well-formed file's parse time on the supported languages (sub-second
/// for the TypeScript compiler's biggest files) but reliably aborts known hangers.
pub const DEFAULT_PARSE_TIMEOUT: Duration = Duration::from_millis(5_000);

fn parse_timeout_from_env() -> Duration {
    std::env::var("GITMIND_PARSE_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_PARSE_TIMEOUT)
}

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
/// `LangId` is the tree-sitter-language-pack identifier (e.g. `"rust"`, `"cpp"`, `"ruby"`),
/// always sourced from TSLP's static registry. Any string handed to `with_parser` / `get_query`
/// must come from [`detect`] or [`intern`] so the lifetime guarantee holds and TSLP can resolve it.
pub type LangId = &'static str;

/// Languages we ship hand-written `.scm` query overrides for. Anything outside this set falls
/// back to TSLP's vendored `tags.scm` (when wired) and produces best-effort extraction.
///
/// Order is the bootstrap download order — keep `rust` first so the most common cold-start case
/// stays fast.
pub const OVERRIDE_LANGUAGES: &[LangId] =
    &["rust", "python", "typescript", "tsx", "javascript", "go"];

/// Back-compat alias used by `gitmind lang install` and tests that pre-warm the cache.
pub const SUPPORTED_LANGUAGES: &[LangId] = OVERRIDE_LANGUAGES;

/// Static map of override `(LangId, .scm source)` pairs. Tail of the lookup chain in
/// [`get_query`]. Adding a language here means dropping a new file in `src/queries/<lang>.scm`
/// using the same `;; section: <name>` convention.
fn override_query_source(lang: LangId) -> Option<&'static str> {
    Some(match lang {
        "rust" => include_str!("queries/rust.scm"),
        "python" => include_str!("queries/python.scm"),
        "typescript" => include_str!("queries/typescript.scm"),
        "tsx" => include_str!("queries/tsx.scm"),
        "javascript" => include_str!("queries/javascript.scm"),
        "go" => include_str!("queries/go.scm"),
        _ => return None,
    })
}

/// Whether gitmind ships a hand-written override `.scm` file for this language.
pub fn has_override(lang: LangId) -> bool {
    override_query_source(lang).is_some()
}

/// Intern a (possibly non-static) language name into the static `LangId` form.
///
/// Used by code paths that load a language tag out of persisted state (`FileEntry.language`,
/// `FileMapL1.language`) and need to feed it back into the parser / query pool. Returns
/// `Some` only when the name resolves through TSLP — unknown strings stay `None` so callers
/// can fail loud instead of leaking arbitrary input.
///
/// Interning is monotonic: each new name is leaked once via `Box::leak` and cached. Cap is
/// bounded by the size of TSLP's registry (~306 grammars × ~10 bytes), well under the cost
/// of a single open file.
pub fn intern(name: &str) -> Option<LangId> {
    // Hot path: known override names are static literals — return them without touching the
    // interner lock. Cheap branch that absorbs 99% of indexed-file lookups.
    for &lid in OVERRIDE_LANGUAGES {
        if lid == name {
            return Some(lid);
        }
    }
    // Already interned? Fast read path.
    let lock = INTERNED.get_or_init(|| RwLock::new(AHashSet::new()));
    if let Some(&existing) = lock
        .read()
        .expect("intern pool poisoned")
        .iter()
        .find(|s| **s == name)
    {
        return Some(existing);
    }
    // Cold path: validate against TSLP's registry before leaking the bytes. Unknown names
    // should not pin memory.
    if !tree_sitter_language_pack::has_language(name) {
        return None;
    }
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    lock.write().expect("intern pool poisoned").insert(leaked);
    Some(leaked)
}

static INTERNED: OnceLock<RwLock<AHashSet<&'static str>>> = OnceLock::new();

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

/// Ensure all `OVERRIDE_LANGUAGES` grammars are present in the tslp cache, downloading any
/// missing ones. Idempotent across the process — runs at most once.
///
/// Only the override-supported set is pre-warmed; dynamic-path languages are pulled on first
/// use of a file with that extension. Keeps cold-start small while still guaranteeing the
/// common cases parse instantly.
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
            for &name in OVERRIDE_LANGUAGES {
                if installed.iter().any(|n| n == name) {
                    already_cached.push(name.to_string());
                } else {
                    missing.push(name);
                }
            }
            if !missing.is_empty() {
                // Offline mode: don't reach the network. If grammars are missing, surface a
                // clean typed error so MCP clients / CLI users see a useful message instead of
                // silent empty parses. Set `GITMIND_GRAMMAR_OFFLINE=1` to opt in (e.g. CI
                // environments where the cache is pre-warmed and outbound traffic is blocked).
                if std::env::var("GITMIND_GRAMMAR_OFFLINE").is_ok_and(|v| v != "0" && !v.is_empty())
                {
                    return Err(Arc::new(LangError::Download(format!(
                        "offline mode: missing grammars {missing:?} and \
                         GITMIND_GRAMMAR_OFFLINE is set",
                    ))));
                }
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

/// Detect the language for a path. Returns the TSLP pack name (a `'static` slice) for any
/// extension TSLP can resolve — across all 306 bundled grammars. Returns `None` for unknown
/// extensions; the scanner skips those files entirely.
pub fn detect(path: &Path) -> Option<LangId> {
    tree_sitter_language_pack::detect_language(path.to_str()?)
}

/// Fetch the underlying tree-sitter Language for a given `LangId`.
pub fn language(lang: LangId) -> Result<Language, LangError> {
    tree_sitter_language_pack::get_language(lang).map_err(|e| LangError::Pack(format!("{e}")))
}

// ─── Parser pool ──────────────────────────────────────────────────────────────
//
// Parser is !Sync and stateful — one per thread per language, kept hot in TLS.

thread_local! {
    static PARSERS: RefCell<AHashMap<LangId, Parser>> = RefCell::new(AHashMap::new());
}

/// Run a closure with a per-thread Parser for the given language.
/// The parser is reused across calls on the same thread.
pub fn with_parser<F, R>(lang: LangId, f: F) -> Result<R, LangError>
where
    F: FnOnce(&mut Parser) -> R,
{
    PARSERS.with(|cell| {
        let mut map = cell.borrow_mut();
        if !map.contains_key(&lang) {
            let mut p = Parser::new();
            let ts_lang = language(lang)?;
            p.set_language(&ts_lang)
                .map_err(|_| LangError::ParserSetLanguage(lang.to_string()))?;
            map.insert(lang, p);
        }
        Ok(f(map.get_mut(&lang).expect("just inserted")))
    })
}

/// Outcome of a single bounded parse.
#[derive(Debug)]
pub enum ParseOutcome {
    Ok(Tree),
    /// Parser returned `None` for a reason other than timeout (rare — typically a malformed
    /// input the grammar can't even start on).
    Failed,
    /// Progress callback aborted because `timeout` elapsed.
    TimedOut,
}

/// Run `parser.parse_with_options` with a progress callback that aborts after `timeout`.
///
/// tree-sitter 0.26 removed the C-side `ts_parser_set_timeout_micros` shortcut in favor of
/// progress-callback-driven cancellation — this helper reinstates the ergonomics. Uses a
/// monotonic clock so it's robust against wall-clock jumps.
pub fn parse_timed(parser: &mut Parser, source: &[u8], timeout: Duration) -> ParseOutcome {
    let started = Instant::now();
    let mut timed_out = false;
    let len = source.len();
    let mut input = |i: usize, _| -> &[u8] { if i < len { &source[i..] } else { &[] } };
    let mut progress = |_state: &tree_sitter::ParseState| -> ControlFlow<()> {
        if started.elapsed() > timeout {
            timed_out = true;
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let opts = ParseOptions::new().progress_callback(&mut progress);
    let tree = parser.parse_with_options(&mut input, None, Some(opts));
    match (tree, timed_out) {
        (Some(t), _) => ParseOutcome::Ok(t),
        (None, true) => ParseOutcome::TimedOut,
        (None, false) => ParseOutcome::Failed,
    }
}

/// Convenience: `parse_timed` with the env-configurable default timeout.
pub fn parse_with_default_timeout(parser: &mut Parser, source: &[u8]) -> ParseOutcome {
    parse_timed(parser, source, parse_timeout_from_env())
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

/// Two-state query cache value: `Some` when a query was found and compiled; `None` when the
/// language has no override section + no TSLP fallback for this kind. The `None` is cached
/// to avoid re-doing the negative lookup for every file in that language.
type CachedQuery = Option<Arc<Query>>;
type QueryMap = AHashMap<(LangId, QueryKind), CachedQuery>;
static QUERIES: OnceLock<RwLock<QueryMap>> = OnceLock::new();

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

/// Look up a `(lang, kind)` query, returning `Ok(Some(arc))` when one exists,
/// `Ok(None)` when neither the override file nor the TSLP fallback provides this section,
/// and `Err` only on a compile error in source we do have.
///
/// Lookup chain:
/// 1. Local override — `src/queries/<lang>.scm` `;; section: <kind>`.
/// 2. TSLP `tags.scm` — for Symbols/Calls only, gated on the upstream `get_tags_query`
///    accessor (not yet wired; placeholder branch returns `None`).
/// 3. None — file is still detected and indexed, but symbol/import/call extraction yields
///    empty vectors for this language.
pub fn try_get_query(lang: LangId, kind: QueryKind) -> Result<CachedQuery, LangError> {
    let lock = QUERIES.get_or_init(|| RwLock::new(AHashMap::new()));
    if let Some(slot) = lock.read().expect("query pool poisoned").get(&(lang, kind)) {
        return Ok(slot.as_ref().map(Arc::clone));
    }

    let source = override_query_source(lang).and_then(|raw| extract_section(raw, kind.name()));
    // Future: when TSLP exposes `get_tags_query`, plug it in here for Symbols/Calls under
    // languages without an override. The adapter rewrites @definition.*/@reference.call
    // captures into our @symbol.*/@call.* shape before compiling.

    let cached = match source {
        Some(src) => {
            let ts_lang = language(lang)?;
            let query = Query::new(&ts_lang, &src).map_err(|e| LangError::QueryCompile {
                lang,
                kind: kind.name(),
                msg: format!("{e}"),
            })?;
            Some(Arc::new(query))
        }
        None => None,
    };

    lock.write()
        .expect("query pool poisoned")
        .insert((lang, kind), cached.as_ref().map(Arc::clone));
    Ok(cached)
}

/// Strict variant of [`try_get_query`] for callers that treat missing sections as errors.
/// Prefer `try_get_query` in new code so unsupported languages degrade gracefully.
pub fn get_query(lang: LangId, kind: QueryKind) -> Result<Arc<Query>, LangError> {
    try_get_query(lang, kind)?.ok_or_else(|| LangError::QueryCompile {
        lang,
        kind: kind.name(),
        msg: format!("no override or TSLP fallback for {}/{}", lang, kind.name()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_known_extensions() {
        assert_eq!(detect(Path::new("foo.rs")), Some("rust"));
        assert_eq!(detect(Path::new("foo.py")), Some("python"));
        assert_eq!(detect(Path::new("foo.go")), Some("go"));
    }

    #[test]
    fn detect_dynamic_extension_resolves() {
        // Any TSLP-registered grammar resolves through detect(); cpp is outside the override
        // set but ships in the language pack, so dynamic dispatch must produce its pack name.
        assert_eq!(detect(Path::new("foo.cpp")), Some("cpp"));
    }

    #[test]
    fn extract_section_basic() {
        let src = ";; section: a\n(foo)\n;; section: b\n(bar)\n";
        assert_eq!(extract_section(src, "a").unwrap().trim(), "(foo)");
        assert_eq!(extract_section(src, "b").unwrap().trim(), "(bar)");
        assert_eq!(extract_section(src, "c"), None);
    }

    #[test]
    fn has_override_for_each_supported() {
        for &name in OVERRIDE_LANGUAGES {
            assert!(has_override(name), "missing override source for {name}");
        }
    }

    #[test]
    fn intern_known_overrides_returns_static() {
        let owned = "rust".to_string();
        let id = intern(&owned).expect("rust must intern");
        assert!(std::ptr::eq(id, "rust"));
    }

    #[test]
    fn intern_unknown_returns_none() {
        assert!(intern("this-is-not-a-real-grammar-name").is_none());
    }

    #[test]
    fn try_get_query_returns_none_for_unsupported_lang() {
        // C++ has no override and the TSLP-fallback branch is not yet wired, so the lookup
        // returns `None`. When `get_tags_query` lands upstream this becomes `Some(...)`.
        let res = try_get_query("cpp", QueryKind::Symbols).expect("query lookup must not error");
        assert!(res.is_none());
    }
}
