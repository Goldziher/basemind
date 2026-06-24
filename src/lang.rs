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
/// Override per-process with `BASEMIND_PARSE_TIMEOUT_MS`. The default — 5 seconds — sits
/// well above any well-formed file's parse time on the supported languages (sub-second
/// for the TypeScript compiler's biggest files) but reliably aborts known hangers.
pub const DEFAULT_PARSE_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Cache of the resolved parse timeout. Initialized once per process on first call to
/// `parse_with_default_timeout`; eliminates a `std::env::var` syscall on every file parse
/// (called ×2 per file when eager L2 is on — once for L1, once for L2).
static PARSE_TIMEOUT: OnceLock<Duration> = OnceLock::new();

fn parse_timeout_from_env() -> Duration {
    *PARSE_TIMEOUT.get_or_init(|| {
        std::env::var("BASEMIND_PARSE_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_PARSE_TIMEOUT)
    })
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

/// Back-compat alias used by `basemind lang install` and tests that pre-warm the cache.
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

/// Whether basemind ships a hand-written override `.scm` file for this language.
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
    // Already interned? Fast read path — `AHashSet::get` runs a single hash + probe instead
    // of a linear scan. `<&'static str as Borrow<str>>` lets us look up by `&str` without
    // allocating; the slot returned is the cached `&'static str` we hand back.
    let lock = INTERNED.get_or_init(|| RwLock::new(AHashSet::new()));
    if let Some(&existing) = lock.read().expect("intern pool poisoned").get(name) {
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
                // silent empty parses. Set `BASEMIND_GRAMMAR_OFFLINE=1` to opt in (e.g. CI
                // environments where the cache is pre-warmed and outbound traffic is blocked).
                if std::env::var("BASEMIND_GRAMMAR_OFFLINE")
                    .is_ok_and(|v| v != "0" && !v.is_empty())
                {
                    return Err(Arc::new(LangError::Download(format!(
                        "offline mode: missing grammars {missing:?} and \
                         BASEMIND_GRAMMAR_OFFLINE is set",
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
    /// Captures: @impl.trait_name, @impl.implementor
    ///
    /// Populated from the `;; section: implementations` block in hand-written `.scm` overrides,
    /// or from `@reference.implementation` captures in TSLP `tags.scm` files adapted by
    /// [`adapt_tslp_tags`]. One `(trait_name, implementor)` pair per inheritance edge.
    Implementations,
}

impl QueryKind {
    pub fn name(self) -> &'static str {
        match self {
            QueryKind::Symbols => "symbols",
            QueryKind::Imports => "imports",
            QueryKind::Calls => "calls",
            QueryKind::Docs => "docs",
            QueryKind::Implementations => "implementations",
        }
    }
}

/// Two-state query cache value: `Some` when a query was found and compiled; `None` when the
/// language has no override section + no TSLP fallback for this kind. The `None` is cached
/// to avoid re-doing the negative lookup for every file in that language.
type CachedQuery = Option<Arc<Query>>;
type QueryMap = AHashMap<(LangId, QueryKind), CachedQuery>;
static QUERIES: OnceLock<RwLock<QueryMap>> = OnceLock::new();

/// Per-language cache for the combined L1 query (symbols + imports + implementations in one
/// `Arc<Query>`). Keyed by `LangId` alone — cheaper to look up than the per-`(lang, kind)` map
/// and avoids three separate lock acquisitions per file.
type CombinedL1Map = AHashMap<LangId, CachedQuery>;
static COMBINED_L1_QUERIES: OnceLock<RwLock<CombinedL1Map>> = OnceLock::new();

/// Per-capture classification for the combined L1 query. Lets `run_combined` dispatch by
/// capture index (a `Copy` array lookup) instead of `str::starts_with` on every query match.
///
/// Derived once when the query is compiled and cached for process lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureClass {
    Symbol,
    Import,
    Impl,
    Other,
}

/// A compiled combined-L1 query paired with its per-capture classification table.
///
/// The `classes` slice has exactly `query.capture_names().len()` entries; index `i` is the
/// class of capture index `i`. Callers read `classes[first_cap.index as usize]` to dispatch
/// without any string comparison in the hot loop.
pub struct ClassifiedQuery {
    pub query: Arc<Query>,
    pub classes: Box<[CaptureClass]>,
}

type ClassifiedL1Map = AHashMap<LangId, Option<Arc<ClassifiedQuery>>>;
static CLASSIFIED_COMBINED_L1: OnceLock<RwLock<ClassifiedL1Map>> = OnceLock::new();

/// Extract a single named query (S-expression `;; @section name`) from the .scm source.
///
/// Convention: each .scm file is divided into sections marked by `;; section: <name>` lines.
/// Sections we look for: `symbols`, `imports`, `calls`, `docs`.
fn extract_section(source: &str, name: &str) -> Option<String> {
    // Zero-alloc section detection: strip the fixed prefix then compare the trimmed remainder
    // to `name` exactly. Avoids the `format!(";; section: {name}")` allocation on every call.
    let mut out = String::new();
    let mut in_section = false;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if let Some(section_name) = trimmed.strip_prefix(";; section:") {
            in_section = section_name.trim() == name;
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

/// Adapt an upstream TSLP `tags.scm` source into basemind's override-shaped section convention.
///
/// TSLP's `tags.scm` uses the GitHub-standard capture names `@definition.<kind>` / `@reference.call`
/// with the identifier name captured as `@name`. Basemind's extractors look for
/// `@symbol.<kind>` / `@symbol.name` (l1) and `@call.range` / `@call.callee` (l2). This walks
/// top-level S-expression patterns, classifies each by its root capture, and emits the
/// rewritten pattern into the appropriate section block.
///
/// Patterns whose root capture is neither `@definition.*`, `@reference.call`, nor
/// `@reference.implementation` (e.g. `@reference.class`, `@reference.interface`,
/// `@reference.send`, `@reference.type`) are dropped — basemind has no consumer for them.
///
/// `@reference.implementation` patterns are rewritten into the `;; section: implementations`
/// block where each `@name` capture becomes `@impl.trait_name`. The `;; section: symbols`
/// block emits a corresponding `@impl.implementor` capture for the surrounding definition
/// node when the grammar allows it — but since TSLP patterns typically expose only the
/// parent identifier node, the implementor is extracted from the match context in
/// `src/extract/l1.rs::build_implementation` by walking to the nearest named ancestor.
fn adapt_tslp_tags(source: &str) -> String {
    let mut sym_buf = String::new();
    let mut call_buf = String::new();
    let mut impl_buf = String::new();
    for pattern in split_top_level_patterns(source) {
        let kind = classify_pattern(pattern);
        match kind {
            PatternKind::Definition => sym_buf.push_str(&rewrite_pattern(pattern, kind)),
            PatternKind::ReferenceCall => call_buf.push_str(&rewrite_pattern(pattern, kind)),
            PatternKind::ReferenceImplementation => {
                impl_buf.push_str(&rewrite_pattern(pattern, kind));
            }
            PatternKind::Other => {}
        }
    }
    let mut out = String::with_capacity(sym_buf.len() + call_buf.len() + impl_buf.len() + 96);
    out.push_str(";; section: symbols\n");
    out.push_str(&sym_buf);
    out.push_str("\n;; section: calls\n");
    out.push_str(&call_buf);
    out.push_str("\n;; section: implementations\n");
    out.push_str(&impl_buf);
    out
}

#[derive(Clone, Copy)]
enum PatternKind {
    Definition,
    ReferenceCall,
    ReferenceImplementation,
    Other,
}

/// Yield each top-level S-expression pattern from the source as a `&str` slice.
///
/// Walks paren depth char-by-char, skipping `;`-to-EOL comments and `"..."` string literals
/// where parens carry no structural meaning. A "pattern" is the substring from a depth-0
/// `(` (along with any trailing `@root.capture` annotation) to the next paren that lands
/// back at depth 0. Free-standing comments and whitespace between patterns are skipped.
fn split_top_level_patterns(source: &str) -> Vec<&str> {
    let bytes = source.as_bytes();
    let mut patterns: Vec<&str> = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    let mut depth: i32 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b';' => {
                // Comment to end of line, regardless of depth.
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => {
                // Skip string literal — escapes preserved minimally.
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'(' => {
                if depth == 0 && start.is_none() {
                    start = Some(i);
                }
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    // Consume any trailing `@root.capture` annotation that belongs to the
                    // just-closed pattern, including whitespace/newlines between `)` and `@`.
                    let mut j = i;
                    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b'@' {
                        // Skip `@capture.name` token.
                        j += 1;
                        while j < bytes.len() && is_capture_ident_byte(bytes[j]) {
                            j += 1;
                        }
                        i = j;
                    }
                    if let Some(s) = start {
                        patterns.push(&source[s..i]);
                    }
                    start = None;
                }
            }
            _ => i += 1,
        }
    }
    patterns
}

fn is_capture_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'?' || b == b'!'
}

/// Find the root capture of a pattern: the rightmost `@<root>.<sub>` at the outermost
/// closing paren (or top level). Returns `Definition` if root is `definition.*`,
/// `ReferenceCall` if root is `reference.call`, `Other` otherwise.
fn classify_pattern(pattern: &str) -> PatternKind {
    // The root capture is the LAST `@...` token in the pattern. Scan from the end.
    let bytes = pattern.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        if bytes[i] == b'@' {
            // Found a `@` — read forward to extract the capture name.
            let cap_start = i + 1;
            let mut j = cap_start;
            while j < bytes.len() && is_capture_ident_byte(bytes[j]) {
                j += 1;
            }
            let cap = &pattern[cap_start..j];
            return classify_capture(cap);
        }
    }
    PatternKind::Other
}

fn classify_capture(cap: &str) -> PatternKind {
    if let Some(suffix) = cap.strip_prefix("definition.") {
        let _ = suffix;
        PatternKind::Definition
    } else if cap == "reference.call" {
        PatternKind::ReferenceCall
    } else if cap == "reference.implementation" {
        PatternKind::ReferenceImplementation
    } else {
        PatternKind::Other
    }
}

/// Rewrite a pattern's capture names from TSLP convention to basemind convention. The trailing
/// `\n` is included so consecutive patterns stay separated in the emitted section.
fn rewrite_pattern(pattern: &str, kind: PatternKind) -> String {
    let mut out = String::with_capacity(pattern.len() + 16);
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'@' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        // Found `@`. Read the capture name.
        let cap_start = i + 1;
        let mut j = cap_start;
        while j < bytes.len() && is_capture_ident_byte(bytes[j]) {
            j += 1;
        }
        let cap = &pattern[cap_start..j];
        let rewritten = rewrite_capture(cap, kind);
        out.push('@');
        out.push_str(&rewritten);
        i = j;
    }
    out.push('\n');
    out
}

fn rewrite_capture(cap: &str, kind: PatternKind) -> String {
    // `@name` is the identifier sub-capture; remap by section.
    if cap == "name" {
        return match kind {
            PatternKind::Definition => "symbol.name".to_string(),
            PatternKind::ReferenceCall => "call.callee".to_string(),
            PatternKind::ReferenceImplementation => "impl.trait_name".to_string(),
            PatternKind::Other => "name".to_string(),
        };
    }
    if let Some(suffix) = cap.strip_prefix("definition.") {
        return format!("symbol.{suffix}");
    }
    if cap == "reference.call" {
        return "call.range".to_string();
    }
    if cap == "reference.implementation" {
        return "impl.range".to_string();
    }
    // Predicates like `@cap (#match? ...)` keep their original name in the pattern they
    // came from — leave untouched.
    cap.to_string()
}

/// Per-language cache of the adapted `tags.scm` string. Populated on first lookup;
/// stays for process lifetime. Bound is the size of TSLP's registry — small.
type AdaptedTagsMap = AHashMap<LangId, Arc<str>>;
static ADAPTED_TAGS: OnceLock<RwLock<AdaptedTagsMap>> = OnceLock::new();

fn tslp_tags_adapted(lang: LangId) -> Option<Arc<str>> {
    let lock = ADAPTED_TAGS.get_or_init(|| RwLock::new(AHashMap::new()));
    if let Some(cached) = lock.read().expect("adapted tags pool poisoned").get(&lang) {
        return Some(Arc::clone(cached));
    }
    let raw = tree_sitter_language_pack::get_tags_query(lang)?;
    let adapted: Arc<str> = Arc::from(adapt_tslp_tags(raw));
    lock.write()
        .expect("adapted tags pool poisoned")
        .insert(lang, Arc::clone(&adapted));
    Some(adapted)
}

/// Look up the combined L1 query for a language, fusing the `symbols`, `imports`, and
/// `implementations` sections into a single compiled `Arc<Query>`.
///
/// One combined query lets `extract_l1` allocate one `QueryCursor` and walk the tree once
/// instead of three times. Capture namespacing (`@symbol.*`, `@import.*`, `@impl.*`) is
/// already enforced by the hand-written `.scm` overrides and `adapt_tslp_tags`, so the
/// patterns from the three sections compose without conflict.
///
/// Returns `Ok(None)` when the language has no L1 content at all (no symbols, no imports,
/// no implementations from either the override path or TSLP). Returns `Err` only when a
/// section source is present but fails to compile.
pub fn try_get_combined_l1_query(lang: LangId) -> Result<CachedQuery, LangError> {
    let lock = COMBINED_L1_QUERIES.get_or_init(|| RwLock::new(AHashMap::new()));
    if let Some(slot) = lock
        .read()
        .expect("combined L1 query pool poisoned")
        .get(&lang)
    {
        return Ok(slot.as_ref().map(Arc::clone));
    }

    // Build the concatenated source from the three L1 sections.
    let combined_src: Option<String> = if let Some(raw) = override_query_source(lang) {
        // Override path: extract the three relevant sections and concatenate.
        let sym = extract_section(raw, "symbols").unwrap_or_default();
        let imp = extract_section(raw, "imports").unwrap_or_default();
        let imp_l = extract_section(raw, "implementations").unwrap_or_default();
        let combined = format!("{sym}\n{imp}\n{imp_l}");
        if combined.trim().is_empty() {
            None
        } else {
            Some(combined)
        }
    } else {
        // TSLP fallback path: symbols + implementations come from the adapted source;
        // imports are not produced by `adapt_tslp_tags` (only Symbols/Calls/Implementations).
        tslp_tags_adapted(lang).and_then(|adapted| {
            let sym = extract_section(&adapted, "symbols").unwrap_or_default();
            let imp_l = extract_section(&adapted, "implementations").unwrap_or_default();
            let combined = format!("{sym}\n{imp_l}");
            if combined.trim().is_empty() {
                None
            } else {
                Some(combined)
            }
        })
    };

    let cached = match combined_src {
        Some(src) => {
            let ts_lang = language(lang)?;
            let query = Query::new(&ts_lang, &src).map_err(|e| LangError::QueryCompile {
                lang,
                kind: "combined_l1",
                msg: format!("{e}"),
            })?;
            Some(Arc::new(query))
        }
        None => None,
    };

    lock.write()
        .expect("combined L1 query pool poisoned")
        .insert(lang, cached.as_ref().map(Arc::clone));
    Ok(cached)
}

/// Like [`try_get_combined_l1_query`] but also returns a per-capture classification table
/// so `run_combined` can dispatch by integer index instead of `starts_with` comparisons.
///
/// The returned [`ClassifiedQuery`] has `classes[i]` set for every capture index `i` in the
/// compiled query. Cached globally per language; built at most once per process per language.
pub fn try_get_classified_combined_l1_query(
    lang: LangId,
) -> Result<Option<Arc<ClassifiedQuery>>, LangError> {
    let lock = CLASSIFIED_COMBINED_L1.get_or_init(|| RwLock::new(AHashMap::new()));
    if let Some(slot) = lock
        .read()
        .expect("classified L1 query pool poisoned")
        .get(&lang)
    {
        return Ok(slot.as_ref().map(Arc::clone));
    }

    // Build from the same combined query — reuses the compiled Arc<Query> from the sibling
    // cache when it already exists, otherwise compiles fresh. We call `try_get_combined_l1_query`
    // to avoid duplicating the source-assembly logic.
    let cached: Option<Arc<ClassifiedQuery>> = match try_get_combined_l1_query(lang)? {
        Some(query) => {
            let classes: Box<[CaptureClass]> = query
                .capture_names()
                .iter()
                .map(|name| {
                    if name.starts_with("symbol.") {
                        CaptureClass::Symbol
                    } else if name.starts_with("import.") {
                        CaptureClass::Import
                    } else if name.starts_with("impl.") {
                        CaptureClass::Impl
                    } else {
                        CaptureClass::Other
                    }
                })
                .collect();
            Some(Arc::new(ClassifiedQuery { query, classes }))
        }
        None => None,
    };

    lock.write()
        .expect("classified L1 query pool poisoned")
        .insert(lang, cached.as_ref().map(Arc::clone));
    Ok(cached)
}

/// Look up a `(lang, kind)` query, returning `Ok(Some(arc))` when one exists,
/// `Ok(None)` when neither the override file nor the TSLP fallback provides this section,
/// and `Err` only on a compile error in source we do have.
///
/// Language coverage is three concentric rings over TSLP's ~306-grammar registry:
/// - **Override ring** ([`OVERRIDE_LANGUAGES`], 6 today): hand-written
///   `src/queries/<lang>.scm` with full symbol/import/call/doc sections.
/// - **TSLP `tags.scm` ring** (~100 grammars in the published bundle — e.g. kotlin,
///   csharp, swift, cpp, scala, solidity): adapted on the fly via [`adapt_tslp_tags`].
///   Best-effort symbol/call/implementation extraction; no import/doc sections.
/// - **Detect-only ring** (the remaining grammars — JSON, YAML, TOML, …): the file
///   is detected, parsed, and listed by `list_files`, but extraction yields empty
///   vectors (see ring 3 below). This is the silent-empty-extraction case.
///
/// Lookup chain:
/// 1. Local override — `src/queries/<lang>.scm` `;; section: <kind>`.
/// 2. TSLP `tags.scm` via [`adapt_tslp_tags`] — only for the `Symbols` / `Calls` /
///    `Implementations` kinds, and only when upstream ships a vendored `tags.scm`.
/// 3. None — the file is still detected and indexed, but symbol/import/call
///    extraction yields empty vectors. Callers that need a hard signal for an
///    unsupported language should use [`get_query`], which turns this into an error;
///    `try_get_query` degrades silently by design.
pub fn try_get_query(lang: LangId, kind: QueryKind) -> Result<CachedQuery, LangError> {
    let lock = QUERIES.get_or_init(|| RwLock::new(AHashMap::new()));
    if let Some(slot) = lock.read().expect("query pool poisoned").get(&(lang, kind)) {
        return Ok(slot.as_ref().map(Arc::clone));
    }

    let source: Option<String> = if let Some(raw) = override_query_source(lang) {
        extract_section(raw, kind.name())
    } else if matches!(
        kind,
        QueryKind::Symbols | QueryKind::Calls | QueryKind::Implementations
    ) {
        tslp_tags_adapted(lang).and_then(|adapted| extract_section(&adapted, kind.name()))
    } else {
        None
    };

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
        assert_eq!(id, "rust");
        // The id is the canonical static instance: a second intern of an equal name returns
        // the SAME pointer. Comparing to the "rust" literal instead would rely on the linker
        // merging identical string literals across the test and the override table, which MSVC
        // does not guarantee (the Windows CI runner caught this).
        let again = intern("rust").expect("rust must intern");
        assert!(std::ptr::eq(id, again));
    }

    #[test]
    fn intern_unknown_returns_none() {
        assert!(intern("this-is-not-a-real-grammar-name").is_none());
    }

    #[test]
    fn try_get_query_returns_none_for_unsupported_lang() {
        // `json` ships in TSLP but has no override AND no upstream `tags.scm`, so both
        // lookup branches miss and the cache stores `None`. (Previously `cpp` — which now
        // resolves through the TSLP tags fallback; data-only formats like JSON / YAML /
        // TOML reliably ship no tags query.)
        let res = try_get_query("json", QueryKind::Symbols).expect("query lookup must not error");
        assert!(res.is_none());
    }

    #[test]
    fn adapt_tslp_tags_emits_two_sections() {
        let src = "(function_item name: (identifier) @name) @definition.function\n\
                   (call_expression function: (identifier) @name) @reference.call\n";
        let out = adapt_tslp_tags(src);
        assert!(out.contains(";; section: symbols"));
        assert!(out.contains(";; section: calls"));
        assert!(out.contains("@symbol.function"));
        assert!(out.contains("@symbol.name"));
        assert!(out.contains("@call.range"));
        assert!(out.contains("@call.callee"));
    }

    #[test]
    fn adapt_tslp_tags_routes_reference_implementation() {
        // `@reference.implementation` (rust impl_item trait, csharp base_list) must land in the
        // implementations section, not the calls section.
        let src = "(impl_item trait: (type_identifier) @name) @reference.implementation\n\
                   (call_expression function: (identifier) @name) @reference.call\n";
        let out = adapt_tslp_tags(src);
        // @reference.implementation pattern must be in the implementations section.
        assert!(out.contains(";; section: implementations"));
        assert!(out.contains("@impl.trait_name"));
        assert!(out.contains("@impl.range"));
        // Calls section must still work.
        assert!(out.contains("@call.range"));
        assert!(out.contains("@call.callee"));
        // No raw @reference.* captures should leak out.
        assert!(!out.contains("@reference.implementation"));
        assert!(!out.contains("@reference.call"));
    }

    #[test]
    fn adapt_tslp_tags_drops_reference_class() {
        // `@reference.class` (kotlin constructor invocations, csharp type refs) are generic
        // type-reference captures — not inheritance. They must be dropped entirely.
        let src = "(object_creation_expression type: (identifier) @name) @reference.class\n\
                   (call_expression function: (identifier) @name) @reference.call\n";
        let out = adapt_tslp_tags(src);
        // @reference.class has no basemind section, so the pattern must be absent.
        assert!(!out.contains("@reference.class"));
        // Calls section must still work.
        assert!(out.contains("@call.range"));
        assert!(out.contains("@call.callee"));
    }

    #[test]
    fn adapt_tslp_tags_handles_multiline_patterns() {
        let src = "(struct_item\n    name: (type_identifier) @name) @definition.class\n";
        let out = adapt_tslp_tags(src);
        assert!(out.contains("@symbol.class"));
        assert!(out.contains("@symbol.name"));
    }

    #[test]
    fn adapt_tslp_tags_real_rust_compiles() {
        // The rust tags.scm from TSLP must produce a query string that tree-sitter compiles
        // against the rust grammar — guards against ever drifting capture rewrites.
        let raw = tree_sitter_language_pack::get_tags_query("rust").expect("rust ships tags.scm");
        let adapted = adapt_tslp_tags(raw);
        let sym = extract_section(&adapted, "symbols").expect("symbols section");
        let calls = extract_section(&adapted, "calls").expect("calls section");
        let impls = extract_section(&adapted, "implementations").expect("implementations section");
        let ts_lang = language("rust").expect("rust language resolves");
        Query::new(&ts_lang, &sym).expect("adapted symbols query compiles");
        Query::new(&ts_lang, &calls).expect("adapted calls query compiles");
        Query::new(&ts_lang, &impls).expect("adapted implementations query compiles");
    }

    #[test]
    fn implementations_query_compiles_for_all_override_languages() {
        // Verify that the `;; section: implementations` block in each hand-written .scm
        // override compiles against the respective tree-sitter grammar. Go is excluded
        // because its implementations section is intentionally empty (structural typing).
        let langs_with_impls = &["rust", "python", "typescript", "tsx", "javascript"];
        for &lang in langs_with_impls {
            let q = try_get_query(lang, QueryKind::Implementations)
                .unwrap_or_else(|e| panic!("{lang} implementations query compile error: {e}"));
            assert!(
                q.is_some(),
                "{lang} implementations section must exist in override .scm"
            );
            // Validate the compiled query has our expected captures.
            let q = q.unwrap();
            let names = q.capture_names();
            assert!(
                names.contains(&"impl.trait_name"),
                "{lang} implementations query must capture @impl.trait_name; captures: {names:?}"
            );
            assert!(
                names.contains(&"impl.implementor"),
                "{lang} implementations query must capture @impl.implementor; captures: {names:?}"
            );
        }
    }
}
