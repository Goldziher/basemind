//! Per-language module-specifier resolution: map an [`ImportEdge`] to the repo-relative target
//! file that satisfies it.
//!
//! The cross-file stitch ([`crate::intel::xfile`]) resolves an importer's module specifier to a
//! target file, then joins the imported name against that file's exports. Historically the resolve
//! step was hardcoded to JS/TS via [`oxc_resolver`]. This module generalizes that step behind a
//! [`SpecifierResolver`] chosen by the importer's language, so Python and Java imports can resolve
//! to their target files too.
//!
//! Each variant is feature-gated to the backend it needs:
//!
//! - [`SpecifierResolver::Js`] (feature `code-intel-js`) — Node/tsconfig-style resolution via
//!   `oxc_resolver`. The single source of truth for the oxc config the whole crate uses.
//! - [`SpecifierResolver::Python`] / [`SpecifierResolver::Java`] (feature `code-intel-stack`) —
//!   pure path-arithmetic resolvers over conventional package/source-root layouts.
//!
//! All resolvers are conservative: an ambiguous or unrecognized specifier resolves to `None` rather
//! than a wrong guess. A miss simply leaves that import unstitched (the same fallback JS had for
//! non-JS files before this generalization).

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
use std::path::Path;

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
use crate::intel::model::ImportEdge;
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
use crate::path::RelPath;

