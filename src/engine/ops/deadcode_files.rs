//! The unreachable files of `basemind code deadcode` (#123): entry points from the build manifests
//! and each language's conventions, reachability through the import graph from them, and the tier
//! of every file it does not reach.

use ahash::{AHashMap, AHashSet};

use super::deadcode_rust::HiddenModules;
use crate::engine::import_graph::{Direction, ImportGraph, NodeId, Tiers};
use crate::engine::manifests::{Ecosystem, Manifest, TargetKind, parent_dir};
use crate::engine::types::deadcode::{DeadFile, Tier};
use crate::path::RelPath;

/// What the text pass learned about one file that its entry-point conventions read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct FileFacts {
    /// Go: the `package` clause.
    pub go_package: Option<String>,
    /// Go: the names a cgo `//export` comment exports to C.
    pub go_exports: Vec<String>,
    /// Python: an `if __name__ == "__main__"` guard.
    pub main_guard: bool,
    /// Python: dotted module paths spelled in string literals (`"app.urls"`).
    pub module_strings: Vec<String>,
    /// Java: a `main` method.
    pub java_main: bool,
    /// Rust: module declarations the resolver may not follow.
    pub rust: HiddenModules,
}

/// Directory names whose files are run, loaded as data or collected by a tool rather than imported.
const ENTRY_DIRS: &[&str] = &[
    "test",
    "tests",
    "__tests__",
    "spec",
    "e2e",
    "testdata",
    "test-data",
    "fixtures",
    "__fixtures__",
    "__mocks__",
    "examples",
    "example",
    "benches",
    "bench",
    "benchmarks",
    "scripts",
    "bin",
];

/// Directory names holding test data: sample repositories and inputs, never built.
const DATA_DIRS: &[&str] = &["fixtures", "__fixtures__", "testdata", "test-data"];

/// Whether `path` is test data, which no finding is reported for.
pub(super) fn is_test_data(path: &RelPath) -> bool {
    let path = path.to_str_lossy();
    let dirs = path.rsplit_once('/').map_or("", |(dirs, _)| dirs);
    dirs.split('/').any(|dir| DATA_DIRS.contains(&dir))
}

/// Python file names a tool loads by name.
const PYTHON_ENTRY_NAMES: &[&str] = &[
    "__main__.py",
    "__init__.py",
    "conftest.py",
    "setup.py",
    "manage.py",
    "noxfile.py",
    "fabfile.py",
    "conf.py",
    "wsgi.py",
    "asgi.py",
];

/// Python file names Django and Celery load by name, and directories whose modules they discover.
const PYTHON_FRAMEWORK_NAMES: &[&str] = &[
    "admin.py",
    "apps.py",
    "urls.py",
    "models.py",
    "signals.py",
    "tasks.py",
    "celery.py",
];
const PYTHON_FRAMEWORK_DIRS: &[&str] = &["management/commands", "templatetags"];

/// JS/TS file stems a framework loads by its file name under a route directory (Next.js, Remix,
/// SvelteKit layouts, routes and roots).
const JS_ROUTE_STEMS: &[&str] = &[
    "root",
    "entry",
    "page",
    "layout",
    "route",
    "loading",
    "error",
    "not-found",
    "template",
    "middleware",
    "default",
    "head",
    "+page",
    "+layout",
    "+server",
];

/// JS/TS file stems a framework loads by name from `src/` or a route directory (Next.js middleware
/// and instrumentation, SvelteKit hooks).
const JS_FRAMEWORK_STEMS: &[&str] = &["middleware", "instrumentation", "hooks"];

/// A Java file a test runner collects: under a test source set or named like a test class.
pub(super) fn java_test_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.starts_with("src/test/")
        || path.contains("/src/test/")
        || ["Test.java", "Tests.java", "IT.java", "TestCase.java"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// The languages whose files the reachability walk judges: those whose imports name files.
pub(super) const FILE_LANGUAGES: &[&str] = &["rust", "go", "python", "javascript", "typescript", "tsx", "java"];

/// Why `path` is an entry point by its language's conventions, or `None`.
pub(super) fn conventional_entry(path: &str, language: &str, facts: &FileFacts) -> Option<&'static str> {
    let (dirs, name) = match path.rsplit_once('/') {
        Some((dirs, name)) => (dirs, name),
        None => ("", path),
    };
    if dirs.split('/').any(|dir| ENTRY_DIRS.contains(&dir)) {
        return Some("under a test, fixture, example, benchmark or script directory");
    }
    if name.starts_with('.') || dirs.split('/').any(|dir| dir.starts_with('.')) {
        return Some("under a dot directory or a dot file: a tool loads it by name");
    }
    match language {
        "go" if name.ends_with("_test.go") => Some("a Go test file"),
        "go" if facts.go_package.as_deref() == Some("main") => Some("Go `package main`"),
        "python" if PYTHON_ENTRY_NAMES.contains(&name) => Some("a Python file a tool loads by name"),
        "python"
            if PYTHON_FRAMEWORK_NAMES.contains(&name)
                || PYTHON_FRAMEWORK_DIRS
                    .iter()
                    .any(|dir| dirs == *dir || dirs.ends_with(&format!("/{dir}"))) =>
        {
            Some("a module Django or Celery loads by name")
        }
        "python" if name.starts_with("test_") || name.ends_with("_test.py") || name == "tests.py" => {
            Some("a Python test file")
        }
        "python" if facts.main_guard => Some("runs as a script (`if __name__ == \"__main__\"`)"),
        "python" if dirs.split('/').any(|dir| dir == "migrations") => Some("a migration loaded by name"),
        "java" if facts.java_main => Some("declares a `main` method"),
        "java" if java_test_file(path) => Some("a Java test"),
        "javascript" | "typescript" | "tsx" => js_entry(dirs, name),
        _ => None,
    }
}

