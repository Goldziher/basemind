//! Path-filtering for the scanner: include/exclude globs + submodule pruning (`Filters`), and the
//! incremental-path indexability oracle with full nested-`.gitignore` stacking (`IndexFilter`).
//!
//! Split out of `scanner.rs` to keep that module under the 1000-line cap; the filtering concern is
//! self-contained and shared between the full scan, the incremental `scan_paths`, and the watcher.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ahash::{AHashMap, AHashSet};
use globset::GlobSet;
use ignore::WalkBuilder;

use crate::config::Config;
use crate::config::rules::{compile_patterns, expand_pattern};
use crate::lang_rules::{Detection, LangRules};
use crate::scanner::{ScanError, ScanSource, submodule_roots_for_source};

/// Exclude globs that are **always** applied, on top of (never replaced by) the user's
/// `scan.exclude`. These are near-universal build-artifact, dependency-cache, VCS, and editor
/// directories that are never worth indexing: indexing them wastes scan time, pollutes symbol /
/// reference results with vendored or generated code, and — for symlink-heavy trees like Bazel's
/// `bazel-*` convenience symlinks — can send a `follow_symlinks` walk out of the repo entirely.
/// Keeping this a hard floor (rather than leaning on the user-replaceable `default_exclude`) means a
/// user who narrows `scan.exclude` to a single custom pattern still gets these pruned.
const FLOOR_EXCLUDES: &[&str] = &[
    "**/node_modules/**",
    "**/dist/**",
    "**/build/**",
    "**/out/**",
    "**/coverage/**",
    "**/.next/**",
    "**/.nuxt/**",
    "**/.svelte-kit/**",
    "**/.venv/**",
    "**/venv/**",
    "**/__pycache__/**",
    "**/*.pyc",
    "**/.pytest_cache/**",
    "**/.mypy_cache/**",
    "**/.ruff_cache/**",
    "**/.tox/**",
    "**/target/**",
    "**/.gradle/**",
    "**/vendor/**",
    "**/.terraform/**",
    "**/bazel-*/**",
    "**/bazel-out/**",
    "**/bazel-bin/**",
    "**/bazel-testlogs/**",
    "**/.git/**",
    "**/.basemind/**",
    "**/.idea/**",
    "**/.DS_Store",
    // Credentials and key material: indexing them makes the secret searchable by every agent that
    // can query the index. `[scan] floor_allow` lists the entry (`.env.*`, `*.pem`) to opt back in.
    "**/.env",
    "**/.env.*",
    "**/.aws/**",
    "**/.ssh/**",
    "**/.gnupg/**",
    "**/.npmrc",
    "**/.pypirc",
    "**/.netrc",
    "**/.git-credentials",
    "**/id_rsa",
    "**/id_dsa",
    "**/id_ecdsa",
    "**/id_ed25519",
    "**/*.pem",
    "**/*.key",
    "**/*.p12",
    "**/*.pfx",
    "**/*.jks",
    "**/*.keystore",
];

/// Floor entries that `[scan] floor_allow` may never remove: indexing VCS internals or basemind's
/// own state would corrupt the index.
const FLOOR_PROTECTED: &[&str] = &["**/.git/**", "**/.basemind/**"];

/// The directory / file name a floor pattern targets: `**/build/**` -> `build`, `**/*.pyc` -> `*.pyc`.
fn floor_name(pattern: &str) -> &str {
    let p = pattern.strip_prefix("**/").unwrap_or(pattern);
    p.strip_suffix("/**").unwrap_or(p)
}

/// True when a `floor_allow` entry names `pattern`: the pattern itself, its directory name, or the
/// `**/name/**` form, ignoring surrounding slashes.
fn floor_allow_names(entry: &str, pattern: &str) -> bool {
    let e = entry.trim().trim_matches('/');
    e == pattern || e == floor_name(pattern) || format!("**/{e}/**") == pattern
}

/// The always-on exclude floor minus the entries the user removed via `[scan] floor_allow`.
fn effective_floor(floor_allow: &[String]) -> Vec<&'static str> {
    for entry in floor_allow {
        let matched = FLOOR_EXCLUDES.iter().any(|p| floor_allow_names(entry, p));
        let protected = FLOOR_PROTECTED.iter().any(|p| floor_allow_names(entry, p));
        if protected {
            tracing::warn!(entry, "[scan] floor_allow cannot remove this entry; it stays excluded");
        } else if !matched {
            tracing::warn!(
                entry,
                "[scan] floor_allow entry matches no exclude-floor entry; ignored"
            );
        }
    }
    FLOOR_EXCLUDES
        .iter()
        .copied()
        .filter(|p| FLOOR_PROTECTED.contains(p) || !floor_allow.iter().any(|e| floor_allow_names(e, p)))
        .collect()
}

/// Include/exclude pair precompiled once per scan. An absent include means "everything"; exclude
/// always wins.
#[derive(Default)]
struct Gate {
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
}

impl Gate {
    fn build(include: &[String], exclude: &[String]) -> Result<Self, ScanError> {
        let compile = |list: &[String]| -> Result<Option<GlobSet>, ScanError> {
            if list.is_empty() {
                return Ok(None);
            }
            compile_patterns(list.iter().map(String::as_str))
                .map(Some)
                .map_err(ScanError::BadGlob)
        };
        Ok(Self {
            include: compile(include)?,
            exclude: compile(exclude)?,
        })
    }

    fn allows(&self, key: &str) -> bool {
        !self.exclude.as_ref().is_some_and(|e| e.is_match(key)) && self.include.as_ref().is_none_or(|i| i.is_match(key))
    }
}

