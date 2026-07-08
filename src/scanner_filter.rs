//! Path-filtering for the scanner: include/exclude globs + submodule pruning (`Filters`), and the
//! incremental-path indexability oracle with full nested-`.gitignore` stacking (`IndexFilter`).
//!
//! Split out of `scanner.rs` to keep that module under the 1000-line cap; the filtering concern is
//! self-contained and shared between the full scan, the incremental `scan_paths`, and the watcher.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use ahash::{AHashMap, AHashSet};
use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;

use crate::config::Config;
use crate::scanner::{ScanError, ScanSource, submodule_roots_for_source};

/// Exclude globs that are **always** applied, on top of (never replaced by) the user's
/// `scan.exclude`. These are near-universal build-artifact, dependency-cache, VCS, and editor
/// directories that are never worth indexing: indexing them wastes scan time, pollutes symbol /
/// reference results with vendored or generated code, and — for symlink-heavy trees like Bazel's
/// `bazel-*` convenience symlinks — can send a `follow_symlinks` walk out of the repo entirely.
/// Keeping this a hard floor (rather than leaning on the user-replaceable `default_exclude`) means a
/// user who narrows `scan.exclude` to a single custom pattern still gets these pruned.
const FLOOR_EXCLUDES: &[&str] = &[
    // JS / TS
    "**/node_modules/**",
    "**/dist/**",
    "**/build/**",
    "**/out/**",
    "**/coverage/**",
    "**/.next/**",
    "**/.nuxt/**",
    "**/.svelte-kit/**",
    // Python
    "**/.venv/**",
    "**/venv/**",
    "**/__pycache__/**",
    "**/*.pyc",
    "**/.pytest_cache/**",
    "**/.mypy_cache/**",
    "**/.ruff_cache/**",
    "**/.tox/**",
    // Rust
    "**/target/**",
    // JVM / Gradle
    "**/.gradle/**",
    // Go / general vendoring
    "**/vendor/**",
    // Terraform
    "**/.terraform/**",
    // Bazel convenience symlinks + output trees (root symlinks escape the repo under follow_symlinks)
    "**/bazel-*/**",
    "**/bazel-out/**",
    "**/bazel-bin/**",
    "**/bazel-testlogs/**",
    // VCS, cache, editor cruft
    "**/.git/**",
    "**/.basemind/**",
    "**/.idea/**",
    "**/.DS_Store",
];

pub(crate) struct Filters {
    include: globset::GlobSet,
    exclude: globset::GlobSet,
    /// Mirror of `config.scan.max_file_bytes`; the per-file size cap is enforced by the scanner.
    pub(crate) max_file_bytes: u64,
    /// Submodule root prefixes (forward-slash, no trailing `/`). When `config.scan
    /// .skip_submodules` is true, any candidate path under one of these prefixes is filtered
    /// out before extraction. Empty when there are no submodules or the knob is disabled.
    submodule_roots: Vec<String>,
    /// Pre-built `"{root}/"` prefix strings for each submodule root — avoids a `format!`
    /// allocation per candidate file in the `allows` hot path.
    submodule_prefixes: Vec<String>,
    /// Mirror of `config.scan.eager_l2`. When true the scanner runs L2 extraction inline
    /// with L1 and pushes calls to the Fjall index. Off → calls index stays stale until
    /// the on-demand lazy path runs.
    pub(crate) eager_l2: bool,
}

impl Filters {
    pub(crate) fn build(config: &Config, submodule_roots: Vec<String>) -> Result<Self, ScanError> {
        let include = compile_globs(config.scan.include.iter().map(String::as_str))?;
        // The exclude set is the always-on floor plus the user's `scan.exclude`, so narrowing the
        // user list never drops a floor entry (build artifacts, VCS dirs, symlink-escape roots).
        let exclude = compile_globs(
            FLOOR_EXCLUDES
                .iter()
                .copied()
                .chain(config.scan.exclude.iter().map(String::as_str)),
        )?;
        let submodule_roots: Vec<String> = if config.scan.skip_submodules {
            submodule_roots
                .into_iter()
                .map(|s| s.trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .collect()
        } else {
            Vec::new()
        };
        // Pre-build `"{root}/"` once so `allows` never calls `format!` per candidate file.
        let submodule_prefixes: Vec<String> = submodule_roots.iter().map(|r| format!("{r}/")).collect();
        Ok(Self {
            include,
            exclude,
            max_file_bytes: config.scan.max_file_bytes,
            submodule_roots,
            submodule_prefixes,
            eager_l2: config.scan.eager_l2,
        })
    }

    pub(crate) fn allows(&self, rel: &str) -> bool {
        if self.exclude.is_match(rel) {
            return false;
        }
        for (root, prefix) in self.submodule_roots.iter().zip(self.submodule_prefixes.iter()) {
            if rel == root || rel.starts_with(prefix.as_str()) {
                return false;
            }
        }
        self.include.is_match(rel)
    }
}

/// True when `rel` matches any of the `embed_exclude` glob `patterns` — the embed gates in
/// `scanner_code` / `scanner_docs` use this to skip embedding a file that is still chunked + indexed.
///
/// The patterns are compiled on each call rather than prebuilt-and-threaded: callers gate on
/// `embed` (off by default for code) AND on a non-empty list, so the default scan never reaches this
/// function, and when it does the globset compile is negligible against the ONNX embedding it
/// guards. Invalid globs are skipped (not fatal) so a typo in one pattern never aborts the scan.
#[cfg(any(feature = "code-search", feature = "documents"))]
pub(crate) fn embed_excluded(rel: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        if let Ok(g) = Glob::new(p) {
            b.add(g);
        }
    }
    b.build().map(|set| set.is_match(rel)).unwrap_or(false)
}