fn js_entry(dirs: &str, name: &str) -> Option<&'static str> {
    let stem = name.split('.').next().unwrap_or(name);
    let infix = |tag: &str| name.split('.').skip(1).any(|part| part == tag);
    if infix("test") || infix("spec") || infix("stories") || infix("story") {
        return Some("a test or story file");
    }
    if infix("config") || name.ends_with(".d.ts") {
        return Some("a config or declaration file a tool loads by name");
    }
    if dirs.is_empty() {
        return Some("a file at the repository root");
    }
    let in_routes = dirs
        .split('/')
        .any(|dir| matches!(dir, "pages" | "routes" | "app" | "public"));
    if in_routes && (JS_ROUTE_STEMS.contains(&stem) || dirs.split('/').any(|dir| dir == "pages" || dir == "public")) {
        return Some("a framework route or page loaded by its path");
    }
    let in_src = dirs == "src" || dirs.ends_with("/src");
    if (in_src || in_routes) && JS_FRAMEWORK_STEMS.contains(&stem) {
        return Some("a file a framework loads by its name");
    }
    if in_src && matches!(stem, "index" | "main") {
        return Some("the conventional entry of a JS app (`src/index`, `src/main`)");
    }
    None
}

/// The entry files the manifests declare (Go `package main` directories expanded to their files),
/// with whether the target is a library surface (`exports`, `main`, a Cargo library root).
pub(super) fn manifest_entries<'a>(
    manifests: &[Manifest],
    files: impl Iterator<Item = &'a RelPath>,
) -> Vec<(RelPath, bool)> {
    let mut go_by_dir: Option<AHashMap<String, Vec<&RelPath>>> = None;
    let mut files = Some(files);
    let mut out = Vec::new();
    for manifest in manifests {
        for target in &manifest.targets {
            let Some(path) = &target.path else { continue };
            let surface = matches!(target.kind, TargetKind::Lib | TargetKind::Export)
                || (target.kind == TargetKind::Main && manifest.ecosystem == Ecosystem::Npm);
            if manifest.ecosystem == Ecosystem::Go && target.kind == TargetKind::Bin {
                // The Go files grouped by directory once, for every `package main` target.
                let by_dir = go_by_dir.get_or_insert_with(|| {
                    let mut by_dir: AHashMap<String, Vec<&RelPath>> = AHashMap::new();
                    for file in files.take().into_iter().flatten() {
                        if file.as_bytes().ends_with(b".go") {
                            let file_str = file.to_str_lossy();
                            by_dir.entry(parent_dir(&file_str).to_string()).or_default().push(file);
                        }
                    }
                    by_dir
                });
                let dir = path.to_str_lossy();
                let dir = dir.trim_end_matches('/');
                let dir = if dir == "." { "" } else { dir };
                for file in by_dir.get(dir).into_iter().flatten() {
                    out.push(((*file).clone(), false));
                }
            } else {
                out.push((path.clone(), surface));
            }
        }
    }
    out
}

/// One file the reachability walk judges.
pub(super) struct Judged<'a> {
    pub path: &'a RelPath,
    pub language: &'a str,
    /// Why the file is an entry point, when it is one.
    pub entry: Option<&'static str>,
}

/// Tiers of the walk: Rust `mod` declarations count (a declared module is compiled), type-only
/// imports are judged separately.
const COMPILED: Tiers = Tiers {
    type_imports: false,
    module_decls: true,
};