pub(crate) struct Filters {
    include: globset::GlobSet,
    exclude: globset::GlobSet,
    /// Directory-level view of `exclude`: every `P/**` pattern also present as bare `P`, so a
    /// directory can be pruned before its children are ever stat'd. See [`dir_exclude_patterns`].
    dir_exclude: Arc<globset::GlobSet>,
    /// Mirror of `config.scan.max_file_bytes`; the per-file size cap is enforced by the scanner.
    pub(crate) max_file_bytes: u64,
    /// Mirror of `config.documents.max_file_bytes`, the cap for the document tier.
    #[cfg_attr(not(feature = "documents"), allow(dead_code))]
    pub(crate) doc_max_file_bytes: u64,
    /// [`crate::config::rules::code_digest`] / [`crate::config::rules::doc_digest`] of the config
    /// this scan runs under; cached derived data is reusable only under the same digest.
    #[cfg_attr(not(feature = "code-search"), allow(dead_code))]
    pub(crate) code_digest: String,
    #[cfg_attr(not(feature = "documents"), allow(dead_code))]
    pub(crate) doc_digest: String,
    /// Pre-built `"{root}/"` prefix strings for each skipped submodule root — avoids a `format!`
    /// allocation per candidate file in the `allows` hot path. Empty when there are no submodules
    /// or `config.scan.skip_submodules` is off. `Arc` so [`Filters::dir_pruner`] is a refcount bump.
    submodule_prefixes: Arc<[String]>,
    /// Mirror of `config.scan.eager_l2`. When true the scanner runs L2 extraction inline
    /// with L1 and pushes calls to the Fjall index. Off → calls index stays stale until
    /// the on-demand lazy path runs.
    pub(crate) eager_l2: bool,
    /// Compiled `[languages]` overrides; see [`Filters::detect_lang`].
    lang: LangRules,
    /// `[documents] include/exclude`: which non-code files are indexed as documents.
    #[cfg_attr(not(feature = "documents"), allow(dead_code))]
    doc_scope: Gate,
    /// `[documents] embed_include/embed_exclude`.
    #[cfg_attr(not(feature = "documents"), allow(dead_code))]
    doc_embed: Gate,
    /// `[code_search] embed_include/embed_exclude`.
    #[cfg_attr(not(feature = "code-search"), allow(dead_code))]
    code_embed: Gate,
    /// Canonical `"{extra_root}/"` prefixes of `scan.extra_roots`, used to turn an extra-root
    /// file's absolute key back into the root-relative path the globs are written against.
    extra_prefixes: Vec<String>,
    /// Set by the full scan when the embed policy (see `config::rules::embed_policy_digest`) changed
    /// since the last scan: unchanged files must then be re-flushed so their vector rows are rebuilt.
    #[cfg_attr(not(any(feature = "code-search", feature = "documents")), allow(dead_code))]
    pub(crate) reflush_embeds: bool,
}

