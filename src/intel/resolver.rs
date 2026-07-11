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

// The `SpecifierResolver` dispatch and its shared helper only exist when at least one resolver
// backend is compiled in — its sole consumer (`xfile`) is gated the same way. Under default
// features the module compiles to nothing.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
use std::path::Path;

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
use crate::intel::model::ImportEdge;
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
use crate::path::RelPath;

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

    /// Resolve `import`'s module specifier, relative to `importer_rel` (a repo-relative forward-
    /// slashed path), to the repo-relative target file under `root`. Returns `None` on any miss —
    /// unresolvable specifier, target outside the repo, or a path the resolver declines to guess.
    ///
    /// The caller still validates the target is an indexed file and performs the name→export join;
    /// this method only answers "which file does this specifier point at".
    pub(crate) fn resolve(&self, root: &Path, importer_rel: &str, import: &ImportEdge) -> Option<RelPath> {
        match self {
            #[cfg(feature = "code-intel-js")]
            Self::Js(resolver) => resolver.resolve(root, importer_rel, import),
            #[cfg(feature = "code-intel-stack")]
            Self::Python(resolver) => resolver.resolve(root, importer_rel, import),
            #[cfg(feature = "code-intel-stack")]
            Self::Java(resolver) => resolver.resolve(root, importer_rel, import),
        }
    }
}

/// Convert an absolute path under `root` to a repo-relative [`RelPath`] (forward-slashed to match
/// the scanner's key convention). Returns `None` for paths outside `root` or non-UTF-8 paths.
/// Shared by the resolver variants.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
fn to_repo_relative(root: &Path, target_abs: &Path) -> Option<RelPath> {
    let rel = target_abs.strip_prefix(root).ok()?;
    let normalized = rel.to_str()?.replace('\\', "/");
    Some(RelPath::from(normalized.as_str()))
}

#[cfg(feature = "code-intel-js")]
pub(crate) mod js {
    //! JS/TS specifier resolution via `oxc_resolver` — the single source of truth for the oxc
    //! module-resolution config the crate uses.

    use std::path::Path;

    use oxc_resolver::{ResolveOptions, Resolver};

    use super::to_repo_relative;
    use crate::intel::model::ImportEdge;
    use crate::path::RelPath;