/// The files of `judged` that no entry point reaches, each with its tier and reason.
pub(super) fn unreachable(graph: &ImportGraph, judged: &[Judged<'_>]) -> Vec<DeadFile> {
    let roots: Vec<NodeId> = judged
        .iter()
        .filter(|file| file.entry.is_some())
        .filter_map(|file| graph.node(&file.path.to_str_lossy()))
        .collect();
    let compiled = graph.reachable(&roots, Direction::Dependencies, COMPILED);
    let typed = graph.reachable(&roots, Direction::Dependencies, Tiers::ALL);
    let mut out = Vec::new();
    for file in judged.iter().filter(|file| file.entry.is_none()) {
        let node = graph.node(&file.path.to_str_lossy());
        if node.is_some_and(|node| compiled[node as usize]) {
            continue;
        }
        let (evidence, reason) = match node {
            Some(node) if typed[node as usize] => (Tier::Low, "reached only through type-only imports".to_string()),
            Some(node) if graph.in_degree(node, Tiers::ALL) > 0 => (
                Tier::Medium,
                format!(
                    "imported only by files no entry point reaches ({})",
                    importers(graph, node)
                ),
            ),
            _ => (Tier::High, "no file imports it and it is no entry point".to_string()),
        };
        let (tier, caveat) = language_cap(file.path, file.language, evidence);
        let reason = match caveat {
            Some(caveat) => format!("{reason}; {caveat}"),
            None => reason,
        };
        out.push(DeadFile {
            path: file.path.clone(),
            language: file.language.to_string(),
            tier,
            reason,
        });
    }
    out
}

/// The first importers of `node`, for a reason.
fn importers(graph: &ImportGraph, node: NodeId) -> String {
    let mut names: Vec<String> = graph
        .importers(node, Tiers::ALL)
        .take(3)
        .map(|edge| graph.path(edge.node).to_string())
        .collect();
    let total = graph.in_degree(node, Tiers::ALL);
    if total > names.len() {
        names.push(format!("{} more", total - names.len()));
    }
    names.join(", ")
}

/// The tier a language allows a file finding, and why it lowers one: Python and JS load modules by
/// name at run time, Java classes of one package use each other without an import, and a Go package
/// outside `internal/` may be imported by another module.
fn language_cap(path: &RelPath, language: &str, evidence: Tier) -> (Tier, Option<&'static str>) {
    let (cap, caveat) = match language {
        "rust" => return (evidence, None),
        "go" if path.to_str_lossy().split('/').any(|dir| dir == "internal") => return (evidence, None),
        "go" => (
            Tier::Low,
            "a package outside `internal/` may be imported from outside the repository",
        ),
        "javascript" | "typescript" | "tsx" => (
            Tier::Medium,
            "a dynamic `import()`, `require` of a computed path or a bundler entry would not be seen",
        ),
        "python" => (
            Tier::Medium,
            "`importlib`, plugin entry points or framework discovery load modules by name",
        ),
        _ => (Tier::Low, "classes of one package use each other without an import"),
    };
    if evidence > cap {
        (cap, Some(caveat))
    } else {
        (evidence, Some(caveat))
    }
}

/// The files of a library surface: everything a Cargo library root, a `package.json` `exports` or
/// `main` reaches. Their exported names may be used outside the repository.
pub(super) fn surface_files(graph: &ImportGraph, surfaces: &[RelPath]) -> AHashSet<RelPath> {
    let roots: Vec<NodeId> = surfaces
        .iter()
        .filter_map(|path| graph.node(&path.to_str_lossy()))
        .collect();
    let reached = graph.reachable(&roots, Direction::Dependencies, Tiers::ALL);
    let mut out: AHashSet<RelPath> = surfaces.iter().cloned().collect();
    out.extend(
        reached
            .iter()
            .enumerate()
            .filter(|(_, reached)| **reached)
            .filter_map(|(node, _)| NodeId::try_from(node).ok())
            .map(|node| graph.path(node).clone()),
    );
    out
}

/// `dir` as a path prefix: `dir/`, or `""` for the root.
fn dir_prefix(dir: &str) -> String {
    if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    }
}

/// The directory prefixes (see [`dir_prefix`]) of the manifests of `ecosystem` that `keep` accepts.
pub(super) fn manifest_prefixes(
    manifests: &[Manifest],
    ecosystem: Ecosystem,
    keep: impl Fn(&Manifest) -> bool,
) -> Vec<String> {
    manifests
        .iter()
        .filter(|m| m.ecosystem == ecosystem && keep(m))
        .map(|m| dir_prefix(parent_dir(&m.path.to_str_lossy())))
        .collect()
}

/// The longest of `prefixes` that `path` starts with.
pub(super) fn nearest<'p>(path: &str, prefixes: &'p [String]) -> Option<&'p str> {
    prefixes
        .iter()
        .filter(|prefix| path.starts_with(prefix.as_str()))
        .max_by_key(|prefix| prefix.len())
        .map(String::as_str)
}