impl Filters {
    pub(crate) fn build(config: &Config, submodule_roots: Vec<String>) -> Result<Self, ScanError> {
        let include_patterns: Vec<String> = config.scan.include.iter().flat_map(|p| expand_pattern(p)).collect();
        let include = compile_globs(include_patterns.iter().map(String::as_str))?;
        let exclude_patterns: Vec<String> = effective_floor(&config.scan.floor_allow)
            .into_iter()
            .map(str::to_string)
            .chain(config.scan.exclude.iter().flat_map(|p| expand_pattern(p)))
            .collect();
        let exclude = compile_globs(exclude_patterns.iter().map(String::as_str))?;
        let dir_exclude = compile_globs(dir_exclude_patterns(&exclude_patterns).iter().map(String::as_str))?;
        let submodule_prefixes: Arc<[String]> = if config.scan.skip_submodules {
            submodule_roots
                .into_iter()
                .map(|s| s.trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .map(|r| format!("{r}/"))
                .collect()
        } else {
            Arc::from([] as [String; 0])
        };
        let extra_prefixes = config
            .scan
            .extra_roots
            .iter()
            .filter_map(|r| r.canonicalize().ok())
            .filter_map(|r| {
                r.to_str()
                    .map(|s| format!("{}/", s.replace('\\', "/").trim_end_matches('/')))
            })
            .collect();
        Ok(Self {
            include,
            exclude,
            dir_exclude: Arc::new(dir_exclude),
            max_file_bytes: config.scan.max_file_bytes,
            doc_max_file_bytes: config.documents.max_file_bytes,
            code_digest: crate::config::rules::code_digest(&config.code_search),
            doc_digest: crate::config::rules::doc_digest(&config.documents, &config.resources, &config.llm),
            submodule_prefixes,
            eager_l2: config.scan.eager_l2,
            lang: LangRules::from_config(&config.languages).map_err(ScanError::BadLanguage)?,
            doc_scope: Gate::build(&config.documents.include, &config.documents.exclude)?,
            doc_embed: Gate::build(&config.documents.embed_include, &config.documents.embed_exclude)?,
            code_embed: Gate::build(&config.code_search.embed_include, &config.code_search.embed_exclude)?,
            extra_prefixes,
            reflush_embeds: false,
        })
    }

    /// Classify `rel` with the `[languages]` overrides layered over built-in detection.
    pub(crate) fn detect_lang(&self, rel: &str) -> Detection {
        self.lang.detect(Path::new(rel))
    }

    /// `key` as the globs see it: an extra-root file's absolute key is reduced to its path
    /// relative to that root; repo-relative keys pass through.
    fn scoped<'a>(&self, key: &'a str) -> &'a str {
        if self.extra_prefixes.is_empty() || !crate::path::is_external_key(key.as_bytes()) {
            return key;
        }
        // Longest matching root wins, so a root nested inside another is scoped relative to itself.
        self.extra_prefixes
            .iter()
            .filter_map(|p| key.strip_prefix(p.as_str()))
            .min_by_key(|rest| rest.len())
            .unwrap_or(key)
    }

    /// `[documents] include/exclude`: may `rel` be indexed as a document.
    #[cfg_attr(not(feature = "documents"), allow(dead_code))]
    pub(crate) fn doc_allowed(&self, rel: &str) -> bool {
        self.doc_scope.allows(self.scoped(rel))
    }

    /// `[documents] embed_include/embed_exclude`: may the document at `rel` be embedded.
    #[cfg_attr(not(feature = "documents"), allow(dead_code))]
    pub(crate) fn doc_embed_allowed(&self, rel: &str) -> bool {
        self.doc_embed.allows(self.scoped(rel))
    }

    /// `[code_search] embed_include/embed_exclude`: may the source file at `rel` be embedded.
    #[cfg_attr(not(feature = "code-search"), allow(dead_code))]
    pub(crate) fn code_embed_allowed(&self, rel: &str) -> bool {
        self.code_embed.allows(self.scoped(rel))
    }

    /// True when `rel` is dropped by the exclude globs or a skipped submodule root — the shared
    /// gate behind [`Filters::allows`] (files).
    fn excluded(&self, rel: &str) -> bool {
        self.exclude.is_match(rel) || under_submodule(rel, &self.submodule_prefixes)
    }

    pub(crate) fn allows(&self, rel: &str) -> bool {
        if self.excluded(rel) {
            return false;
        }
        self.include.is_match(rel)
    }

    /// [`Filters::allows`] for a path relative to an `extra_roots` entry: globs only. Submodule
    /// roots are repo-relative, so they say nothing about a tree outside the repository.
    pub(crate) fn allows_in_extra_root(&self, rel_to_extra_root: &str) -> bool {
        !self.exclude.is_match(rel_to_extra_root) && self.include.is_match(rel_to_extra_root)
    }

    /// Directory gate. A directory is kept iff it is NOT matched by the directory-level exclude
    /// set and not under a skipped submodule root. Include globs are irrelevant for directories
    /// (they match files), so only the exclude set + submodule pruning apply.
    ///
    /// Used by the Linux watcher to decide which directories to register an inotify watch on, so
    /// permission-denied or excluded trees are never handed to inotify. Matching against
    /// `dir_exclude` rather than `exclude` is what makes it drop `node_modules` itself and not
    /// merely `node_modules/react` — before this the watcher watched every excluded tree's root.
    ///
    /// Only the Linux watcher registers per-directory watches — macOS/Windows use one recursive
    /// watch — so the method is dead code elsewhere; `test` keeps the unit test building on any
    /// host. The scan walk shares the rule through [`DirPruner`], which needs an owned `'static`
    /// gate and therefore calls [`dir_allowed`] directly.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn allows_dir(&self, rel: &str) -> bool {
        dir_allowed(rel, &self.dir_exclude, &self.submodule_prefixes)
    }

    /// Detach a `'static`, `Send + Sync` directory gate for `WalkBuilder::filter_entry`, which
    /// cannot borrow `Filters`. `base` is the directory the walk was rooted at when candidate keys
    /// are repo-relative (the primary walk), or `None` when they are absolute (extra roots).
    pub(crate) fn dir_pruner(&self, base: Option<&Path>) -> DirPruner {
        DirPruner {
            dir_exclude: Arc::clone(&self.dir_exclude),
            submodule_prefixes: Arc::clone(&self.submodule_prefixes),
            base: base.map(Path::to_path_buf),
        }
    }

    /// Directory gate for an `extra_roots` walk rooted at `extra_root`: directories are matched by
    /// their path relative to that root, so an absolute prefix containing `build` or `out` cannot
    /// drop the whole root.
    pub(crate) fn dir_pruner_for_extra_root(&self, extra_root: &Path) -> DirPruner {
        DirPruner {
            dir_exclude: Arc::clone(&self.dir_exclude),
            submodule_prefixes: Arc::from([] as [String; 0]),
            base: Some(extra_root.to_path_buf()),
        }
    }
}

/// True when `rel` is a skipped submodule root or lives under one. `prefixes` carry the trailing
/// `/`, so the root itself is the "one shorter, same bytes" case.
fn under_submodule(rel: &str, prefixes: &[String]) -> bool {
    prefixes
        .iter()
        .any(|p| rel.starts_with(p.as_str()) || (p.len() == rel.len() + 1 && p.starts_with(rel)))
}

fn dir_allowed(rel: &str, dir_exclude: &globset::GlobSet, submodule_prefixes: &[String]) -> bool {
    !dir_exclude.is_match(rel) && !under_submodule(rel, submodule_prefixes)
}