    /// JS/TS module-resolution extensions, TS-first so a bare `./util` specifier binds to `util.ts`
    /// before `util.js` (matching `tsc`'s module-resolution precedence).
    const RESOLVE_EXTENSIONS: &[&str] = &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];

    /// TS-aware Node resolver wrapping [`oxc_resolver`]. Built once per stitch and reused across all
    /// JS/TS importers.
    pub(crate) struct JsResolver {
        inner: Resolver,
    }

    impl JsResolver {
        /// Construct the resolver. Configured for TS-aware Node resolution: TS extensions win, a
        /// `./util.js` specifier maps back to `util.ts` (TS's rewritten-extension convention), and
        /// the standard import/require conditions are enabled for `package.json` `exports` maps.
        pub(crate) fn new() -> Self {
            let ext_alias = |from: &str, to: &[&str]| (from.to_string(), to.iter().map(|s| (*s).to_string()).collect());
            let inner = Resolver::new(ResolveOptions {
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
                ..ResolveOptions::default()
            });
            Self { inner }
        }

        /// Resolve `import`'s specifier relative to `importer_rel`'s directory to a repo-relative
        /// target under `root`. `None` on any oxc miss or out-of-repo path.
        pub(crate) fn resolve(&self, root: &Path, importer_rel: &str, import: &ImportEdge) -> Option<RelPath> {
            let importer_abs = root.join(importer_rel);
            let importer_dir = importer_abs.parent()?;
            let resolution = self.inner.resolve(importer_dir, &import.specifier).ok()?;
            to_repo_relative(root, &resolution.full_path())
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

    use super::to_repo_relative;
    use crate::intel::model::ImportEdge;
    use crate::path::RelPath;

    /// Package roots searched for absolute dotted imports, most-specific first. `src/` is a common
    /// layout (`src`-layout packaging); the repo root covers flat layouts.
    const PACKAGE_ROOTS: &[&str] = &["src", ""];

    pub(crate) struct PythonResolver;

    impl PythonResolver {
        /// Resolve `import`'s dotted-or-relative specifier to a repo-relative `.py` file.
        pub(crate) fn resolve(&self, root: &Path, importer_rel: &str, import: &ImportEdge) -> Option<RelPath> {
            let specifier = import.specifier.as_str();
            let dots = specifier.chars().take_while(|&c| c == '.').count();
            if dots > 0 {
                self.resolve_relative(root, importer_rel, specifier, dots)
            } else {
                self.resolve_absolute(root, specifier)
            }
        }

        /// Absolute dotted import: try each package root, mapping `foo.bar` → `foo/bar`.
        fn resolve_absolute(&self, root: &Path, specifier: &str) -> Option<RelPath> {
            let rel_parts = dotted_to_parts(specifier)?;
            for pkg_root in PACKAGE_ROOTS {
                let mut base = root.to_path_buf();
                if !pkg_root.is_empty() {
                    base.push(pkg_root);
                    if !base.is_dir() {
                        continue;
                    }
                }
                if let Some(hit) = module_file(root, &base, &rel_parts) {
                    return Some(hit);
                }
            }
            None
        }

        /// Relative import: climb `dots` package levels from the importer's directory, then descend
        /// through the dotted tail after the dots.
        fn resolve_relative(&self, root: &Path, importer_rel: &str, specifier: &str, dots: usize) -> Option<RelPath> {
            let importer_abs = root.join(importer_rel);
            // The importer's package directory is its parent; the first dot refers to it, so we
            // climb `dots - 1` additional parents.
            let mut base = importer_abs.parent()?.to_path_buf();
            for _ in 1..dots {
                base = base.parent()?.to_path_buf();
                // Never climb above the repo root.
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
            module_file(root, &base, &parts)
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
    fn module_file(root: &Path, base: &Path, parts: &[&str]) -> Option<RelPath> {
        let mut dir = base.to_path_buf();
        if parts.is_empty() {
            let init = dir.join("__init__.py");
            return init.is_file().then(|| to_repo_relative(root, &init)).flatten();
        }
        // All but the last component must be package directories.
        for part in &parts[..parts.len() - 1] {
            dir.push(part);
        }
        let last = parts[parts.len() - 1];
        let module: PathBuf = dir.join(format!("{last}.py"));
        if module.is_file()
            && let Some(hit) = to_repo_relative(root, &module)
        {
            return Some(hit);
        }
        let package_init = dir.join(last).join("__init__.py");
        if package_init.is_file() {
            return to_repo_relative(root, &package_init);
        }
        None
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

    use std::path::Path;

    use super::to_repo_relative;
    use crate::intel::model::ImportEdge;
    use crate::path::RelPath;

    /// Source roots searched for a fully-qualified type, most-specific first. Covers the Maven/
    /// Gradle standard directory layout plus a bare `src/` and the repo root (flat / single-module).
    const SOURCE_ROOTS: &[&str] = &["src/main/java", "src/test/java", "src", ""];

    pub(crate) struct JavaResolver;

    impl JavaResolver {
        /// Resolve `import`'s fully-qualified name (`com.example.Foo`) to `<root>/com/example/Foo.java`
        /// under some source root. `None` for wildcard imports or when no matching file exists.
        pub(crate) fn resolve(&self, root: &Path, _importer_rel: &str, import: &ImportEdge) -> Option<RelPath> {
            let fqn = import.specifier.as_str();
            let parts: Vec<&str> = fqn.split('.').collect();
            // Need at least a package segment and a type; reject wildcards and malformed segments.
            if parts.len() < 2 || parts.iter().any(|p| p.is_empty() || *p == "*") {
                return None;
            }
            let rel_java = format!("{}.java", parts.join("/"));
            for source_root in SOURCE_ROOTS {
                let mut base = root.to_path_buf();
                if !source_root.is_empty() {
                    for segment in source_root.split('/') {
                        base.push(segment);
                    }
                    if !base.is_dir() {
                        continue;
                    }
                }
                let candidate = base.join(&rel_java);
                if candidate.is_file()
                    && let Some(hit) = to_repo_relative(root, &candidate)
                {
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
        let got = resolver.resolve(root, "app/main.py", &import("foo.bar", Some("x")));
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
        let got = resolver.resolve(root, "app/main.py", &import("pkg", Some("y")));
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
        let got = resolver.resolve(root, "src/mypkg/main.py", &import("mypkg.util", Some("z")));
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
        let got = resolver.resolve(root, "pkg/main.py", &import(".sibling", Some("a")));
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
        let got = resolver.resolve(root, "pkg/main.py", &import(".", Some("b")));
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
        let got = resolver.resolve(root, "pkg/sub/main.py", &import("..other", Some("c")));
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
        // `..x` from a top-level file would climb above root.
        let got = resolver.resolve(root, "main.py", &import("..x", Some("x")));
        assert_eq!(got, None);
    }

    #[test]
    fn python_missing_module_resolves_none() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let resolver = SpecifierResolver::for_language("python").unwrap();
        let got = resolver.resolve(root, "main.py", &import("nope.gone", Some("x")));
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
        let got = resolver.resolve(
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
        let got = resolver.resolve(root, "com/example/Main.java", &import("com.example.Baz", None));
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
        let got = resolver.resolve(
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
        let got = resolver.resolve(root, "Main.java", &import("com.example.Gone", None));
        assert_eq!(got, None);
    }

    #[test]
    fn unknown_language_has_no_resolver() {
        assert!(SpecifierResolver::for_language("cobol").is_none());
    }
}
