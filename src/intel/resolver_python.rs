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