/// Every repo-relative path a resolver consulted while resolving one importer's imports: hit
/// targets, failed probe candidates, and the config files (`tsconfig.json`, `package.json`) it read.
///
/// This is the dependency set the incremental re-stitch is exact over: the importer's result can
/// only change when one of these paths is added, deleted or modified. Directories need no entry — a
/// directory appearing or vanishing is always a file in it appearing or vanishing, and the resolver
/// decides through the files it probes.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
#[derive(Default)]
pub(crate) struct ProbeSet(ahash::AHashSet<String>);

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
impl ProbeSet {
    pub(crate) fn insert_rel(&mut self, rel: &RelPath) {
        if let Some(path) = rel.as_str().filter(|p| !p.is_empty()) {
            self.0.insert(path.to_string());
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The probes as sorted, distinct hashes — the rows `importers_by_probe` stores.
    pub(crate) fn hashes(&self) -> Vec<crate::index::keys::ProbeHash> {
        let mut hashes: Vec<_> = self.0.iter().map(|p| crate::index::keys::probe_hash(p)).collect();
        hashes.sort_unstable();
        hashes.dedup();
        hashes
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, rel: &str) -> bool {
        self.0.contains(rel)
    }
}

/// A per-language resolver for import module specifiers. Pick the variant with [`for_language`];
/// then call [`resolve`](SpecifierResolver::resolve) to map an [`ImportEdge`] to its target file.
///
/// The enum is `#[non_exhaustive]`-in-spirit through feature gates: which variants exist depends on
/// the enabled features, and a language with no compiled-in resolver yields `None` from
/// [`for_language`], so its imports are skipped by the stitch.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
pub(crate) enum SpecifierResolver {
    /// JavaScript/TypeScript (Node/tsconfig resolution via oxc). Feature `code-intel-js`. Boxed —
    /// the oxc `Resolver` it wraps dwarfs the unit-struct Python/Java variants, so an inline field
    /// would bloat every enum value when the stack variants are also compiled in (clippy
    /// `large_enum_variant`).
    #[cfg(feature = "code-intel-js")]
    Js(Box<js::JsResolver>),
    /// Python dotted / relative import resolution. Feature `code-intel-stack`.
    #[cfg(feature = "code-intel-stack")]
    Python(python::PythonResolver),
    /// Java fully-qualified-name resolution. Feature `code-intel-stack`.
    #[cfg(feature = "code-intel-stack")]
    Java(java::JavaResolver),
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
impl SpecifierResolver {
    /// Build the resolver for `language` (a TSLP pack name, e.g. `"typescript"`, `"python"`,
    /// `"java"`), or `None` if no resolver is compiled in for it. Building is cheap for the
    /// path-based resolvers; the JS variant constructs an `oxc_resolver` — hoist it out of the
    /// importer loop and reuse it across all importers of the same language.
    ///
    /// To merely ask *whether* a language is resolvable, use [`supports`](Self::supports): it
    /// answers without constructing anything.
    pub(crate) fn for_language(language: &str) -> Option<Self> {
        match language {
            #[cfg(feature = "code-intel-js")]
            "javascript" | "typescript" | "tsx" => Some(Self::Js(Box::new(js::JsResolver::new()))),
            #[cfg(feature = "code-intel-stack")]
            "python" => Some(Self::Python(python::PythonResolver)),
            #[cfg(feature = "code-intel-stack")]
            "java" => Some(Self::Java(java::JavaResolver)),
            _ => None,
        }
    }

    /// Whether a resolver is compiled in for `language` — the capability check, with no resolver
    /// construction. The incremental pass tests this once per indexed file, so it must stay free.
    pub(crate) fn supports(language: &str) -> bool {
        match language {
            #[cfg(feature = "code-intel-js")]
            "javascript" | "typescript" | "tsx" => true,
            #[cfg(feature = "code-intel-stack")]
            "python" | "java" => true,
            _ => false,
        }
    }

    /// Start resolving a new importer. The JS resolver drops its file-system cache here so that
    /// every path it consults for this importer reaches the probe recorder again instead of being
    /// served from a cache entry an earlier importer filled; the path-arithmetic resolvers have no
    /// cache. Call once per importer, before its first [`resolve`](Self::resolve).
    pub(crate) fn begin_importer(&self) {
        match self {
            #[cfg(feature = "code-intel-js")]
            Self::Js(resolver) => resolver.begin_importer(),
            #[cfg(feature = "code-intel-stack")]
            Self::Python(_) | Self::Java(_) => {}
        }
    }

    /// Resolve `import`'s module specifier, relative to `importer_rel` (a repo-relative forward-
    /// slashed path), to the repo-relative target file under `root`. Returns `None` on any miss —
    /// unresolvable specifier, target outside the repo, or a path the resolver declines to guess.
    /// Every path consulted, on a hit or a miss, is added to `probes`.
    ///
    /// The caller still validates the target is an indexed file and performs the name→export join;
    /// this method only answers "which file does this specifier point at".
    pub(crate) fn resolve(
        &self,
        root: &Path,
        importer_rel: &str,
        import: &ImportEdge,
        probes: &mut ProbeSet,
    ) -> Option<RelPath> {
        match self {
            #[cfg(feature = "code-intel-js")]
            Self::Js(resolver) => resolver.resolve(root, importer_rel, import, probes),
            #[cfg(feature = "code-intel-stack")]
            Self::Python(resolver) => resolver.resolve(root, importer_rel, import, probes),
            #[cfg(feature = "code-intel-stack")]
            Self::Java(resolver) => resolver.resolve(root, importer_rel, import, probes),
        }
    }

    /// The export name an import with no specific `imported` symbol binds to. JS/TS default imports
    /// (`import x from 'm'`) bind the target's `default` export, so `Js` returns `Some("default")`.
    /// A Python `import m` or Java `import a.b.C` with `imported == None` names no single export — a
    /// module (not one symbol) — so `Python`/`Java` return `None` and the join skips it. Without this
    /// distinction the join would fall back to `"default"` for every language and wrongly bind a bare
    /// Python `import m` to an unrelated `default = ...` / `def default` in the target.
    pub(crate) fn default_export_name(&self) -> Option<&'static str> {
        match self {
            #[cfg(feature = "code-intel-js")]
            Self::Js(_) => Some("default"),
            #[cfg(feature = "code-intel-stack")]
            Self::Python(_) | Self::Java(_) => None,
        }
    }
}

/// Convert an absolute path under `root` to a repo-relative [`RelPath`] (forward-slashed to match
/// the scanner's key convention). Returns `None` for paths outside `root` or non-UTF-8 paths.
/// Shared by the resolver variants.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
fn to_repo_relative(root: &Path, target_abs: &Path) -> Option<RelPath> {
    let rel = match target_abs.strip_prefix(root) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => {
            let canonical_root = std::fs::canonicalize(root).ok()?;
            target_abs.strip_prefix(&canonical_root).ok()?.to_path_buf()
        }
    };
    let normalized = rel.to_str()?.replace('\\', "/");
    Some(RelPath::from(normalized.as_str()))
}

/// Record `candidate` as consulted and return its repo-relative path when it is a regular file.
/// Used by the path-arithmetic resolvers, whose every probe is an `is_file` on a computed candidate.
#[cfg(feature = "code-intel-stack")]
fn probe_file(root: &Path, candidate: &Path, probes: &mut ProbeSet) -> Option<RelPath> {
    let rel = to_repo_relative(root, candidate)?;
    probes.insert_rel(&rel);
    candidate.is_file().then_some(rel)
}

#[cfg(feature = "code-intel-js")]
pub(crate) mod js {
    //! JS/TS specifier resolution via `oxc_resolver` — the single source of truth for the oxc
    //! module-resolution config the crate uses.
    //!
    //! ## tsconfig path aliases
    //!
    //! A monorepo rarely imports by relative path. It writes `@app/hooks/useSettings`, and the
    //! mapping from that alias to a file lives in a `tsconfig.json` (`compilerOptions.baseUrl` +
    //! `paths`). With no tsconfig in play, oxc cannot resolve such a specifier, the cross-file stitch
    //! emits no edge, and `goto_definition` degrades to the local import binding — a same-file answer
    //! to a cross-file question.
    //!
    //! So the resolver runs with [`TsconfigDiscovery::Auto`] and resolves against the importing
    //! **file** (`resolve_file`, the only API `Auto` applies to). oxc then walks up from that file to
    //! the nearest `tsconfig.json` that actually *claims* it (via `files` / `include` / `exclude`, or
    //! a matching project reference) and applies its `baseUrl` / `paths` — the same config `tsc`
    //! would apply, including solution-style monorepo references. A file no tsconfig claims gets
    //! plain Node resolution rather than an unrelated ancestor's `compilerOptions`, matching
    //! `tsserver`.
    //!
    //! Resolution stays entirely oxc's: an alias resolves exactly when TypeScript would resolve it,
    //! and to the same file. A specifier still only resolves when the target exists on disk, so an
    //! alias pointing at nothing stays unresolved — a miss, never a guessed target.

    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, OnceLock};