/// What the files of one Cargo package declare where the resolver cannot follow.
#[derive(Debug, Default)]
pub(super) struct CrateModules {
    mod_names: AHashSet<String>,
    rs_files: AHashSet<String>,
    macro_modules: bool,
}

/// Per Cargo package directory prefix, the module declarations its Rust files make in code the
/// resolver does not read: a `mod` inside a macro body, a `.rs` path in a string (`include!`, a
/// `#[cfg_attr(…, path = …)]`), a macro declaring modules from its arguments.
pub(super) fn crate_modules<'f>(
    crates: &[String],
    facts: impl Iterator<Item = (&'f RelPath, &'f FileFacts)>,
) -> AHashMap<String, CrateModules> {
    let mut out: AHashMap<String, CrateModules> = AHashMap::new();
    for (path, facts) in facts {
        let hidden = &facts.rust;
        if hidden.mod_names.is_empty() && hidden.rs_strings.is_empty() && !hidden.macro_modules {
            continue;
        }
        let Some(prefix) = nearest(&path.to_str_lossy(), crates) else {
            continue;
        };
        let entry = out.entry(prefix.to_string()).or_default();
        entry.mod_names.extend(hidden.mod_names.iter().cloned());
        entry.rs_files.extend(hidden.rs_strings.iter().cloned());
        entry.macro_modules |= hidden.macro_modules;
    }
    out
}

/// What lowers a file finding below its evidence once the walk is done, by the package it sits in.
pub(super) struct PackageCaps<'a> {
    /// Cargo package prefixes, with what their files declare out of the resolver's sight.
    pub crates: &'a [String],
    pub crate_modules: &'a AHashMap<String, CrateModules>,
    /// Prefixes of the Python packages a `pyproject.toml` names: their public modules are importable.
    pub python_packages: &'a [String],
    /// Prefixes of the npm packages whose declared entry is a build output with no indexed source.
    pub js_build_output: &'a [String],
}

/// Cap the findings whose package hides an entry or a declaration from the walk.
pub(super) fn cap_by_package(found: &mut [DeadFile], caps: &PackageCaps<'_>) {
    for file in found.iter_mut().filter(|file| file.tier > Tier::Low) {
        let path = file.path.to_str_lossy();
        let caveat = match file.language.as_str() {
            "rust" => rust_hidden(&path, caps).then_some(
                "a `mod` inside a macro, a `#[cfg_attr(…, path)]` or an `include!` in its crate may declare it, \
                 which the resolver does not follow",
            ),
            "python" => nearest(&path, caps.python_packages)
                .is_some_and(|prefix| path[prefix.len()..].split('/').all(|segment| !segment.starts_with('_')))
                .then_some(
                    "a public module of a package `pyproject.toml` names may be imported from outside the repository",
                ),
            "javascript" | "typescript" | "tsx" => nearest(&path, caps.js_build_output)
                .is_some()
                .then_some("its package's declared entry is a build output with no indexed source"),
            _ => None,
        };
        if let Some(caveat) = caveat {
            file.tier = Tier::Low;
            file.reason.push_str("; ");
            file.reason.push_str(caveat);
        }
    }
}

/// Whether a Rust file may be declared where the resolver cannot see: its crate names its stem
/// after a `mod` it did not resolve, spells its file name in a `.rs` string, or declares modules
/// from macro arguments.
fn rust_hidden(path: &str, caps: &PackageCaps<'_>) -> bool {
    let Some(modules) = nearest(path, caps.crates).and_then(|prefix| caps.crate_modules.get(prefix)) else {
        return false;
    };
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = match name.strip_suffix(".rs") {
        Some("mod") => parent_dir(path).rsplit('/').next().unwrap_or_default(),
        Some(stem) => stem,
        None => name,
    };
    modules.macro_modules || modules.mod_names.contains(stem) || modules.rs_files.contains(name)
}

/// Whether a Python file's dotted module path, or a dot-boundary suffix of it of at least two
/// segments, is spelled in a string of the indexed Python code (`"app.urls"` reaches
/// `backend/app/urls.py`): a framework loads it by name.
pub(super) fn named_by_module_string(path: &str, strings: &AHashSet<&str>) -> bool {
    if strings.is_empty() {
        return false;
    }
    let Some(module) = path.strip_suffix(".py") else {
        return false;
    };
    let module = module.strip_suffix("/__init__").unwrap_or(module).replace('/', ".");
    let mut rest = module.as_str();
    loop {
        if rest.contains('.') && strings.contains(rest) {
            return true;
        }
        match rest.split_once('.') {
            Some((_, tail)) => rest = tail,
            None => return false,
        }
    }
}
