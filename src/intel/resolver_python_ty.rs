//! Python import resolution through ty's module resolver (`ty_module_resolver`, feature
//! `code-intel-python`), tried before the path arithmetic of [`super::python`].
//!
//! ty resolves an import the way a type checker does: search-path order, the `src` layout,
//! regular and namespace packages, stub files and stub-only (`-stubs`) packages, and relative
//! imports from the importer's own module name. When the configured roots miss, it searches the
//! importer's ancestor directories that are not packages, which is how a package nested deep in a
//! large monorepo (`services/billing/app/main.py` importing `app.models`) resolves with no
//! configured source root.
//!
//! The resolver reads an in-memory file system built from the index, not the disk: every indexed
//! `.py` and `.pyi` file, mounted under a virtual `/`, so a target is always an indexed file and
//! unindexed trees (virtual environments, ignored build output) never answer. The files are empty:
//! ty reads a file's content only to detect a legacy `pkgutil`/`pkg_resources` namespace package,
//! which it therefore treats as a regular package. The standard library is an empty custom
//! typeshed, so a standard-library import resolves to nothing and the caller falls back as before.
//! A runtime file wins over its stub (`foo.py` over `foo.pyi`); a stub resolves when it is the only
//! file.
//!
//! ty caches its queries for the life of the resolver (one stitch), so the paths it reads while
//! resolving one importer are not the paths that importer depends on. [`TyResolver::record_lookup`]
//! derives them from the module name and the search roots instead.
//!
//! `ty_module_resolver` is an internal component of ruff with no stable API: its version is pinned
//! exactly and every use of it stays in this module.

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use ruff_db::Db as _;
use ruff_db::files::{File, Files, system_path_to_file};
use ruff_db::system::{InMemorySystem, MemoryFileSystem, System, SystemPath, SystemPathBuf};
use ruff_db::vendored::VendoredFileSystem;
use ruff_python_ast::PythonVersion;
use ty_module_resolver::{
    FallibleStrategy, ImportingFile, Module, ModuleName, ResolverEnvironment, ResolverFile, SearchPathSettings,
    SearchPaths, file_to_module, resolve_module, resolve_real_module, resolve_real_module_confident,
};

use super::ProbeSet;
use crate::path::RelPath;

/// The virtual directory the repository is mounted at.
const ROOT: &str = "/";

/// The `src`-layout root, searched before [`ROOT`] when it holds Python files.
const SRC: &str = "/src";

/// The files that make a directory a regular package.
const INITS: [&str; 2] = ["__init__.py", "__init__.pyi"];

/// Where the empty custom typeshed lives. It sits under the root but starts with a dot, so no
/// import can name it.
const TYPESHED: &str = "/.basemind-ty-typeshed";

/// ty's module resolver over the indexed Python files, for one stitch.
pub(crate) struct TyResolver {
    db: ResolverDb,
    search_paths: SearchPaths,
    /// The first-party roots, in search order: `/src` when it holds Python files, then `/`.
    roots: Vec<SystemPathBuf>,
    /// Set when a ty query panicked; every later lookup then misses, so the caller's path
    /// arithmetic answers for the rest of the stitch.
    failed: Cell<bool>,
}

impl TyResolver {
    /// Build the resolver over `files`, the repo-relative paths of the indexed files (other than
    /// `.py` and `.pyi` files are skipped). `None` when ty rejects the search paths.
    pub(crate) fn new<'a>(files: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        let fs = MemoryFileSystem::with_current_directory(ROOT);
        for rel in files.into_iter().filter(|rel| is_python_source(rel)) {
            if let Err(error) = fs.write_file_all(virtual_path(rel), "") {
                tracing::debug!(%rel, %error, "ty resolver: file not mounted");
            }
        }
        let typeshed = SystemPathBuf::from(TYPESHED);
        fs.write_file_all(typeshed.join("stdlib/VERSIONS"), "").ok()?;