/// Directory-level projection of the exclude patterns, built **only** from the `P/**` family:
/// each such pattern is kept, and is also added with the suffix stripped (`**/node_modules/**` →
/// `**/node_modules`), because `P/**` matches paths *beneath* `P` but never `P` itself.
///
/// **Why this is candidate-set-preserving, by construction.** `P/**` excludes every file beneath
/// `P`, so refusing to descend into directory `P` removes only paths [`Filters::allows`] would have
/// rejected one stat later. The index is byte-identical; only the walk is cheaper. (That is the
/// whole point — a non-gitignored `node_modules` used to be traversed and stat'd in full before
/// every one of its files was thrown away.)
///
/// **Why patterns without the `/**` suffix are deliberately dropped.** Such a pattern constrains
/// files, not subtrees: `**/generated` matches the directory `a/generated` but not the file
/// `a/generated/x.rs`, which is therefore indexed today. Pruning that directory would silently
/// delete real files from the index — trading a memory fix for a data-loss bug — so the directory
/// gate simply never sees those patterns. `Filters::allows` still applies the full set to files, so
/// nothing stops being excluded that was excluded before.
///
/// This is also why the watcher shares this set rather than the file-level one: gating inotify
/// registration on `**/generated` made it blind to changes under a directory it was indexing.
fn dir_exclude_patterns(patterns: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(patterns.len() * 2);
    for p in patterns {
        let Some(stem) = p.strip_suffix("/**") else {
            continue;
        };
        if stem.is_empty() {
            continue;
        }
        out.push(p.clone());
        out.push(stem.to_string());
    }
    out
}

/// Owned, `'static` directory gate for `WalkBuilder::filter_entry` (which demands
/// `Fn(&DirEntry) -> bool + Send + Sync + 'static` and so cannot borrow [`Filters`]). Cloned out of
/// a `Filters` by [`Filters::dir_pruner`]; every field is a refcount bump.
pub(crate) struct DirPruner {
    dir_exclude: Arc<globset::GlobSet>,
    submodule_prefixes: Arc<[String]>,
    base: Option<PathBuf>,
}

impl DirPruner {
    /// Should the walker descend into / yield `dent`? **Only directories are ever pruned** —
    /// files fall through to the unchanged per-file [`Filters::allows`] gate, which is also where
    /// the include globs live.
    pub(crate) fn keep(&self, dent: &ignore::DirEntry) -> bool {
        // Depth 0 is the walk root, whose relative path is `""`; pruning it would empty the scan ~keep
        // for a repo that happens to be named `target`. ~keep
        if dent.depth() == 0 {
            return true;
        }
        if !dent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            return true;
        }
        let path = dent.path();
        let candidate = match &self.base {
            Some(base) => match path.strip_prefix(base) {
                Ok(rel) => rel,
                Err(_) => return true,
            },
            None => path,
        };
        let Some(rel) = candidate.to_str() else {
            return true;
        };
        #[cfg(windows)]
        let rel_owned = rel.replace('\\', "/");
        #[cfg(windows)]
        let rel = rel_owned.as_str();
        if rel.is_empty() {
            return true;
        }
        dir_allowed(rel, &self.dir_exclude, &self.submodule_prefixes)
    }
}

/// Compile `patterns` (already expanded) into one set; a bad glob is a scan error, never dropped.
fn compile_globs<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<globset::GlobSet, ScanError> {
    let mut b = globset::GlobSetBuilder::new();
    for p in patterns {
        let g = globset::Glob::new(p).map_err(|e| ScanError::BadGlob(format!("{p:?}: {e}")))?;
        b.add(g);
    }
    b.build().map_err(|e| ScanError::BadGlob(format!("{e}")))
}

/// Single source of truth for the `ignore` crate's walk configuration. Both the full-scan
/// `walk_candidates` and the incremental `IndexFilter` build their walkers here so the gitignore /
/// git-exclude / hidden semantics stay identical between a full scan and a watcher batch.
pub(crate) fn ignore_walk_builder(dir: &Path, respect_gitignore: bool, follow_links: bool) -> WalkBuilder {
    let mut b = WalkBuilder::new(dir);
    b.standard_filters(respect_gitignore)
        .follow_links(follow_links)
        .git_ignore(respect_gitignore)
        .git_exclude(respect_gitignore)
        .hidden(false);
    b
}

/// Indexability oracle for the **incremental** path (watcher + `scan_paths`), matching what a full
/// scan would index. A full scan keeps a file iff it passes the include/exclude globs (`Filters`)
/// AND survives the `ignore` crate's gitignore walk (`walk_candidates`). `IndexFilter` reproduces
/// both layers per-path so a watcher batch never indexes — or wakes on — a path the full scan
/// would drop.
///
/// The gitignore layer honors the **full nested `.gitignore` hierarchy** (not just the repo-root
/// file) by composing per-directory shallow walks via the `ignore` crate's own engine: a path is
/// gitignore-allowed iff every path segment, from the repo root down, is yielded as a non-ignored
/// child of its parent directory. A per-instance memo caches each directory's allowed-children set,
/// so a batch touching K distinct directories costs at most K `max_depth(1)` walks. Composing
/// level-by-level with `parents(false)` correctly rejects a path whose *ancestor* directory is
/// itself gitignored — the case a single flat `Gitignore` matcher gets wrong.
pub(crate) struct IndexFilter {
    filters: Filters,
    root: PathBuf,
    respect_gitignore: bool,
    /// Mirror of `config.scan.follow_symlinks`. Threaded into the per-directory shallow walks so the
    /// incremental path resolves symlinked children the same way a full scan's `walk_candidates` does.
    follow_links: bool,
    /// dir → set of its non-ignored immediate children (absolute paths). `RefCell` because the
    /// memo is filled lazily during the otherwise-`&self` `is_indexable` check; the filter is only
    /// ever driven from a single thread (the watcher loop / the `scan_paths` filter loop).
    allowed_children: RefCell<AHashMap<PathBuf, AHashSet<PathBuf>>>,
}

impl IndexFilter {
    pub(crate) fn new(root: &Path, config: &Config) -> Result<Self, ScanError> {
        let submodule_roots = submodule_roots_for_source(root, &ScanSource::WorkingTree);
        let filters = Filters::build(config, submodule_roots)?;
        Ok(Self {
            filters,
            root: root.to_path_buf(),
            respect_gitignore: config.scan.respect_gitignore,
            follow_links: config.scan.follow_symlinks,
            allowed_children: RefCell::new(AHashMap::new()),
        })
    }

