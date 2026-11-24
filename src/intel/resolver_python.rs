//! Python import-specifier resolution over conventional package layouts.
//!
//! Handles the two shapes the oxc analysis's Python counterpart emits:
//!
//! - **Absolute dotted** (`import foo.bar`, `from foo.bar import x`): the `specifier` is the
//!   dotted module path `foo.bar`, resolved under a package root to `foo/bar.py` or
//!   `foo/bar/__init__.py`, and failing those to the stub `foo/bar.pyi` or `foo/bar/__init__.pyi`.
//!   The package roots are the directory of every `pyproject.toml`, `setup.cfg` and `setup.py` of
//!   the repository and its `src/`, those holding the import's top-level name only, the nearest to
//!   the importer first, then `src/` and the repository root.
//! - **Relative** (`from . import x`, `from .mod import y`, `from ..pkg import z`): the
//!   `specifier` carries the leading dots. Each leading dot climbs one package directory from
//!   the importer's own package (the importer's directory), then the remaining dotted tail
//!   resolves under that directory.
//!
//! A `from` import's name may be a submodule rather than a binding of the package
//! (`from . import views`); [`PythonResolver::resolve_submodule`] resolves that second reading,
//! which the cross-file stitch tries when the name binds no export.
//!
//! Conservative: a specifier that climbs above `root`, or resolves to no on-disk file, yields
//! `None`.
//!
//! The project directories are found once per resolver, by walking the repository. A project added
//! after that walk is seen by the next stitch, not by the importers of the current one.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ahash::AHashSet;

use super::{ProbeSet, probe_file};
use crate::intel::model::ImportEdge;
use crate::path::RelPath;

/// Package roots always searched for absolute dotted imports, after the project roots. `src/` is a
/// common layout (`src`-layout packaging); the repo root covers flat layouts.
const DEFAULT_ROOTS: &[&str] = &["src", ""];

/// Files that mark a directory as a Python project.
const MANIFESTS: &[&str] = &["pyproject.toml", "setup.cfg", "setup.py"];

/// The source suffix and package-initialiser name of a module, in the order they are tried: the
/// implementation, then its type stub.
const SOURCE_KINDS: [(&str, &str); 2] = [(".py", "__init__.py"), (".pyi", "__init__.pyi")];

/// Resolves Python import specifiers. Discovers the repository's package roots on first use.
#[derive(Default)]
pub(crate) struct PythonResolver {
    roots: OnceLock<Vec<SourceRoot>>,
}

impl PythonResolver {
    /// Resolve `import`'s dotted-or-relative specifier to a repo-relative `.py` file.
    pub(crate) fn resolve(
        &self,
        root: &Path,
        importer_rel: &str,
        import: &ImportEdge,
        probes: &mut ProbeSet,
    ) -> Option<RelPath> {
        self.resolve_specifier(root, importer_rel, &import.specifier, probes)
    }

    /// Resolve `from <module> import <name>` to the submodule `<module>.<name>`, for a `<name>` that
    /// binds no export of `<module>`: `from . import views` names `views.py`, not a binding of the
    /// package's `__init__.py`. `package` is the file `<module>` resolved to, if any. A plain module
    /// file has no submodules, so only a package `__init__.py` or a package that resolved to no
    /// file (a namespace package) is searched. A package's submodule is looked for beside its
    /// `__init__.py`, as Python searches the package's `__path__`; a namespace package's under every
    /// package root. `None` for an import that names no member.
    pub(crate) fn resolve_submodule(
        &self,
        root: &Path,
        importer_rel: &str,
        package: Option<&RelPath>,
        import: &ImportEdge,
        probes: &mut ProbeSet,
    ) -> Option<RelPath> {
        let name = import.imported.as_deref()?;
        if let Some(package) = package {
            if !is_package_init(package) {
                return None;
            }
            let package_dir = root.join(package.as_str()?).parent()?.to_path_buf();
            return module_file(root, &package_dir, &dotted_to_parts(name)?, true, probes);
        }
        let module = import.specifier.as_str();
        let specifier = if module.ends_with('.') {
            format!("{module}{name}")
        } else {
            format!("{module}.{name}")
        };
        self.resolve_specifier(root, importer_rel, &specifier, probes)
    }