        let mut roots = Vec::with_capacity(2);
        if fs.is_directory(SRC) {
            roots.push(SystemPathBuf::from(SRC));
        }
        roots.push(SystemPathBuf::from(ROOT));
        let db = ResolverDb::new(InMemorySystem::from_memory_fs(fs));
        let mut settings = SearchPathSettings::new(roots.clone());
        settings.custom_typeshed = Some(typeshed);
        let search_paths = match settings.to_search_paths(db.system(), db.vendored(), &FallibleStrategy) {
            Ok(search_paths) => search_paths,
            Err(error) => {
                tracing::warn!(%error, "ty resolver: search paths rejected; Python imports resolve by path");
                return None;
            }
        };
        search_paths.try_register_static_roots(&db);
        Some(Self {
            db,
            search_paths,
            roots,
            failed: Cell::new(false),
        })
    }

    /// Resolve `specifier` (`a.b`, `.`, `..pkg.mod`) as imported by `importer_rel` to the
    /// repo-relative file of the module it names. `None` for a module with no indexed file: a
    /// namespace package, the standard library, a dependency, or nothing at all. Every path the
    /// answer depends on is added to `probes`.
    pub(crate) fn resolve(&self, importer_rel: &str, specifier: &str, probes: &mut ProbeSet) -> Option<RelPath> {
        self.guarded(|| {
            let level = specifier.bytes().take_while(|&byte| byte == b'.').count();
            let tail = Some(&specifier[level..]).filter(|tail| !tail.is_empty());
            let importer_path = virtual_path(importer_rel);
            let importer = system_path_to_file(&self.db, &importer_path).ok()?;
            let environment = self.environment();
            if level > 0 {
                // A relative import is anchored at the importer's own module name. ~keep
                let own = file_to_module(&self.db, ResolverFile::new(&self.db, importer, environment));
                if let Some(own) = own {
                    self.record_lookup(own.name(&self.db), &self.roots, probes);
                }
            }
            let importing = ImportingFile::File(importer, environment);
            let level = u32::try_from(level).ok()?;
            let name = ModuleName::from_identifier_parts(&self.db, importing, tail, level).ok()?;
            self.lookup(importer, &importer_path, &name, environment, probes)
        })
    }

    /// Resolve `name` as a submodule of the package whose `__init__` file is `package_rel`: the
    /// `views` of `from . import views`. `None` when `package_rel` is a plain module, which has no
    /// submodules, or the submodule has no indexed file.
    pub(crate) fn submodule(&self, package_rel: &str, name: &str, probes: &mut ProbeSet) -> Option<RelPath> {
        self.guarded(|| {
            let package_path = virtual_path(package_rel);
            let package = system_path_to_file(&self.db, &package_path).ok()?;
            let environment = self.environment();
            let module = file_to_module(&self.db, ResolverFile::new(&self.db, package, environment))?;
            self.record_lookup(module.name(&self.db), &self.roots, probes);
            if !module.kind(&self.db).is_package() {
                return None;
            }
            let submodule = module.name(&self.db).join(&ModuleName::new(name)?);
            self.lookup(package, &package_path, &submodule, environment, probes)
        })
    }

    /// The file `name` resolves to from `importer`: the runtime module first, then a stub-only one.
    fn lookup<'db>(
        &'db self,
        importer: File,
        importer_path: &SystemPath,
        name: &ModuleName,
        environment: ResolverEnvironment<'db>,
        probes: &mut ProbeSet,
    ) -> Option<RelPath> {
        self.record_lookup(name, &self.roots, probes);
        if resolve_real_module_confident(&self.db, environment, name).is_none() {
            // The configured roots miss, so ty searches the importer's non-package ancestors. ~keep
            let ancestors = self.ancestor_roots(importer_path, probes);
            self.record_lookup(name, &ancestors, probes);
        }
        let importing = ImportingFile::File(importer, environment);
        let file_of = |module: Module<'db>| module.file(&self.db);
        let file = resolve_real_module(&self.db, importing, name)
            .and_then(file_of)
            .or_else(|| resolve_module(&self.db, importing, name).and_then(file_of))?;
        let path = file.path(&self.db).as_system_path()?;
        (!path.starts_with(TYPESHED)).then(|| RelPath::from(repo_relative(path).as_str()))
    }

    /// The ancestor directories of `importer_path` that ty searches when the configured roots miss:
    /// those with no `__init__.py(i)`, closest first. The `__init__` candidates of every ancestor
    /// are recorded, since adding or removing one moves the search.
    fn ancestor_roots(&self, importer_path: &SystemPath, probes: &mut ProbeSet) -> Vec<SystemPathBuf> {
        let mut roots = Vec::new();
        for dir in importer_path.ancestors().skip(1) {
            let mut is_package = false;
            for init in INITS {
                let init = dir.join(init);
                probe_file(&init, probes);
                is_package |= self.fs().is_file(&init);
            }
            if !is_package {
                roots.push(dir.to_path_buf());
            }
        }
        roots
    }

    /// Record every path whose appearance, removal or change can alter how `name` resolves under
    /// `roots`. Per root: the stub-only package directory of the first component; then, component
    /// by component, the module files (`c.py`, `c.pyi`) and the package's `__init__.py(i)`, down
    /// to the first component that is not a directory, which is recorded as a directory so that
    /// any file appearing under it re-resolves the import.
    fn record_lookup(&self, name: &ModuleName, roots: &[SystemPathBuf], probes: &mut ProbeSet) {
        let first = name.first_component();
        for root in roots {
            probes.insert_dir(&repo_relative(&root.join(format!("{first}-stubs"))));
            let mut dir = root.clone();
            for component in name.components() {
                probe_file(&dir.join(format!("{component}.py")), probes);
                probe_file(&dir.join(format!("{component}.pyi")), probes);
                let package = dir.join(component);
                if !self.fs().is_directory(&package) {
                    probes.insert_dir(&repo_relative(&package));
                    break;
                }
                for init in INITS {
                    probe_file(&package.join(init), probes);
                }
                dir = package;
            }
        }
    }

    fn environment(&self) -> ResolverEnvironment<'_> {
        ResolverEnvironment::new(&self.db, PythonVersion::default(), &self.search_paths)
    }

    fn fs(&self) -> &MemoryFileSystem {
        self.db.system.fs()
    }

    /// Run a ty query, containing a panic in it: the crate is not built to be embedded, and one bad
    /// lookup must not take the scan down. After a panic every lookup misses.
    fn guarded<T>(&self, query: impl FnOnce() -> Option<T>) -> Option<T> {
        if self.failed.get() {
            return None;
        }
        catch_unwind(AssertUnwindSafe(query)).unwrap_or_else(|_| {
            self.failed.set(true);
            tracing::warn!("ty resolver: a query panicked; Python imports resolve by path for the rest of the stitch");
            None
        })
    }
}