    use oxc_resolver::{
        FileMetadata, FileSystem, FileSystemOs, ResolveError, ResolveOptions, ResolverGeneric, TsconfigDiscovery,
    };

    use super::{ProbeSet, to_repo_relative};
    use crate::intel::model::ImportEdge;
    use crate::path::RelPath;

    /// Paths the file system was asked about, in call order. Shared between the resolver's
    /// [`RecordingFs`] and the [`JsResolver`] that drains it.
    #[derive(Clone, Default)]
    struct ProbeLog(Arc<Mutex<Vec<PathBuf>>>);

    impl ProbeLog {
        fn note(&self, path: &Path) {
            if let Ok(mut log) = self.0.lock() {
                log.push(path.to_path_buf());
            }
        }

        fn drain(&self) -> Vec<PathBuf> {
            self.0
                .lock()
                .map(|mut log| std::mem::take(&mut *log))
                .unwrap_or_default()
        }
    }

    /// oxc's real file system, with every consulted path recorded. oxc reaches the disk only through
    /// this trait (`metadata` for existence probes, `read*` for `package.json` / `tsconfig.json`),
    /// so the log is the complete input set of a resolution. A successful directory `metadata` is
    /// not logged: directories carry no state beyond the files in them, and those are logged.
    struct RecordingFs {
        inner: FileSystemOs,
        log: ProbeLog,
    }

    impl RecordingFs {
        fn with_log(log: ProbeLog) -> Self {
            Self {
                inner: <FileSystemOs as FileSystem>::new(),
                log,
            }
        }

        fn note_unless_dir(&self, path: &Path, result: &io::Result<FileMetadata>) {
            if !result.as_ref().is_ok_and(|meta| meta.is_dir()) {
                self.log.note(path);
            }
        }
    }

    impl FileSystem for RecordingFs {
        fn new() -> Self {
            Self::with_log(ProbeLog::default())
        }

        fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
            self.log.note(path);
            FileSystem::read(&self.inner, path)
        }

        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            self.log.note(path);
            FileSystem::read_to_string(&self.inner, path)
        }

        fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
            let result = FileSystem::metadata(&self.inner, path);
            self.note_unless_dir(path, &result);
            result
        }

        fn symlink_metadata(&self, path: &Path) -> io::Result<FileMetadata> {
            let result = FileSystem::symlink_metadata(&self.inner, path);
            self.note_unless_dir(path, &result);
            result
        }

        fn read_link(&self, path: &Path) -> Result<PathBuf, ResolveError> {
            self.log.note(path);
            FileSystem::read_link(&self.inner, path)
        }

        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            self.log.note(path);
            FileSystem::canonicalize(&self.inner, path)
        }
    }

    /// JS/TS module-resolution extensions, TS-first so a bare `./util` specifier binds to `util.ts`
    /// before `util.js` (matching `tsc`'s module-resolution precedence).
    pub(crate) const RESOLVE_EXTENSIONS: &[&str] = &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];

    /// TS-aware Node resolver wrapping [`oxc_resolver`]. Built once per stitch and reused across all
    /// JS/TS importers — construction is expensive (it seeds oxc's file-system cache), and that cache
    /// is also what makes the per-file tsconfig discovery cheap, so never build one per file.
    pub(crate) struct JsResolver {
        inner: ResolverGeneric<RecordingFs>,
        log: ProbeLog,
        /// The canonical form of the repo root, computed on first use: oxc canonicalises some
        /// paths (a tsconfig's `extends` chain), and on a symlinked root those come back under it.
        canonical_root: OnceLock<Option<PathBuf>>,
    }

    impl JsResolver {
        /// Construct the resolver. Configured for TS-aware Node resolution: TS extensions win, a
        /// `./util.js` specifier maps back to `util.ts` (TS's rewritten-extension convention), the
        /// standard import/require conditions are enabled for `package.json` `exports` maps, and
        /// tsconfig `baseUrl` / `paths` are discovered per importing file (see the module docs).
        pub(crate) fn new() -> Self {
            let ext_alias = |from: &str, to: &[&str]| (from.to_string(), to.iter().map(|s| (*s).to_string()).collect());
            let log = ProbeLog::default();
            let inner = ResolverGeneric::new_with_file_system(
                RecordingFs::with_log(log.clone()),
                ResolveOptions {
                    extensions: RESOLVE_EXTENSIONS.iter().map(|e| (*e).to_string()).collect(),
                    extension_alias: vec![
                        ext_alias(".js", &[".ts", ".tsx", ".js", ".jsx"]),
                        ext_alias(".mjs", &[".mts", ".mjs"]),
                        ext_alias(".cjs", &[".cts", ".cjs"]),
                    ],
                    condition_names: vec![
                        "node".to_string(),
                        "import".to_string(),
                        "require".to_string(),
                        "default".to_string(),
                    ],
                    symlinks: false,
                    tsconfig: Some(TsconfigDiscovery::Auto),
                    ..ResolveOptions::default()
                },
            );
            Self {
                inner,
                log,
                canonical_root: OnceLock::new(),
            }
        }

        /// Forget everything oxc has cached and everything recorded so far. oxc memoises every stat
        /// and parsed config, so without this a second importer would hit the cache for the paths
        /// the first one consulted and its probes would never reach the log.
        pub(crate) fn begin_importer(&self) {
            self.inner.clear_cache();
            drop(self.log.drain());
        }

        /// Resolve `import`'s specifier for the importing file `importer_rel` to a repo-relative
        /// target under `root`. `None` on any oxc miss or out-of-repo path.
        ///
        /// Resolves against the **file** (not its directory): `TsconfigDiscovery::Auto` only applies
        /// to `resolve_file`, and it is what lets an aliased specifier find the tsconfig that governs
        /// this importer.
        pub(crate) fn resolve(
            &self,
            root: &Path,
            importer_rel: &str,
            import: &ImportEdge,
            probes: &mut ProbeSet,
        ) -> Option<RelPath> {
            let importer_abs = root.join(importer_rel);
            let resolved = self.inner.resolve_file(&importer_abs, &import.specifier);
            self.drain_into(root, probes);
            to_repo_relative(root, &resolved.ok()?.full_path())
        }

        /// Move the recorded paths that lie under the repo into `probes`. Paths above the root
        /// (the ancestors oxc walks looking for a `package.json`) are not repo state and are dropped.
        fn drain_into(&self, root: &Path, probes: &mut ProbeSet) {
            let canonical = self
                .canonical_root
                .get_or_init(|| std::fs::canonicalize(root).ok())
                .as_deref();
            for abs in self.log.drain() {
                let rel = abs
                    .strip_prefix(root)
                    .ok()
                    .or_else(|| canonical.and_then(|c| abs.strip_prefix(c).ok()))
                    .and_then(Path::to_str);
                if let Some(rel) = rel {
                    probes.insert_rel(&RelPath::from(rel.replace('\\', "/").as_str()));
                }
            }
        }
    }
}

#[cfg(feature = "code-intel-stack")]
pub(crate) mod python {
    //! Python import-specifier resolution over conventional package layouts.
    //!
    //! Handles the two shapes the oxc analysis's Python counterpart emits:
    //!
    //! - **Absolute dotted** (`import foo.bar`, `from foo.bar import x`): the `specifier` is the
    //!   dotted module path `foo.bar`, resolved under a package root (repo root, and `src/` if it
    //!   exists) to `foo/bar.py` or `foo/bar/__init__.py`.
    //! - **Relative** (`from . import x`, `from .mod import y`, `from ..pkg import z`): the
    //!   `specifier` carries the leading dots. Each leading dot climbs one package directory from
    //!   the importer's own package (the importer's directory), then the remaining dotted tail
    //!   resolves under that directory.
    //!
    //! Conservative: a specifier that climbs above `root`, or resolves to no on-disk file, yields
    //! `None`.

    use std::path::{Path, PathBuf};

    use super::{ProbeSet, probe_file};
    use crate::intel::model::ImportEdge;
    use crate::path::RelPath;