    /// Drop every cached directory listing. The watcher reuses one `IndexFilter` across batches
    /// (so submodule discovery / globset compilation happens once); clearing between batches makes
    /// a freshly-added or edited `.gitignore` take effect on the next batch.
    pub(crate) fn clear_cache(&self) {
        self.allowed_children.borrow_mut().clear();
    }

    /// Cheap, path-only glob/submodule gate (no filesystem I/O). Mirrors `Filters::allows`. Use for
    /// **deleted** paths (a vanished file can't be gitignore-walked, but a previously-indexed file
    /// must still be forwarded for pruning) and as the first gate everywhere else.
    pub(crate) fn allows_glob(&self, rel: &str) -> bool {
        self.filters.allows(rel)
    }

    /// Borrow the underlying glob/submodule filters — `run_candidates` needs them and they were
    /// already compiled when this `IndexFilter` was built, so there is no reason to build a second
    /// `Filters`.
    pub(crate) fn filters(&self) -> &Filters {
        &self.filters
    }

    /// Repo-relative, forward-slash path for `abs`, or `None` when `abs` is outside the root.
    fn rel_of(&self, abs: &Path) -> Option<String> {
        let rel = abs.strip_prefix(&self.root).ok()?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() { None } else { Some(rel) }
    }

    /// Would a full scan index `abs`? Applies the glob gate, then (when `respect_gitignore`) the
    /// nested-gitignore walk. Assumes `abs` exists; callers handle deletions via [`allows_glob`].
    pub(crate) fn is_indexable(&self, abs: &Path) -> bool {
        let Some(rel) = self.rel_of(abs) else {
            return false;
        };
        if !self.filters.allows(&rel) {
            return false;
        }
        if !self.is_regular_file(abs) {
            return false;
        }
        if !self.respect_gitignore {
            return true;
        }
        self.gitignore_allows(abs)
    }

    /// The full scan only yields regular files (`dent.file_type().is_file()`), and never follows a
    /// link unless `follow_symlinks` is set. Mirror both: without it, no path component below the
    /// root may be a symlink (a tracked `notes.md -> ~/.ssh/id_rsa`, a `link -> /etc` directory, or
    /// `x.rs -> /dev/zero` must not be read); with it, the resolved target must be a regular file.
    fn is_regular_file(&self, abs: &Path) -> bool {
        if self.follow_links {
            return std::fs::metadata(abs).is_ok_and(|m| m.is_file());
        }
        let Ok(rel) = abs.strip_prefix(&self.root) else {
            return false;
        };
        let mut cur = self.root.clone();
        let mut last_is_file = false;
        for comp in rel.components() {
            cur.push(comp.as_os_str());
            let Ok(meta) = std::fs::symlink_metadata(&cur) else {
                return false;
            };
            if meta.file_type().is_symlink() {
                return false;
            }
            last_is_file = meta.is_file();
        }
        last_is_file
    }

    /// Walk the path's segments root→leaf; reject as soon as a segment is a gitignored child of its
    /// parent. Memoized per directory.
    fn gitignore_allows(&self, abs: &Path) -> bool {
        let Ok(rel) = abs.strip_prefix(&self.root) else {
            return false;
        };
        let mut cur = self.root.clone();
        for comp in rel.components() {
            let child = cur.join(comp.as_os_str());
            {
                let mut memo = self.allowed_children.borrow_mut();
                let allowed = memo
                    .entry(cur.clone())
                    .or_insert_with(|| shallow_allowed_children(&cur, self.respect_gitignore, self.follow_links));
                if !allowed.contains(&child) {
                    return false;
                }
            }
            cur = child;
        }
        true
    }
}