/// Whether `rel` is a file ty resolves imports to.
fn is_python_source(rel: &str) -> bool {
    rel.ends_with(".py") || rel.ends_with(".pyi")
}

/// The virtual path of the repo-relative `rel`.
fn virtual_path(rel: &str) -> SystemPathBuf {
    SystemPath::new(ROOT).join(rel)
}

/// The repo-relative, forward-slashed form of the virtual `path`.
fn repo_relative(path: &SystemPath) -> String {
    path.as_str().trim_start_matches(['/', '\\']).replace('\\', "/")
}

/// Record the virtual file `path` as consulted.
fn probe_file(path: &SystemPath, probes: &mut ProbeSet) {
    probes.insert_rel(&RelPath::from(repo_relative(path).as_str()));
}

/// The salsa database ty's queries run against: the in-memory file system, ruff's file table and an
/// empty vendored file system (the custom typeshed replaces the bundled one).
#[salsa::db]
#[derive(Clone)]
struct ResolverDb {
    storage: salsa::Storage<Self>,
    files: Files,
    system: InMemorySystem,
    vendored: VendoredFileSystem,
}

impl ResolverDb {
    fn new(system: InMemorySystem) -> Self {
        Self {
            storage: salsa::Storage::default(),
            files: Files::default(),
            system,
            vendored: VendoredFileSystem::default(),
        }
    }
}

#[salsa::db]
impl ruff_db::Db for ResolverDb {
    fn vendored(&self) -> &VendoredFileSystem {
        &self.vendored
    }

    fn system(&self) -> &dyn System {
        &self.system
    }

    fn files(&self) -> &Files {
        &self.files
    }
}

#[salsa::db]
impl ty_module_resolver::Db for ResolverDb {}

#[salsa::db]
impl salsa::Database for ResolverDb {}