    /// Package roots searched for absolute dotted imports, most-specific first. `src/` is a common
    /// layout (`src`-layout packaging); the repo root covers flat layouts.
    const PACKAGE_ROOTS: &[&str] = &["src", ""];

    pub(crate) struct PythonResolver;

    impl PythonResolver {
        /// Resolve `import`'s dotted-or-relative specifier to a repo-relative `.py` file.
        pub(crate) fn resolve(
            &self,
            root: &Path,
            importer_rel: &str,
            import: &ImportEdge,
            probes: &mut ProbeSet,
        ) -> Option<RelPath> {
            let specifier = import.specifier.as_str();
            let dots = specifier.chars().take_while(|&c| c == '.').count();
            if dots > 0 {
                self.resolve_relative(root, importer_rel, specifier, dots, probes)
            } else {
                self.resolve_absolute(root, specifier, probes)
            }
        }

        /// Absolute dotted import: try each package root, mapping `foo.bar` → `foo/bar`.
        fn resolve_absolute(&self, root: &Path, specifier: &str, probes: &mut ProbeSet) -> Option<RelPath> {
            let rel_parts = dotted_to_parts(specifier)?;
            for pkg_root in PACKAGE_ROOTS {
                let mut base = root.to_path_buf();
                if !pkg_root.is_empty() {
                    base.push(pkg_root);
                }
                if let Some(hit) = module_file(root, &base, &rel_parts, probes) {
                    return Some(hit);
                }
            }
            None
        }

        /// Relative import: climb `dots` package levels from the importer's directory, then descend
        /// through the dotted tail after the dots.
        fn resolve_relative(
            &self,
            root: &Path,
            importer_rel: &str,
            specifier: &str,
            dots: usize,
            probes: &mut ProbeSet,
        ) -> Option<RelPath> {
            let importer_abs = root.join(importer_rel);
            let mut base = importer_abs.parent()?.to_path_buf();
            for _ in 1..dots {
                base = base.parent()?.to_path_buf();
                if !base.starts_with(root) {
                    return None;
                }
            }
            let tail = &specifier[dots..];
            let parts = if tail.is_empty() {
                Vec::new()
            } else {
                dotted_to_parts(tail)?
            };
            module_file(root, &base, &parts, probes)
        }
    }

    /// Split a dotted module path into path components, rejecting empty segments (a malformed
    /// specifier like `foo..bar`).
    fn dotted_to_parts(dotted: &str) -> Option<Vec<&str>> {
        let parts: Vec<&str> = dotted.split('.').collect();
        if parts.iter().any(|p| p.is_empty()) {
            return None;
        }
        Some(parts)
    }

    /// Given a package `base` directory and the module `parts` under it, return the repo-relative
    /// path of `<base>/<parts...>.py` or `<base>/<parts...>/__init__.py`, whichever exists. Empty
    /// `parts` (a bare `from . import x`) resolves to the package's own `__init__.py`.
    ///
    /// Both candidates are recorded in `probes` before either is tested, so a module that appears
    /// later (`pkg/mod.py` shadowing `pkg/mod/__init__.py`, or the first `__init__.py` of a package)
    /// is found by the importers that missed it. A missing directory needs no separate probe: the
    /// files under it do not exist either.
    fn module_file(root: &Path, base: &Path, parts: &[&str], probes: &mut ProbeSet) -> Option<RelPath> {
        let mut dir = base.to_path_buf();
        if parts.is_empty() {
            return probe_file(root, &dir.join("__init__.py"), probes);
        }
        for part in &parts[..parts.len() - 1] {
            dir.push(part);
        }
        let last = parts[parts.len() - 1];
        let module: PathBuf = dir.join(format!("{last}.py"));
        if let Some(hit) = probe_file(root, &module, probes) {
            return Some(hit);
        }
        probe_file(root, &dir.join(last).join("__init__.py"), probes)
    }
}

#[cfg(feature = "code-intel-stack")]
pub(crate) mod java {
    //! Java fully-qualified-name resolution over conventional Maven/Gradle source roots.
    //!
    //! A Java import `com.example.Foo` names a public type `Foo` in package `com.example`, which by
    //! convention lives at `<source-root>/com/example/Foo.java`. We try each known source root
    //! (repo root plus the standard Maven/Gradle roots) and return the first `.java` file that
    //! exists on disk. Wildcard imports (`com.example.*`) resolve to `None` (no single target).
    //!
    //! Known gap: **static imports** (`import static a.b.C.method;`) are not distinguished from type
    //! imports — the trailing member is treated as a type name, so the resolver builds a
    //! `.../C/method.java` path that does not exist and returns `None`. Static imports therefore get
    //! no cross-file resolution today (a miss, never a wrong answer).