/// Non-ignored immediate children (files and directories) of `dir`, as absolute paths, per the
/// `ignore` crate. `parents(false)` keeps each directory's `.gitignore` scoped to its own level so
/// the caller can compose the hierarchy; `max_depth(1)` lists children without descending.
fn shallow_allowed_children(dir: &Path, respect_gitignore: bool, follow_links: bool) -> AHashSet<PathBuf> {
    let mut set = AHashSet::new();
    let walker = ignore_walk_builder(dir, respect_gitignore, follow_links)
        .parents(false)
        .max_depth(Some(1))
        .build();
    for dent in walker.flatten() {
        let p = dent.path();
        if p == dir {
            continue;
        }
        set.insert(p.to_path_buf());
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build an `IndexFilter` rooted at a fresh temp dir, run `body` to populate the tree, and
    /// return `(filter, root, tmp)`. `root` is canonicalized to match the absolute paths the filter
    /// and the `ignore` walker compare against. The caller must keep `tmp` bound for the duration of
    /// the test so the tree stays on disk while the filter walks it.
    fn filter_for(body: impl FnOnce(&Path)) -> (IndexFilter, PathBuf, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        fs::create_dir_all(root.join(".git")).expect("mkdir .git");
        body(&root);
        let config = crate::config::default_for_root(&root);
        let filter = IndexFilter::new(&root, &config).expect("build filter");
        (filter, root, tmp)
    }

    #[test]
    fn should_reject_path_under_nested_gitignore_rule() {
        let (filter, root, _tmp) = filter_for(|root| {
            fs::create_dir_all(root.join("sub")).unwrap();
            fs::write(root.join("sub/.gitignore"), b"ignored.rs\n").unwrap();
            fs::write(root.join("sub/ignored.rs"), b"fn a() {}\n").unwrap();
            fs::write(root.join("sub/kept.rs"), b"fn b() {}\n").unwrap();
        });
        assert!(
            !filter.is_indexable(&root.join("sub/ignored.rs")),
            "a file matched by its own dir's nested .gitignore must be rejected"
        );
        assert!(
            filter.is_indexable(&root.join("sub/kept.rs")),
            "a tracked sibling must be kept"
        );
    }

    #[test]
    fn should_reject_path_when_ancestor_directory_is_gitignored() {
        let (filter, root, _tmp) = filter_for(|root| {
            fs::write(root.join(".gitignore"), b"build/\n").unwrap();
            fs::create_dir_all(root.join("build/nested")).unwrap();
            fs::write(root.join("build/nested/out.rs"), b"fn c() {}\n").unwrap();
            fs::write(root.join("main.rs"), b"fn main() {}\n").unwrap();
        });
        assert!(
            !filter.is_indexable(&root.join("build/nested/out.rs")),
            "a file under an ancestor-gitignored directory must be rejected"
        );
        assert!(
            filter.is_indexable(&root.join("main.rs")),
            "a tracked top-level file must be kept"
        );
    }

    #[test]
    fn should_reject_root_and_nested_basemind_via_default_exclude() {
        let (filter, root, _tmp) = filter_for(|root| {
            fs::create_dir_all(root.join(".basemind")).unwrap();
            fs::write(root.join(".basemind/x.msgpack"), b"\x00").unwrap();
            fs::create_dir_all(root.join("child/.basemind")).unwrap();
            fs::write(root.join("child/.basemind/y.msgpack"), b"\x00").unwrap();
            fs::write(root.join("child/real.rs"), b"fn d() {}\n").unwrap();
        });
        assert!(!filter.allows_glob(".basemind/x.msgpack"));
        assert!(!filter.allows_glob("child/.basemind/y.msgpack"));
        assert!(!filter.is_indexable(&root.join(".basemind/x.msgpack")));
        assert!(!filter.is_indexable(&root.join("child/.basemind/y.msgpack")));
        assert!(
            filter.is_indexable(&root.join("child/real.rs")),
            "a real source file beside a nested .basemind must still be kept"
        );
    }

    #[test]
    fn embed_gates_compose_include_and_exclude_with_exclude_winning() {
        let mut config = crate::config::default_for_root(Path::new("."));
        let filters = Filters::build(&config, Vec::new()).expect("build filters");
        assert!(filters.code_embed_allowed("src/lib.rs"), "empty lists allow everything");
        assert!(filters.doc_embed_allowed("docs/a.pdf"));
        assert!(filters.doc_allowed("docs/a.pdf"));

        config.code_search.embed_include = vec!["src/**".to_string()];
        config.code_search.embed_exclude = vec!["**/generated/**".to_string(), "**/*.min.js".to_string()];
        config.documents.include = vec!["docs".to_string()];
        config.documents.exclude = vec!["**/draft*".to_string()];
        config.documents.embed_include = vec!["**/*.pdf".to_string()];
        config.documents.embed_exclude = vec!["docs/huge".to_string()];
        let filters = Filters::build(&config, Vec::new()).expect("build filters");

        assert!(filters.code_embed_allowed("src/lib.rs"));
        assert!(!filters.code_embed_allowed("tools/x.rs"), "outside embed_include");
        assert!(
            !filters.code_embed_allowed("src/generated/schema.rs"),
            "exclude beats include"
        );
        assert!(!filters.code_embed_allowed("src/bundle.min.js"));

        assert!(filters.doc_allowed("docs/guide.md"));
        assert!(!filters.doc_allowed("other/guide.md"), "outside documents.include");
        assert!(
            !filters.doc_allowed("docs/draft-1.md"),
            "documents.exclude beats include"
        );
        assert!(filters.doc_embed_allowed("docs/a.pdf"));
        assert!(!filters.doc_embed_allowed("docs/a.md"), "outside embed_include");
        assert!(
            !filters.doc_embed_allowed("docs/huge/a.pdf"),
            "bare embed_exclude covers the subtree"
        );
    }

    #[test]
    fn invalid_globs_fail_filter_construction() {
        let mut config = crate::config::default_for_root(Path::new("."));
        config.documents.embed_exclude = vec!["a/[".to_string()];
        assert!(matches!(
            Filters::build(&config, Vec::new()),
            Err(ScanError::BadGlob(_))
        ));
        let mut config = crate::config::default_for_root(Path::new("."));
        config.documents.include = vec!["a/[".to_string()];
        assert!(matches!(
            Filters::build(&config, Vec::new()),
            Err(ScanError::BadGlob(_))
        ));
    }

    #[test]
    fn bare_exclude_name_excludes_the_subtree_and_prunes_the_directory() {
        let mut config = crate::config::default_for_root(Path::new("."));
        config.scan.exclude = vec!["generated".to_string()];
        let filters = Filters::build(&config, Vec::new()).expect("build filters");
        assert!(!filters.allows("generated/schema.rs"));
        assert!(!filters.allows("pkg/generated/deep/schema.rs"));
        assert!(filters.allows("pkg/generated_code/schema.rs"));
        assert!(
            !filters.allows_dir("generated"),
            "the walker prunes the directory itself"
        );
        assert!(!filters.allows_dir("pkg/generated"));
    }

    #[test]
    fn floor_allow_removes_named_floor_entries_but_never_git_or_basemind() {
        let mut config = crate::config::default_for_root(Path::new("."));
        let baseline = Filters::build(&config, Vec::new()).expect("build filters");
        assert!(!baseline.allows("build/gen.rs"));
        assert!(!baseline.allows("vendor/dep/lib.go"));

        config.scan.floor_allow = vec![
            "build".to_string(),
            "**/vendor/**".to_string(),
            ".git".to_string(),
            "no-such-entry".to_string(),
        ];
        let filters = Filters::build(&config, Vec::new()).expect("build filters");
        assert!(filters.allows("build/gen.rs"), "named by directory");
        assert!(filters.allows("vendor/dep/lib.go"), "named by floor pattern");
        assert!(
            filters.allows_dir("build"),
            "the walker descends into an allowed floor dir"
        );
        assert!(!filters.allows(".git/config"), ".git can never be allowed");
        assert!(!filters.allows("out/x.rs"), "untouched floor entries stay");
    }

    #[test]
    fn secrets_are_excluded_by_default_and_floor_allow_opts_back_in() {
        let mut config = crate::config::default_for_root(Path::new("."));
        let filters = Filters::build(&config, Vec::new()).expect("build filters");
        for secret in [
            ".env",
            "svc/.env.production",
            ".aws/credentials",
            "home/.ssh/config",
            ".npmrc",
            "deploy/id_rsa",
            "certs/server.pem",
            "certs/tls.key",
        ] {
            assert!(!filters.allows(secret), "{secret} must not be indexed");
        }
        assert!(filters.allows("src/environment.rs"));
        assert!(filters.allows("keys.rs"));

        config.scan.floor_allow = vec![".env.*".to_string(), "*.pem".to_string()];
        let filters = Filters::build(&config, Vec::new()).expect("build filters");
        assert!(filters.allows("svc/.env.production"));
        assert!(filters.allows("certs/server.pem"));
        assert!(!filters.allows(".env"), "unlisted entries stay excluded");
    }

    #[test]
    fn extra_root_files_match_globs_relative_to_the_root() {
        let config = crate::config::default_for_root(Path::new("."));
        let filters = Filters::build(&config, Vec::new()).expect("build filters");
        assert!(
            filters.allows_in_extra_root("src/lib.rs"),
            "root-relative path is judged on its own, not on where the root lives"
        );
        assert!(!filters.allows_in_extra_root("node_modules/x/index.js"));
        assert!(
            !filters.allows("/opt/build/ext/src/lib.rs"),
            "an absolute key under a floor-named directory is what the old check tripped on"
        );
    }

    #[test]
    fn nested_extra_roots_scope_a_key_to_the_longest_matching_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");
        for dir in ["ext", "ext/inner", "extra"] {
            fs::create_dir_all(base.join(dir)).expect("mkdir");
        }
        let mut config = crate::config::default_for_root(Path::new("."));
        config.scan.extra_roots = vec![base.join("ext"), base.join("ext/inner"), base.join("extra")];
        let filters = Filters::build(&config, Vec::new()).expect("build filters");
        let key = |rel: &str| format!("{}/{rel}", base.display());

        assert_eq!(filters.scoped(&key("ext/a.md")), "a.md");
        assert_eq!(
            filters.scoped(&key("ext/inner/a.md")),
            "a.md",
            "the nested root wins, not the first listed"
        );
        assert_eq!(filters.scoped(&key("extra/a.md")), "a.md", "/ext must not claim /extra");
    }

    #[test]
    fn should_apply_exclude_floor_even_with_a_narrow_user_exclude() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        fs::create_dir_all(root.join(".git")).expect("mkdir .git");
        let mut config = crate::config::default_for_root(&root);
        config.scan.exclude = vec!["**/mycustom/**".to_string()];
        let filter = IndexFilter::new(&root, &config).expect("build filter");

        assert!(
            !filter.allows_glob("node_modules/react/index.js"),
            "floor: node_modules"
        );
        assert!(!filter.allows_glob("target/debug/build.rs"), "floor: target");
        assert!(
            !filter.allows_glob("pkg/__pycache__/mod.pyc"),
            "floor: __pycache__ / *.pyc"
        );
        assert!(!filter.allows_glob("bazel-out/gen/x.go"), "floor: bazel-out");
        assert!(!filter.allows_glob("mycustom/thing.rs"), "user exclude honored");
        assert!(filter.allows_glob("src/lib.rs"), "real source file kept");
    }

    #[test]
    fn should_reject_out_of_root_and_empty_rel() {
        let (filter, root, _tmp) = filter_for(|root| {
            fs::write(root.join("a.rs"), b"fn e() {}\n").unwrap();
        });
        assert!(!filter.is_indexable(&root));
        assert!(!filter.is_indexable(Path::new("/definitely/not/under/root.rs")));
    }

    /// `allows_dir` prunes directories by exclude globs only (no include-glob gate), so the
    /// watcher can register inotify watches on kept directories and skip excluded or unreadable
    /// ones — the fix for the "notify error: Permission denied" crash on unreadable trees.
    #[test]
    fn allows_dir_prunes_by_exclude_globs_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        fs::create_dir_all(root.join(".git")).expect("mkdir .git");
        let mut config = crate::config::default_for_root(&root);
        config.scan.exclude = vec!["**/generated/**".to_string()];
        let filter = IndexFilter::new(&root, &config).expect("build filter");
        let filters = filter.filters();

        // Directories under the user exclude glob are pruned. The glob `**/generated/**` matches
        // subdirectories (a trailing segment is required), so we assert on nested paths.
        assert!(!filters.allows_dir("generated/schema"), "excluded nested dir pruned");
        assert!(
            !filters.allows_dir("generated/schema/types"),
            "deeply nested excluded dir pruned"
        );
        // Plain source directories are kept.
        assert!(filters.allows_dir("src"), "kept dir");
        assert!(filters.allows_dir("src/services"), "kept nested dir");
        assert!(!filters.allows_dir("node_modules/react"), "floor: node_modules/react");
        assert!(!filters.allows_dir("target/debug"), "floor: target/debug");
        // The bare directory itself: `**/X/**` never matches `X`, so the directory-level exclude
        // set carries the `/**`-stripped form. Without it the walker descends into `node_modules`
        // and stats every file inside only to throw them all away.
        for dir in ["node_modules", "target", "vendor", ".git", "dist", "generated"] {
            assert!(!filters.allows_dir(dir), "bare excluded dir pruned: {dir}");
        }
        assert!(
            !filters.allows_dir("packages/app/node_modules"),
            "nested bare excluded dir pruned"
        );
    }

    /// A `scan.exclude` entry with no `/**` suffix constrains files, not subtrees, so it must never
    /// prune a directory. `**/generated` does not match `generated/schema.rs`, which is therefore
    /// indexed today; pruning `generated/` would delete real files from the index — a data-loss bug
    /// wearing a memory fix's clothes. This is the invariant [`dir_exclude_patterns`] exists to hold,
    /// and the reason it drops every pattern outside the `P/**` family.
    #[test]
    fn a_file_shaped_exclude_never_prunes_the_directory_it_names() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        fs::create_dir_all(root.join(".git")).expect("mkdir .git");
        let mut config = crate::config::default_for_root(&root);
        config.scan.exclude = vec!["**/generated".to_string(), "**/cache/**".to_string()];
        let filter = IndexFilter::new(&root, &config).expect("build filter");
        let filters = filter.filters();

        assert!(
            filters.allows("generated/schema.rs"),
            "a file under `generated/` is indexed today; the directory gate must not change that"
        );
        assert!(
            filters.allows_dir("generated"),
            "pruning `generated/` would drop the very file `allows` just admitted"
        );
        assert!(
            !filters.allows_dir("cache"),
            "the `/**` form still prunes, so the distinction is real and not an accident"
        );
    }

    /// The walk root is never pruned, whatever it is called: its relative path is `""` and a repo
    /// that happens to be named `target` must still scan.
    #[test]
    fn dir_pruner_keeps_the_walk_root_even_when_named_target() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize").join("target");
        fs::create_dir_all(root.join(".git")).expect("mkdir .git");
        fs::create_dir_all(root.join("node_modules/pkg")).expect("mkdir node_modules");
        fs::write(root.join("a.rs"), b"fn a() {}\n").expect("write a.rs");
        fs::write(root.join("node_modules/pkg/i.js"), b"//\n").expect("write i.js");
        let config = crate::config::default_for_root(&root);
        let filters = Filters::build(&config, Vec::new()).expect("build filters");
        let pruner = filters.dir_pruner(Some(&root));

        let kept: Vec<String> = ignore_walk_builder(&root, false, false)
            .filter_entry(move |dent| pruner.keep(dent))
            .build()
            .flatten()
            .filter_map(|d| {
                d.path()
                    .strip_prefix(&root)
                    .ok()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
            })
            .collect();

        assert!(
            kept.iter().any(|p| p == "a.rs"),
            "root named `target` still walked: {kept:?}"
        );
        assert!(
            !kept.iter().any(|p| p.starts_with("node_modules")),
            "node_modules pruned at the directory, not per file: {kept:?}"
        );
    }

    /// The only thing `filter_entry(DirPruner)` changes is how much the walker *does*: the indexed
    /// set was already identical without it, because `walk_candidates` dropped every `node_modules`
    /// path through `Filters::allows` one stat later. Scan output therefore cannot observe this fix
    /// — walk work can, so the two walks below differ by exactly the subtree the gate refuses to
    /// descend into.
    #[test]
    fn the_dir_pruner_stops_the_walk_descending_into_a_non_gitignored_node_modules() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        fs::create_dir_all(root.join("src")).expect("mkdir src");
        fs::write(root.join("src/a.rs"), b"fn a() {}\n").expect("write a.rs");
        for pkg in 0..4 {
            let pkg_dir = root.join(format!("node_modules/pkg{pkg}/lib"));
            fs::create_dir_all(&pkg_dir).expect("mkdir pkg");
            for file in 0..5 {
                fs::write(pkg_dir.join(format!("m{file}.js")), b"//\n").expect("write module");
            }
        }
        let config = crate::config::default_for_root(&root);
        let filters = Filters::build(&config, Vec::new()).expect("build filters");

        let visited = |pruner: Option<DirPruner>| -> Vec<String> {
            let mut builder = ignore_walk_builder(&root, false, false);
            if let Some(pruner) = pruner {
                builder.filter_entry(move |dent| pruner.keep(dent));
            }
            builder
                .build()
                .flatten()
                .filter_map(|d| {
                    d.path()
                        .strip_prefix(&root)
                        .ok()
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
                })
                .collect()
        };

        let ungated = visited(None);
        let pruned = visited(Some(filters.dir_pruner(Some(&root))));

        assert!(
            ungated.iter().filter(|p| p.starts_with("node_modules")).count() >= 20,
            "the ungated walk must actually descend, or the comparison proves nothing: {ungated:?}"
        );
        assert!(
            !pruned.iter().any(|p| p.starts_with("node_modules")),
            "the gated walk must not enter node_modules at all: {pruned:?}"
        );
        assert!(
            pruned.len() < ungated.len(),
            "pruning must be strictly less walk work: {} vs {}",
            pruned.len(),
            ungated.len()
        );
        assert!(
            pruned.iter().any(|p| p == "src/a.rs"),
            "real source is still walked: {pruned:?}"
        );
    }
}
