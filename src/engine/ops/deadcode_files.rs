//! The unreachable files of `basemind code deadcode` (#123): entry points from the build manifests
//! and each language's conventions, reachability through the import graph from them, and the tier
//! of every file it does not reach.

use ahash::AHashSet;

use crate::engine::import_graph::{Direction, ImportGraph, NodeId, Tiers};
use crate::engine::manifests::{Ecosystem, Manifest, TargetKind};
use crate::engine::types::deadcode::{DeadFile, Tier};
use crate::path::RelPath;

/// What the text pass learned about one file that its entry-point conventions read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct FileFacts {
    /// Go: the `package` clause.
    pub go_package: Option<String>,
    /// Python: an `if __name__ == "__main__"` guard.
    pub main_guard: bool,
    /// Java: a `static void main(` method.
    pub java_main: bool,
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

/// JS/TS file stems a framework loads by its file name (Next.js, Remix, SvelteKit layouts and
/// routes).
const JS_ROUTE_STEMS: &[&str] = &[
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
        "python" if name.starts_with("test_") || name.ends_with("_test.py") || name == "tests.py" => {
            Some("a Python test file")
        }
        "python" if facts.main_guard => Some("runs as a script (`if __name__ == \"__main__\"`)"),
        "python" if dirs.split('/').any(|dir| dir == "migrations") => Some("a migration loaded by name"),
        "java" if facts.java_main => Some("declares `static void main`"),
        "java"
            if path.contains("src/test/")
                || ["Test.java", "Tests.java", "IT.java"].iter().any(|s| name.ends_with(s)) =>
        {
            Some("a Java test")
        }
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
    None
}

/// The entry files the manifests declare (Go `package main` directories expanded to their files),
/// with whether the target is a library surface (`exports`, `main`, a Cargo library root).
pub(super) fn manifest_entries<'a>(
    manifests: &[Manifest],
    files: impl Iterator<Item = &'a RelPath> + Clone,
) -> Vec<(RelPath, bool)> {
    let mut out = Vec::new();
    for manifest in manifests {
        for target in &manifest.targets {
            let Some(path) = &target.path else { continue };
            let surface = matches!(target.kind, TargetKind::Lib | TargetKind::Export)
                || (target.kind == TargetKind::Main && manifest.ecosystem == Ecosystem::Npm);
            if manifest.ecosystem == Ecosystem::Go && target.kind == TargetKind::Bin {
                let dir = path.to_str_lossy();
                let dir = dir.trim_end_matches('/');
                for file in files.clone() {
                    let file_str = file.to_str_lossy();
                    let in_dir = match file_str.rsplit_once('/') {
                        Some((parent, _)) => parent == dir,
                        None => dir.is_empty(),
                    };
                    if in_dir && file_str.ends_with(".go") {
                        out.push((file.clone(), false));
                    }
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