    use std::path::Path;

    use super::{ProbeSet, probe_file};
    use crate::intel::model::ImportEdge;
    use crate::path::RelPath;

    /// Source roots searched for a fully-qualified type, most-specific first. Covers the Maven/
    /// Gradle standard directory layout plus a bare `src/` and the repo root (flat / single-module).
    const SOURCE_ROOTS: &[&str] = &["src/main/java", "src/test/java", "src", ""];

    pub(crate) struct JavaResolver;

    impl JavaResolver {
        /// Resolve `import`'s fully-qualified name (`com.example.Foo`) to `<root>/com/example/Foo.java`
        /// under some source root. `None` for wildcard imports or when no matching file exists.
        pub(crate) fn resolve(
            &self,
            root: &Path,
            _importer_rel: &str,
            import: &ImportEdge,
            probes: &mut ProbeSet,
        ) -> Option<RelPath> {
            let fqn = import.specifier.as_str();
            let parts: Vec<&str> = fqn.split('.').collect();
            if parts.len() < 2 || parts.iter().any(|p| p.is_empty() || *p == "*") {
                return None;
            }
            let rel_java = format!("{}.java", parts.join("/"));
            for source_root in SOURCE_ROOTS {
                let mut base = root.to_path_buf();
                for segment in source_root.split('/').filter(|segment| !segment.is_empty()) {
                    base.push(segment);
                }
                if let Some(hit) = probe_file(root, &base.join(&rel_java), probes) {
                    return Some(hit);
                }
            }
            None
        }
    }
}