    fn resolve_specifier(
        &self,
        root: &Path,
        importer_rel: &str,
        specifier: &str,
        probes: &mut ProbeSet,
    ) -> Option<RelPath> {
        let dots = specifier.chars().take_while(|&c| c == '.').count();
        if dots > 0 {
            self.resolve_relative(root, importer_rel, specifier, dots, probes)
        } else {
            self.resolve_absolute(root, importer_rel, specifier, probes)
        }
    }

    /// Absolute dotted import: try each package root, mapping `foo.bar` → `foo/bar`.
    fn resolve_absolute(
        &self,
        root: &Path,
        importer_rel: &str,
        specifier: &str,
        probes: &mut ProbeSet,
    ) -> Option<RelPath> {
        let rel_parts = dotted_to_parts(specifier)?;
        for pkg_root in self.package_roots(root, importer_rel, rel_parts[0]) {
            let base = if pkg_root.is_empty() {
                root.to_path_buf()
            } else {
                root.join(&pkg_root)
            };
            if let Some(hit) = module_file(root, &base, &rel_parts, false, probes) {
                return Some(hit);
            }
        }
        None
    }

    /// The package roots to search for a module whose top-level name is `first`: the project roots
    /// that hold that name, nearest to the importer first, then [`DEFAULT_ROOTS`].
    fn package_roots(&self, root: &Path, importer_rel: &str, first: &str) -> Vec<String> {
        let project_roots = self
            .roots
            .get_or_init(|| roots::discover(root, MANIFESTS, &["src", ""], DEFAULT_ROOTS, top_level_names));
        roots::nearest(project_roots, importer_rel, first)
            .chain(DEFAULT_ROOTS.iter().copied())
            .map(str::to_string)
            .collect()
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
        module_file(root, &base, &parts, false, probes)
    }
}

/// The module names a directory holds at its top level: its subdirectories and its `.py` / `.pyi`
/// files without the suffix.
fn top_level_names(dir: &Path) -> AHashSet<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return AHashSet::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .map(|name| {
            name.strip_suffix(".pyi")
                .or_else(|| name.strip_suffix(".py"))
                .map_or(name.clone(), str::to_string)
        })
        .collect()
}

/// Whether `path` is a package's `__init__.py` or its stub.
fn is_package_init(path: &RelPath) -> bool {
    path.as_str().is_some_and(|path| {
        SOURCE_KINDS
            .iter()
            .any(|(_, init)| path == *init || path.strip_suffix(*init).is_some_and(|dir| dir.ends_with('/')))
    })
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
/// path of `<base>/<parts...>.py` or `<base>/<parts...>/__init__.py`, whichever exists, else of the
/// `.pyi` stub of either. Empty
/// `parts` (a bare `from . import x`) resolves to the package's own `__init__.py`. The module file
/// wins unless `package_first`, which a submodule looked up in its package's directory sets, as
/// Python's finder prefers the package.
///
/// Both candidates are recorded in `probes` before either is tested, so a module that appears
/// later (`pkg/mod.py` shadowing `pkg/mod/__init__.py`, or the first `__init__.py` of a package)
/// is found by the importers that missed it. A missing directory needs no separate probe: the
/// files under it do not exist either.
fn module_file(
    root: &Path,
    base: &Path,
    parts: &[&str],
    package_first: bool,
    probes: &mut ProbeSet,
) -> Option<RelPath> {
    SOURCE_KINDS
        .iter()
        .find_map(|&(suffix, init)| module_source(root, base, parts, (suffix, init), package_first, probes))
}

/// [`module_file`] for one source kind: `<base>/<parts...><suffix>` or `<base>/<parts...>/<init>`.
fn module_source(
    root: &Path,
    base: &Path,
    parts: &[&str],
    (suffix, init): (&str, &str),
    package_first: bool,
    probes: &mut ProbeSet,
) -> Option<RelPath> {
    let mut dir = base.to_path_buf();
    let Some((last, parents)) = parts.split_last() else {
        return probe_file(root, &dir.join(init), probes);
    };
    dir.extend(parents);
    let module: PathBuf = dir.join(format!("{last}{suffix}"));
    let package: PathBuf = dir.join(last).join(init);
    if package_first {
        // Both are probed, so a module file that appears beside a package re-stitches its importers.
        let package_hit = probe_file(root, &package, probes);
        let module_hit = probe_file(root, &module, probes);
        return package_hit.or(module_hit);
    }
    probe_file(root, &module, probes).or_else(|| probe_file(root, &package, probes))
}