fn compile_globs<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<globset::GlobSet, ScanError> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let g = Glob::new(p).map_err(|e| ScanError::BadGlob(format!("{p:?}: {e}")))?;
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

/// Walk each configured `scan.extra_roots` directory and append its files to `out`, keyed by
/// **absolute** path (see `RelPath::is_external`). Extra roots live outside the repo, so there is
/// no `strip_prefix(root)` — the absolute path *is* the index key, which never collides with the
/// repo's relative keys. Symlinks are followed (Bazel `external/` is symlink-heavy). Missing or
/// unreadable roots are skipped with a warning; a root inside the repo is skipped because the
/// primary walk already covers it.
pub(crate) fn walk_extra_roots(root: &Path, config: &Config, filters: &Filters, out: &mut Vec<String>) {
    if config.scan.extra_roots.is_empty() {
        return;
    }
    let repo_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let start = out.len();
    for raw_root in &config.scan.extra_roots {
        let extra = match raw_root.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(root = %raw_root.display(), error = %e, "extra_root skipped: cannot access");
                continue;
            }
        };
        if !extra.is_dir() {
            tracing::warn!(root = %extra.display(), "extra_root skipped: not a directory");
            continue;
        }
        if extra.starts_with(&repo_root) {
            tracing::warn!(root = %extra.display(), "extra_root skipped: inside the repository root (already indexed)");
            continue;
        }
        for dent in ignore_walk_builder(&extra, config.scan.respect_gitignore, true)
            .build()
            .flatten()
        {
            if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let Some(abs_str) = dent.path().to_str() else {
                continue;
            };
            #[cfg(windows)]
            let abs_owned = abs_str.replace('\\', "/");
            #[cfg(windows)]
            let abs_str = abs_owned.as_str();
            if !filters.allows(abs_str) {
                continue;
            }
            out.push(abs_str.to_string());
        }
    }
    // Extra roots can overlap (nested config entries); dedup only the appended tail so the
    // repo walk's hot path is untouched when `extra_roots` is empty.
    if out.len() > start {
        out[start..].sort_unstable();
        let mut seen_tail = out.split_off(start);
        seen_tail.dedup();
        out.extend(seen_tail);
    }
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
        if !self.respect_gitignore {
            return true;
        }
        self.gitignore_allows(abs)
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
        // Mark it a git repo so `.git/info/exclude`-style resolution behaves like a real checkout;
        // `ignore` honors `.gitignore` files regardless, but this keeps semantics realistic.
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
        // The case a single flat `Gitignore` matcher gets wrong: the file itself matches nothing,
        // but its parent directory is ignored by the repo-root .gitignore.
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
        // Glob layer alone (no I/O) must reject both the root and nested child `.basemind/`.
        assert!(!filter.allows_glob(".basemind/x.msgpack"));
        assert!(!filter.allows_glob("child/.basemind/y.msgpack"));
        assert!(!filter.is_indexable(&root.join(".basemind/x.msgpack")));
        assert!(!filter.is_indexable(&root.join("child/.basemind/y.msgpack")));
        assert!(
            filter.is_indexable(&root.join("child/real.rs")),
            "a real source file beside a nested .basemind must still be kept"
        );
    }

    #[cfg(any(feature = "code-search", feature = "documents"))]
    #[test]
    fn embed_excluded_matches_globs_and_is_empty_no_op() {
        assert!(!embed_excluded("src/lib.rs", &[]), "empty patterns never exclude");
        let patterns = vec!["**/generated/**".to_string(), "**/*.min.js".to_string()];
        assert!(embed_excluded("app/generated/schema.rs", &patterns));
        assert!(embed_excluded("static/bundle.min.js", &patterns));
        assert!(
            !embed_excluded("src/lib.rs", &patterns),
            "non-matching file is embedded"
        );
    }

    #[test]
    fn should_apply_exclude_floor_even_with_a_narrow_user_exclude() {
        // A user who narrows `scan.exclude` to a single custom pattern must still get the floor
        // (node_modules, target, …) pruned — the floor is added on top, never replaced.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        fs::create_dir_all(root.join(".git")).expect("mkdir .git");
        let mut config = crate::config::default_for_root(&root);
        config.scan.exclude = vec!["**/mycustom/**".to_string()];
        let filter = IndexFilter::new(&root, &config).expect("build filter");

        // Floor entries are pruned despite not being in the user's narrowed list.
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
        // The user's own pattern is honored too (added on top of the floor).
        assert!(!filter.allows_glob("mycustom/thing.rs"), "user exclude honored");
        // A normal source file survives.
        assert!(filter.allows_glob("src/lib.rs"), "real source file kept");
    }

    #[test]
    fn should_reject_out_of_root_and_empty_rel() {
        let (filter, root, _tmp) = filter_for(|root| {
            fs::write(root.join("a.rs"), b"fn e() {}\n").unwrap();
        });
        // The watched root itself (empty rel) — the FSEvents coalescing case — is not indexable.
        assert!(!filter.is_indexable(&root));
        // A path outside the root is rejected.
        assert!(!filter.is_indexable(Path::new("/definitely/not/under/root.rs")));
    }
}