#[cfg(all(test, feature = "code-intel-stack"))]
mod stack_tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::intel::model::ImportEdge;

    fn import(specifier: &str, imported: Option<&str>) -> ImportEdge {
        ImportEdge {
            local: imported.unwrap_or("mod").to_string(),
            specifier: specifier.to_string(),
            imported: imported.map(str::to_string),
            is_type: false,
            local_start: 0,
        }
    }

    /// Resolve without caring which paths were probed.
    fn resolve_unprobed(
        resolver: &SpecifierResolver,
        root: &Path,
        importer_rel: &str,
        import: &ImportEdge,
    ) -> Option<RelPath> {
        resolver.resolve(root, importer_rel, import, &mut ProbeSet::default())
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let abs = dir.join(rel);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(abs, body).unwrap();
    }

    #[test]
    fn python_absolute_module_resolves_to_py_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "foo/bar.py", "x = 1\n");
        write(root, "foo/__init__.py", "");
        let resolver = SpecifierResolver::for_language("python").expect("python resolver");
        let got = resolve_unprobed(&resolver, root, "app/main.py", &import("foo.bar", Some("x")));
        assert_eq!(
            got.and_then(|r| r.as_str().map(str::to_string)),
            Some("foo/bar.py".to_string())
        );
    }

    #[test]
    fn python_absolute_module_resolves_to_package_init() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "pkg/__init__.py", "y = 2\n");
        let resolver = SpecifierResolver::for_language("python").unwrap();
        let got = resolve_unprobed(&resolver, root, "app/main.py", &import("pkg", Some("y")));
        assert_eq!(
            got.and_then(|r| r.as_str().map(str::to_string)),
            Some("pkg/__init__.py".to_string())
        );
    }

    #[test]
    fn python_absolute_module_prefers_src_layout() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "src/mypkg/util.py", "z = 3\n");
        let resolver = SpecifierResolver::for_language("python").unwrap();
        let got = resolve_unprobed(&resolver, root, "src/mypkg/main.py", &import("mypkg.util", Some("z")));
        assert_eq!(
            got.and_then(|r| r.as_str().map(str::to_string)),
            Some("src/mypkg/util.py".to_string())
        );
    }

    #[test]
    fn python_relative_single_dot_resolves_sibling() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "pkg/sibling.py", "a = 1\n");
        write(root, "pkg/main.py", "");
        let resolver = SpecifierResolver::for_language("python").unwrap();
        let got = resolve_unprobed(&resolver, root, "pkg/main.py", &import(".sibling", Some("a")));
        assert_eq!(
            got.and_then(|r| r.as_str().map(str::to_string)),
            Some("pkg/sibling.py".to_string())
        );
    }

    #[test]
    fn python_relative_bare_dot_resolves_package_init() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "pkg/__init__.py", "b = 1\n");
        write(root, "pkg/main.py", "");
        let resolver = SpecifierResolver::for_language("python").unwrap();
        let got = resolve_unprobed(&resolver, root, "pkg/main.py", &import(".", Some("b")));
        assert_eq!(
            got.and_then(|r| r.as_str().map(str::to_string)),
            Some("pkg/__init__.py".to_string())
        );
    }

    #[test]
    fn python_relative_double_dot_climbs_package() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "pkg/other.py", "c = 1\n");
        write(root, "pkg/sub/main.py", "");
        let resolver = SpecifierResolver::for_language("python").unwrap();
        let got = resolve_unprobed(&resolver, root, "pkg/sub/main.py", &import("..other", Some("c")));
        assert_eq!(
            got.and_then(|r| r.as_str().map(str::to_string)),
            Some("pkg/other.py".to_string())
        );
    }

    #[test]
    fn python_relative_cannot_climb_above_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "main.py", "");
        let resolver = SpecifierResolver::for_language("python").unwrap();
        let got = resolve_unprobed(&resolver, root, "main.py", &import("..x", Some("x")));
        assert_eq!(got, None);
    }

    #[test]
    fn python_missing_module_resolves_none() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let resolver = SpecifierResolver::for_language("python").unwrap();
        let got = resolve_unprobed(&resolver, root, "main.py", &import("nope.gone", Some("x")));
        assert_eq!(got, None);
    }

    #[test]
    fn java_fqn_resolves_under_maven_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            root,
            "src/main/java/com/example/Foo.java",
            "package com.example;\nclass Foo {}\n",
        );
        let resolver = SpecifierResolver::for_language("java").expect("java resolver");
        let got = resolve_unprobed(
            &resolver,
            root,
            "src/main/java/com/example/Bar.java",
            &import("com.example.Foo", None),
        );
        assert_eq!(
            got.and_then(|r| r.as_str().map(str::to_string)),
            Some("src/main/java/com/example/Foo.java".to_string())
        );
    }

    #[test]
    fn java_fqn_resolves_at_repo_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "com/example/Baz.java", "package com.example;\nclass Baz {}\n");
        let resolver = SpecifierResolver::for_language("java").unwrap();
        let got = resolve_unprobed(
            &resolver,
            root,
            "com/example/Main.java",
            &import("com.example.Baz", None),
        );
        assert_eq!(
            got.and_then(|r| r.as_str().map(str::to_string)),
            Some("com/example/Baz.java".to_string())
        );
    }

    #[test]
    fn java_wildcard_import_resolves_none() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "src/main/java/com/example/Foo.java", "class Foo {}\n");
        let resolver = SpecifierResolver::for_language("java").unwrap();
        let got = resolve_unprobed(
            &resolver,
            root,
            "src/main/java/com/example/Bar.java",
            &import("com.example.*", None),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn java_missing_type_resolves_none() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let resolver = SpecifierResolver::for_language("java").unwrap();
        let got = resolve_unprobed(&resolver, root, "Main.java", &import("com.example.Gone", None));
        assert_eq!(got, None);
    }

    fn probes_of(resolver: &SpecifierResolver, root: &Path, importer: &str, import: &ImportEdge) -> ProbeSet {
        let mut probes = ProbeSet::default();
        resolver.resolve(root, importer, import, &mut probes);
        probes
    }

    #[test]
    fn python_miss_records_every_candidate_it_tried() {
        let tmp = TempDir::new().unwrap();
        let resolver = SpecifierResolver::for_language("python").unwrap();
        let probes = probes_of(&resolver, tmp.path(), "main.py", &import("nope.gone", Some("x")));
        for candidate in [
            "src/nope/gone.py",
            "src/nope/gone/__init__.py",
            "nope/gone.py",
            "nope/gone/__init__.py",
        ] {
            assert!(probes.contains(candidate), "{candidate} must be a recorded probe");
        }
    }

    /// The module file beats the package, so the package hit alone is not the dependency: the
    /// importer must also be found by the `pkg/mod.py` that would shadow it.
    #[test]
    fn python_package_hit_also_records_the_shadowing_module_candidate() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "pkg/mod/__init__.py", "x = 1\n");
        let resolver = SpecifierResolver::for_language("python").unwrap();
        let import = import("pkg.mod", Some("x"));
        let probes = probes_of(&resolver, tmp.path(), "main.py", &import);
        assert!(probes.contains("pkg/mod.py"));
        assert!(probes.contains("pkg/mod/__init__.py"));
    }

    #[test]
    fn java_miss_records_a_candidate_per_source_root() {
        let tmp = TempDir::new().unwrap();
        let resolver = SpecifierResolver::for_language("java").unwrap();
        let probes = probes_of(&resolver, tmp.path(), "Main.java", &import("com.example.Gone", None));
        for candidate in [
            "src/main/java/com/example/Gone.java",
            "src/test/java/com/example/Gone.java",
            "src/com/example/Gone.java",
            "com/example/Gone.java",
        ] {
            assert!(probes.contains(candidate), "{candidate} must be a recorded probe");
        }
    }

    #[test]
    fn unknown_language_has_no_resolver() {
        assert!(SpecifierResolver::for_language("cobol").is_none());
        assert!(!SpecifierResolver::supports("cobol"));
    }

    #[test]
    fn python_java_imports_have_no_default_export_fallback() {
        assert_eq!(
            SpecifierResolver::for_language("python").unwrap().default_export_name(),
            None,
            "Python has no default-export convention"
        );
        assert_eq!(
            SpecifierResolver::for_language("java").unwrap().default_export_name(),
            None,
            "Java has no default-export convention"
        );
    }
}

#[cfg(all(test, feature = "code-intel-js"))]
mod js_tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn import(specifier: &str) -> ImportEdge {
        ImportEdge {
            local: "x".to_string(),
            specifier: specifier.to_string(),
            imported: Some("x".to_string()),
            is_type: false,
            local_start: 0,
        }
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let abs = dir.join(rel);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(abs, body).unwrap();
    }

    fn resolve(
        resolver: &SpecifierResolver,
        root: &Path,
        importer: &str,
        specifier: &str,
    ) -> (Option<RelPath>, ProbeSet) {
        let mut probes = ProbeSet::default();
        let hit = resolver.resolve(root, importer, &import(specifier), &mut probes);
        (hit, probes)
    }

    /// A TS-first resolver that found `util.js` must still depend on the `util.ts` it tried first:
    /// adding that file shadows the hit.
    #[test]
    fn hit_and_the_higher_precedence_misses_are_recorded() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "util.js", "export const x = 1;\n");
        write(tmp.path(), "app.ts", "");
        let resolver = SpecifierResolver::for_language("typescript").unwrap();
        resolver.begin_importer();
        let (hit, probes) = resolve(&resolver, tmp.path(), "app.ts", "./util");
        assert_eq!(
            hit.and_then(|r| r.as_str().map(str::to_string)),
            Some("util.js".to_string())
        );
        assert!(probes.contains("util.ts"), "the shadowing candidate must be recorded");
        assert!(probes.contains("util.js"), "the hit must be recorded");
    }

    /// A tsconfig is state the result depends on: its path must be recorded so an edit re-stitches.
    #[test]
    fn the_tsconfig_that_governed_the_resolution_is_recorded() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib/*":["lib/*"]}}}"#,
        );
        write(tmp.path(), "lib/thing.ts", "export const x = 1;\n");
        write(tmp.path(), "src/app.ts", "");
        let resolver = SpecifierResolver::for_language("typescript").unwrap();
        resolver.begin_importer();
        let (hit, probes) = resolve(&resolver, tmp.path(), "src/app.ts", "@lib/thing");
        assert_eq!(
            hit.and_then(|r| r.as_str().map(str::to_string)),
            Some("lib/thing.ts".to_string())
        );
        assert!(probes.contains("tsconfig.json"));
        assert!(probes.contains("lib/thing.ts"));
    }

    /// oxc memoises every stat. Without `begin_importer` a second importer's probes would silently
    /// shrink to what the first one left uncached; with it they are complete again.
    #[test]
    fn begin_importer_makes_a_second_importers_probes_complete() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "util.ts", "export const x = 1;\n");
        write(tmp.path(), "a.ts", "");
        write(tmp.path(), "b.ts", "");
        let resolver = SpecifierResolver::for_language("typescript").unwrap();

        resolver.begin_importer();
        let (_, first) = resolve(&resolver, tmp.path(), "a.ts", "./util");
        let (_, cached) = resolve(&resolver, tmp.path(), "b.ts", "./util");
        assert!(first.contains("util.ts"));
        assert!(
            !cached.contains("util.ts"),
            "the cache must hide the probe, or this test proves nothing"
        );

        resolver.begin_importer();
        let (_, second) = resolve(&resolver, tmp.path(), "b.ts", "./util");
        assert!(second.contains("util.ts"));
    }

    /// A specifier that resolves to nothing is the case probes exist for: the candidates it failed
    /// on are the only way a later-added file can find this importer.
    #[test]
    fn an_unresolved_specifier_still_records_what_it_tried() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "app.ts", "");
        let resolver = SpecifierResolver::for_language("typescript").unwrap();
        resolver.begin_importer();
        let (hit, probes) = resolve(&resolver, tmp.path(), "app.ts", "./missing");
        assert_eq!(hit, None);
        assert!(probes.contains("missing.ts"));
        assert!(
            probes.contains("missing"),
            "a directory candidate must be recorded for later creation"
        );
    }
}
