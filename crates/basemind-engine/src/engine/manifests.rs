//! Build manifests of a repository (#131, shared with the dead-code entry points of #123):
//! `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, `pom.xml` and `build.gradle[.kts]`,
//! read for what they declare: name, version, workspace members, dependency counts and names, and
//! the targets the build starts from.
//!
//! Manifests are configuration, not code, so the code map does not hold them. They are found next
//! to the indexed files instead: [`discover`] looks for each manifest name in every directory that
//! holds an indexed file and in each of its ancestors, so a manifest whose package has no indexed
//! code is not reported, and nothing is walked that the scan left out (a `node_modules`, a build
//! directory).
//!
//! Parsing is lenient: a manifest that does not parse is reported with its `error` and nothing
//! else, never guessed at. A target is reported with a repository-relative `path` only when that
//! file is indexed ([`FileSet`]), so a declared `main` that does not exist is named, not resolved.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::path::RelPath;

/// Largest manifest read; a larger one is reported with an `error`.
pub(crate) const MANIFEST_BYTES_MAX: u64 = 4 * 1024 * 1024;

/// Dependency names one manifest lists; the counts cover the rest.
pub(crate) const DEPENDENCY_NAMES_MAX: usize = 25;

/// `exports` subpaths of one `package.json` reported as targets.
const EXPORTS_MAX: usize = 25;

/// Bytes of a Go file read to find its `package` clause.
pub(crate) const GO_HEAD_BYTES: usize = 4096;

/// Go files read for their `package` clause per module; past it the module's binaries are partial.
pub(crate) const GO_FILES_READ_MAX: usize = 20_000;

/// The build system a manifest belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Ecosystem {
    Cargo,
    Npm,
    Python,
    Go,
    Maven,
    Gradle,
}

/// File names [`discover`] looks for, with the ecosystem each one belongs to.
pub(crate) const MANIFEST_NAMES: &[(&str, Ecosystem)] = &[
    ("Cargo.toml", Ecosystem::Cargo),
    ("package.json", Ecosystem::Npm),
    ("pyproject.toml", Ecosystem::Python),
    ("go.mod", Ecosystem::Go),
    ("pom.xml", Ecosystem::Maven),
    ("build.gradle", Ecosystem::Gradle),
    ("build.gradle.kts", Ecosystem::Gradle),
];

/// What a build starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TargetKind {
    /// A library root: Cargo `[lib]` or `src/lib.rs`.
    Lib,
    /// An executable: Cargo `[[bin]]`, `src/main.rs` or `src/bin/*`, a `package.json` `bin`, a Go
    /// `package main` directory.
    Bin,
    /// A module the package names as its entry: `package.json` `main`, a Python `__main__.py`, a JVM
    /// `mainClass`.
    Main,
    /// A `package.json` `exports` subpath.
    Export,
    /// A declared command: a `package.json` script or a Python console/GUI script.
    Script,
    /// A Cargo build script.
    Build,
    /// A Cargo test target: `[[test]]` or `tests/*`.
    Test,
    /// A Cargo benchmark: `[[bench]]` or `benches/*`.
    Bench,
    /// A Cargo example: `[[example]]` or `examples/*`.
    Example,
}

/// One target of a manifest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(crate) struct Target {
    pub kind: TargetKind,
    /// The target's own name, when the manifest gives or implies one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The indexed file (or, for a Go binary, the directory) the target starts at; absent when the
    /// manifest names none or the named file is not indexed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<RelPath>,
    /// The function the target calls by name in `path`: a Python script's `pkg.cli:run` names `run`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
}

/// Dependencies one manifest declares, by tier.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct DependencyCounts {
    /// Runtime dependencies (Go: direct requirements).
    pub runtime: usize,
    /// Development, test and optional dependencies.
    #[serde(skip_serializing_if = "is_zero")]
    pub dev: usize,
    /// Cargo build dependencies.
    #[serde(skip_serializing_if = "is_zero")]
    pub build: usize,
    /// Go requirements marked `// indirect`.
    #[serde(skip_serializing_if = "is_zero")]
    pub indirect: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// What one manifest declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Manifest {
    pub path: RelPath,
    pub ecosystem: Ecosystem,
    /// Package name (Go: the module path; Maven: the `artifactId`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Workspace members or modules, as the manifest writes them (globs stay globs).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    pub dependencies: DependencyCounts,
    /// Runtime dependency names, sorted, at most [`DEPENDENCY_NAMES_MAX`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dependency_names: Vec<String>,
    /// Targets, sorted by kind then name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<Target>,
    /// Go only: the module's files were not all read for their `package` clause
    /// ([`GO_FILES_READ_MAX`]), so its binaries may be incomplete.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub targets_partial: bool,
    /// Why the manifest could not be read; nothing else is reported for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Manifest {
    fn new(path: &str, ecosystem: Ecosystem) -> Self {
        Self {
            path: RelPath::from(path),
            ecosystem,
            name: None,
            version: None,
            members: Vec::new(),
            dependencies: DependencyCounts::default(),
            dependency_names: Vec::new(),
            targets: Vec::new(),
            targets_partial: false,
            error: None,
        }
    }

    fn failed(path: &str, ecosystem: Ecosystem, error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::new(path, ecosystem)
        }
    }

    /// Record runtime dependency names: counted in full, listed up to [`DEPENDENCY_NAMES_MAX`].
    fn runtime_names(&mut self, names: impl IntoIterator<Item = String>) {
        let mut names: Vec<String> = names.into_iter().collect();
        names.sort();
        names.dedup();
        self.dependencies.runtime += names.len();
        names.truncate(DEPENDENCY_NAMES_MAX);
        self.dependency_names = names;
    }

    fn finish(mut self) -> Self {
        self.targets.sort();
        self.targets.dedup();
        self
    }
}

/// The indexed files of a workspace, as sorted repository-relative strings: what a manifest's
/// declared paths are checked against.
pub(crate) struct FileSet<'a> {
    sorted: Vec<&'a str>,
}

impl<'a> FileSet<'a> {
    /// `paths` in any order; duplicates are kept once.
    pub(crate) fn new(paths: impl IntoIterator<Item = &'a str>) -> Self {
        let mut sorted: Vec<&str> = paths.into_iter().collect();
        sorted.sort_unstable();
        sorted.dedup();
        Self { sorted }
    }

    pub(crate) fn contains(&self, path: &str) -> bool {
        self.sorted.binary_search(&path).is_ok()
    }

    /// Every file under directory `dir` (`""` is the root), at any depth, in path order.
    pub(crate) fn under(&self, dir: &str) -> impl Iterator<Item = &'a str> + '_ {
        let prefix = dir_prefix(dir);
        let start = self.sorted.partition_point(|p| *p < prefix.as_str());
        self.sorted[start..]
            .iter()
            .copied()
            .take_while(move |p| p.starts_with(prefix.as_str()))
    }

    /// Every directory holding an indexed file, and each of its ancestors (`""` is the root), sorted.
    pub(crate) fn directories(&self) -> Vec<&'a str> {
        let mut dirs: Vec<&str> = vec![""];
        for path in &self.sorted {
            let mut end = path.len();
            while let Some(slash) = path[..end].rfind('/') {
                dirs.push(&path[..slash]);
                end = slash;
            }
        }
        dirs.sort_unstable();
        dirs.dedup();
        dirs
    }

    /// The file `dir/rel` (`rel` may start with `./`), as a [`RelPath`] when it is indexed.
    fn indexed(&self, dir: &str, rel: &str) -> Option<RelPath> {
        let joined = join(dir, rel)?;
        self.contains(&joined).then(|| RelPath::from(joined))
    }
}

/// `dir` with a trailing `/`, or `""` for the root.
fn dir_prefix(dir: &str) -> String {
    if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    }
}

/// `dir/rel` with `./` segments dropped; `None` when `rel` is absolute or climbs out with `..`.
fn join(dir: &str, rel: &str) -> Option<String> {
    if rel.starts_with('/') || rel.contains('\\') {
        return None;
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for segment in rel.split('/') {
        match segment {
            "" | "." => {}
            ".." => return None,
            other => parts.push(other),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The directory of a repository-relative file path (`""` for a file at the root).
pub(crate) fn parent_dir(path: &str) -> &str {
    path.rfind('/').map_or("", |slash| &path[..slash])
}

/// What a manifest parser may read beyond its own text: the indexed files, and the head of another
/// file of the repository (a Go file's `package` clause, a Gradle `settings.gradle`).
pub(crate) struct Context<'a, 'f> {
    pub files: &'a FileSet<'f>,
    /// The text of a repository-relative file, at most `max` bytes of it; `None` when unreadable.
    pub read: &'a dyn Fn(&str, usize) -> Option<String>,
    /// Directories holding a `go.mod`: a Go module stops where a nested one starts.
    pub go_modules: &'a [&'a str],
}

/// Every manifest in `dirs` (see [`FileSet::directories`]) that `exists`, parsed. Sorted by path.
pub(crate) fn discover(dirs: &[&str], exists: &dyn Fn(&str) -> bool, cx: &Context<'_, '_>) -> Vec<Manifest> {
    let found: Vec<(String, Ecosystem)> = dirs
        .iter()
        .flat_map(|dir| {
            MANIFEST_NAMES.iter().filter_map(move |(name, ecosystem)| {
                let path = join(dir, name)?;
                exists(&path).then_some((path, *ecosystem))
            })
        })
        .collect();
    let go_dirs: Vec<&str> = found
        .iter()
        .filter(|(_, ecosystem)| *ecosystem == Ecosystem::Go)
        .map(|(path, _)| parent_dir(path))
        .collect();
    let cx = Context {
        go_modules: &go_dirs,
        ..*cx
    };
    let mut manifests: Vec<Manifest> = found
        .iter()
        .map(
            |(path, ecosystem)| match (cx.read)(path, MANIFEST_BYTES_MAX as usize + 1) {
                None => Manifest::failed(path, *ecosystem, "unreadable".to_string()),
                Some(text) if text.len() as u64 > MANIFEST_BYTES_MAX => {
                    Manifest::failed(path, *ecosystem, format!("larger than {MANIFEST_BYTES_MAX} bytes"))
                }
                Some(text) => parse(path, *ecosystem, &text, &cx),
            },
        )
        .collect();
    manifests.sort_by(|a, b| a.path.cmp(&b.path));
    manifests
}

/// Parse the manifest at repository-relative `path`.
pub(crate) fn parse(path: &str, ecosystem: Ecosystem, text: &str, cx: &Context<'_, '_>) -> Manifest {
    let dir = parent_dir(path);
    let parsed = match ecosystem {
        Ecosystem::Cargo => cargo(path, dir, text, cx.files),
        Ecosystem::Npm => npm(path, dir, text, cx.files),
        Ecosystem::Python => python(path, dir, text, cx.files),
        Ecosystem::Go => Ok(go(path, dir, text, cx)),
        Ecosystem::Maven => Ok(maven(path, text)),
        Ecosystem::Gradle => Ok(gradle(path, dir, text, cx)),
    };
    parsed
        .unwrap_or_else(|error| Manifest::failed(path, ecosystem, error))
        .finish()
}

fn toml_table(text: &str) -> Result<toml::Table, String> {
    toml::from_str::<toml::Table>(text).map_err(|e| format!("not valid TOML: {}", e.message()))
}

fn str_at<'v>(table: Option<&'v toml::Value>, key: &str) -> Option<&'v str> {
    table?.get(key)?.as_str()
}

fn table_keys(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(toml::Value::as_table)
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default()
}

/// `Cargo.toml`: the package, its workspace, its dependency tables (target-specific ones included)
/// and its targets, declared or found by Cargo's layout conventions.
fn cargo(path: &str, dir: &str, text: &str, files: &FileSet<'_>) -> Result<Manifest, String> {
    let table = toml_table(text)?;
    let mut m = Manifest::new(path, Ecosystem::Cargo);
    let package = table.get("package");
    let workspace = table.get("workspace");
    m.name = str_at(package, "name").map(str::to_owned);
    m.version = str_at(package, "version").map(str::to_owned);
    m.members = workspace
        .and_then(|w| w.get("members"))
        .and_then(toml::Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    let mut runtime = table_keys(table.get("dependencies"));
    m.dependencies.dev = table_keys(table.get("dev-dependencies")).len();
    m.dependencies.build = table_keys(table.get("build-dependencies")).len();
    if let Some(targets) = table.get("target").and_then(toml::Value::as_table) {
        for platform in targets.values() {
            runtime.extend(table_keys(platform.get("dependencies")));
            m.dependencies.dev += table_keys(platform.get("dev-dependencies")).len();
            m.dependencies.build += table_keys(platform.get("build-dependencies")).len();
        }
    }
    if package.is_none() {
        // A virtual workspace manifest: its shared dependency table is what it declares.
        runtime.extend(table_keys(workspace.and_then(|w| w.get("dependencies"))));
    }
    m.runtime_names(runtime);
    if package.is_some() {
        m.targets = cargo_targets(&table, m.name.as_deref(), dir, files);
    }
    Ok(m)
}

fn cargo_targets(table: &toml::Table, package: Option<&str>, dir: &str, files: &FileSet<'_>) -> Vec<Target> {
    let mut targets = Vec::new();
    let target = |kind, name: Option<&str>, path: Option<RelPath>| Target {
        kind,
        name: name.map(str::to_owned),
        path,
        symbol: None,
    };
    let lib = table.get("lib");
    let lib_path = str_at(lib, "path").unwrap_or("src/lib.rs");
    if lib.is_some() || files.contains(&join(dir, lib_path).unwrap_or_default()) {
        let name = str_at(lib, "name").or(package);
        targets.push(target(TargetKind::Lib, name, files.indexed(dir, lib_path)));
    }
    let declared = [
        ("bin", TargetKind::Bin),
        ("test", TargetKind::Test),
        ("bench", TargetKind::Bench),
        ("example", TargetKind::Example),
    ];
    for (key, kind) in declared {
        for entry in table.get(key).and_then(toml::Value::as_array).into_iter().flatten() {
            let name = entry.get("name").and_then(toml::Value::as_str);
            let path = entry.get("path").and_then(toml::Value::as_str);
            let resolved = path.and_then(|p| files.indexed(dir, p)).or_else(|| {
                let conventional = match (kind, name) {
                    (TargetKind::Bin, Some(n)) if Some(n) == package => "src/main.rs".to_string(),
                    (TargetKind::Bin, Some(n)) => format!("src/bin/{n}.rs"),
                    (TargetKind::Test, Some(n)) => format!("tests/{n}.rs"),
                    (TargetKind::Bench, Some(n)) => format!("benches/{n}.rs"),
                    (TargetKind::Example, Some(n)) => format!("examples/{n}.rs"),
                    _ => return None,
                };
                files.indexed(dir, &conventional)
            });
            targets.push(target(kind, name, resolved));
        }
    }
    if let Some(main) = files.indexed(dir, "src/main.rs") {
        targets.push(target(TargetKind::Bin, package, Some(main)));
    }
    let conventional_dirs = [
        ("src/bin", TargetKind::Bin),
        ("tests", TargetKind::Test),
        ("benches", TargetKind::Bench),
        ("examples", TargetKind::Example),
    ];
    for (sub, kind) in conventional_dirs {
        let Some(sub_dir) = join(dir, sub) else { continue };
        let skip = sub_dir.len() + 1;
        for file in files.under(&sub_dir) {
            let rest = &file[skip..];
            let name = match rest.split_once('/') {
                None => rest.strip_suffix(".rs"),
                Some((name, "main.rs")) => Some(name),
                Some(_) => None,
            };
            if let Some(name) = name {
                targets.push(target(kind, Some(name), Some(RelPath::from(file))));
            }
        }
    }
    let build = table.get("package").and_then(|p| p.get("build"));
    match build {
        Some(toml::Value::Boolean(false)) => {}
        Some(toml::Value::String(script)) => {
            targets.push(target(TargetKind::Build, None, files.indexed(dir, script)));
        }
        _ => {
            if let Some(script) = files.indexed(dir, "build.rs") {
                targets.push(target(TargetKind::Build, None, Some(script)));
            }
        }
    }
    // A declared target and its conventional twin are one target: keep the one with a name.
    targets.sort_by(|a, b| {
        (a.kind, &a.path)
            .cmp(&(b.kind, &b.path))
            .then_with(|| b.name.cmp(&a.name))
    });
    targets.dedup_by(|b, a| a.kind == b.kind && a.path.is_some() && a.path == b.path);
    targets
}

fn json_keys(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(serde_json::Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// `package.json`: name, version, workspaces, dependencies, and `main`, `bin`, `exports` and
/// `scripts` as targets.
fn npm(path: &str, dir: &str, text: &str, files: &FileSet<'_>) -> Result<Manifest, String> {
    let json: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let mut m = Manifest::new(path, Ecosystem::Npm);
    let str_of = |key: &str| json.get(key).and_then(serde_json::Value::as_str).map(str::to_owned);
    m.name = str_of("name");
    m.version = str_of("version");
    let workspaces = json
        .get("workspaces")
        .map(|w| w.get("packages").unwrap_or(w))
        .and_then(serde_json::Value::as_array);
    m.members = workspaces
        .map(|list| {
            list.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mut runtime = json_keys(json.get("dependencies"));
    runtime.extend(json_keys(json.get("optionalDependencies")));
    m.dependencies.dev = json_keys(json.get("devDependencies")).len() + json_keys(json.get("peerDependencies")).len();
    m.runtime_names(runtime);

    let file_target = |kind, name: Option<String>, rel: &str| Target {
        kind,
        name,
        path: files.indexed(dir, rel).or_else(|| build_source(dir, rel, files)),
        symbol: None,
    };
    if let Some(main) = json.get("main").and_then(serde_json::Value::as_str) {
        m.targets.push(file_target(TargetKind::Main, None, main));
    }
    // The source entry bundlers and type checkers read; each names the package's own code.
    for key in ["source", "module", "types", "typings"] {
        if let Some(entry) = json.get(key).and_then(serde_json::Value::as_str) {
            m.targets
                .push(file_target(TargetKind::Main, Some(key.to_owned()), entry));
        }
    }
    match json.get("bin") {
        Some(serde_json::Value::String(bin)) => m.targets.push(file_target(TargetKind::Bin, m.name.clone(), bin)),
        Some(serde_json::Value::Object(bins)) => {
            for (name, bin) in bins {
                if let Some(bin) = bin.as_str() {
                    m.targets.push(file_target(TargetKind::Bin, Some(name.clone()), bin));
                }
            }
        }
        _ => {}
    }
    let mut exports = Vec::new();
    if let Some(value) = json.get("exports") {
        export_files(value, &mut exports);
    }
    exports.sort();
    exports.dedup();
    for export in exports.into_iter().take(EXPORTS_MAX) {
        m.targets.push(file_target(TargetKind::Export, None, &export));
    }
    let scripts = json.get("scripts").and_then(serde_json::Value::as_object);
    for script in json_keys(json.get("scripts")) {
        let command = scripts
            .and_then(|scripts| scripts.get(&script))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        // Every word of the command that names an indexed script file: `node gen.js && node build.js`.
        let mut paths: Vec<RelPath> = command
            .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
            .filter(|word| is_script_file(word))
            .filter_map(|word| files.indexed(dir, word))
            .collect();
        paths.dedup();
        if paths.is_empty() {
            m.targets.push(Target {
                kind: TargetKind::Script,
                name: Some(script),
                path: None,
                symbol: None,
            });
            continue;
        }
        for path in paths {
            m.targets.push(Target {
                kind: TargetKind::Script,
                name: Some(script.clone()),
                path: Some(path),
                symbol: None,
            });
        }
    }
    Ok(m)
}

/// Extensions of a file a `package.json` script runs.
const SCRIPT_EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx", "py", "sh"];

/// Whether a word of a script command names a file it runs (`install.js`), not an argument
/// (`README.md`) or a tool (`tsc`).
fn is_script_file(word: &str) -> bool {
    word.rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && SCRIPT_EXTENSIONS.contains(&ext))
}

/// Directories a package builds its output into.
const BUILD_DIRS: &[&str] = &["dist", "lib", "build", "out", "esm", "cjs"];

/// The indexed source file a build output `rel` of the package at `dir` is compiled from:
/// `./dist/index.js` (or `.mjs`, `.cjs`, `.d.ts`) maps to `src/index.ts` (or `.tsx`, `.js`, …).
fn build_source(dir: &str, rel: &str, files: &FileSet<'_>) -> Option<RelPath> {
    let rel = rel.trim_start_matches("./");
    let (first, rest) = rel.split_once('/')?;
    if !BUILD_DIRS.contains(&first) {
        return None;
    }
    let stem = [".d.ts", ".d.mts", ".d.cts", ".js", ".mjs", ".cjs", ".jsx"]
        .iter()
        .find_map(|ext| rest.strip_suffix(ext))?;
    ["src/", ""].iter().find_map(|base| {
        ["ts", "tsx", "mts", "js", "jsx", "mjs"]
            .iter()
            .find_map(|ext| files.indexed(dir, &format!("{base}{stem}.{ext}")))
    })
}

/// The relative file paths among an `exports` value's leaves (conditions and subpaths at any depth).
fn export_files(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) if s.starts_with("./") && !s.contains('*') => out.push(s.clone()),
        serde_json::Value::Object(map) => map.values().for_each(|v| export_files(v, out)),
        serde_json::Value::Array(list) => list.iter().for_each(|v| export_files(v, out)),
        _ => {}
    }
}

/// The distribution name a PEP 508 requirement starts with, lower-cased.
fn requirement_name(requirement: &str) -> Option<String> {
    let name: String = requirement
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    (!name.is_empty()).then(|| name.to_ascii_lowercase())
}

/// `pyproject.toml`: PEP 621 `[project]` or Poetry's `[tool.poetry]`, uv workspace members, the
/// declared scripts (resolved to their module file when it is indexed), and every `__main__.py`.
fn python(path: &str, dir: &str, text: &str, files: &FileSet<'_>) -> Result<Manifest, String> {
    let table = toml_table(text)?;
    let mut m = Manifest::new(path, Ecosystem::Python);
    let project = table.get("project");
    let poetry = table.get("tool").and_then(|t| t.get("poetry"));
    m.name = str_at(project, "name").or(str_at(poetry, "name")).map(str::to_owned);
    m.version = str_at(project, "version")
        .or(str_at(poetry, "version"))
        .map(str::to_owned);
    m.members = table
        .get("tool")
        .and_then(|t| t.get("uv"))
        .and_then(|uv| uv.get("workspace"))
        .and_then(|w| w.get("members"))
        .and_then(toml::Value::as_array)
        .map(|list| list.iter().filter_map(toml::Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();

    let array_names = |value: Option<&toml::Value>| -> Vec<String> {
        value
            .and_then(toml::Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(toml::Value::as_str)
                    .filter_map(requirement_name)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut runtime = array_names(project.and_then(|p| p.get("dependencies")));
    runtime.extend(
        table_keys(poetry.and_then(|p| p.get("dependencies")))
            .into_iter()
            .filter(|name| name != "python")
            .map(|name| name.to_ascii_lowercase()),
    );
    let mut dev = 0;
    for groups in [
        project.and_then(|p| p.get("optional-dependencies")),
        table.get("dependency-groups"),
    ] {
        if let Some(groups) = groups.and_then(toml::Value::as_table) {
            dev += groups.values().map(|g| array_names(Some(g)).len()).sum::<usize>();
        }
    }
    dev += table_keys(poetry.and_then(|p| p.get("dev-dependencies"))).len();
    if let Some(groups) = poetry.and_then(|p| p.get("group")).and_then(toml::Value::as_table) {
        dev += groups
            .values()
            .map(|g| table_keys(g.get("dependencies")).len())
            .sum::<usize>();
    }
    m.dependencies.dev = dev;
    m.runtime_names(runtime);

    let script_tables = [
        project.and_then(|p| p.get("scripts")),
        project.and_then(|p| p.get("gui-scripts")),
        poetry.and_then(|p| p.get("scripts")),
    ];
    // `[project.entry-points.<group>]` and Poetry `plugins`: modules a plugin host loads by name.
    let plugin_groups = [
        project.and_then(|p| p.get("entry-points")),
        poetry.and_then(|p| p.get("plugins")),
    ];
    let plugin_tables = plugin_groups
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table)
        .flat_map(|groups| groups.values().filter_map(toml::Value::as_table));
    for scripts in script_tables
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table)
        .chain(plugin_tables)
    {
        for (name, spec) in scripts {
            let spec = spec
                .as_str()
                .or_else(|| spec.get("reference").and_then(toml::Value::as_str));
            m.targets.push(Target {
                kind: TargetKind::Script,
                name: Some(name.clone()),
                path: spec.and_then(|spec| python_module_file(dir, spec, files)),
                symbol: spec
                    .and_then(|spec| spec.split_once(':'))
                    .map(|(_, func)| func.trim().split('.').next().unwrap_or_default().to_owned())
                    .filter(|func| !func.is_empty()),
            });
        }
    }
    for file in files.under(dir) {
        if file.rsplit('/').next() == Some("__main__.py") {
            m.targets.push(Target {
                kind: TargetKind::Main,
                name: None,
                path: Some(RelPath::from(file)),
                symbol: None,
            });
        }
    }
    Ok(m)
}

/// The indexed file of the module an entry-point spec (`pkg.mod:func`) names, under `dir` or
/// `dir/src`.
fn python_module_file(dir: &str, spec: &str, files: &FileSet<'_>) -> Option<RelPath> {
    let module = spec.split(':').next()?.trim().replace('.', "/");
    if module.is_empty() {
        return None;
    }
    ["", "src/"].iter().find_map(|base| {
        files
            .indexed(dir, &format!("{base}{module}.py"))
            .or_else(|| files.indexed(dir, &format!("{base}{module}/__init__.py")))
    })
}

/// `go.mod`: the module path, the Go version as `version`, its requirements, and each directory of
/// the module whose files declare `package main` as a binary.
fn go(path: &str, dir: &str, text: &str, cx: &Context<'_, '_>) -> Manifest {
    let mut m = Manifest::new(path, Ecosystem::Go);
    let mut runtime = Vec::new();
    let mut in_require = false;
    for raw in text.lines() {
        let (code, comment) = raw.split_once("//").unwrap_or((raw, ""));
        let line = code.trim();
        let spec = if in_require {
            if line == ")" {
                in_require = false;
                continue;
            }
            line
        } else if let Some(rest) = line.strip_prefix("module ") {
            m.name = Some(rest.trim().trim_matches('"').to_owned());
            continue;
        } else if let Some(rest) = line.strip_prefix("go ") {
            m.version = Some(rest.trim().to_owned());
            continue;
        } else if let Some(rest) = line.strip_prefix("require") {
            let rest = rest.trim();
            if rest == "(" {
                in_require = true;
                continue;
            }
            rest
        } else {
            continue;
        };
        let Some(module) = spec.split_whitespace().next() else {
            continue;
        };
        if comment.trim() == "indirect" {
            m.dependencies.indirect += 1;
        } else {
            runtime.push(module.trim_matches('"').to_owned());
        }
    }
    m.runtime_names(runtime);
    let (bins, partial) = go_main_packages(dir, cx);
    m.targets = bins;
    m.targets_partial = partial;
    m
}

/// The directories of the module at `dir` holding a non-test `.go` file in `package main`, and
/// whether [`GO_FILES_READ_MAX`] stopped the search.
fn go_main_packages(dir: &str, cx: &Context<'_, '_>) -> (Vec<Target>, bool) {
    let nested = |file: &str| {
        cx.go_modules
            .iter()
            .any(|module| *module != dir && module.len() > dir.len() && file.starts_with(&dir_prefix(module)))
    };
    let mut mains: Vec<&str> = Vec::new();
    let mut read = 0usize;
    for file in cx.files.under(dir) {
        if !file.ends_with(".go") || file.ends_with("_test.go") || nested(file) {
            continue;
        }
        let package_dir = parent_dir(file);
        if mains.last() == Some(&package_dir) {
            continue;
        }
        if read == GO_FILES_READ_MAX {
            return (go_bins(mains), true);
        }
        read += 1;
        if (cx.read)(file, GO_HEAD_BYTES).is_some_and(|head| go_package(&head) == Some("main")) {
            mains.push(package_dir);
        }
    }
    (go_bins(mains), false)
}

fn go_bins(dirs: Vec<&str>) -> Vec<Target> {
    dirs.into_iter()
        .map(|d| Target {
            kind: TargetKind::Bin,
            name: Some(d.rsplit('/').next().unwrap_or(d).to_owned()).filter(|n| !n.is_empty()),
            path: Some(RelPath::from(if d.is_empty() { "." } else { d })),
            symbol: None,
        })
        .collect()
}

/// The package a Go file's head declares, skipping comments and blank lines before the clause.
pub(crate) fn go_package(head: &str) -> Option<&str> {
    let mut in_block = false;
    for line in head.lines() {
        let mut line = line.trim();
        if in_block {
            let Some(end) = line.find("*/") else { continue };
            line = line[end + 2..].trim();
            in_block = false;
        }
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("/*") {
            in_block = !rest.contains("*/");
            continue;
        }
        return line
            .strip_prefix("package ")
            .and_then(|rest| rest.split_whitespace().next());
    }
    None
}

static XML_COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").expect("static regex"));
/// Blocks of a POM whose `artifactId`, `version` and dependencies are not the project's own.
static POM_NESTED: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        "parent",
        "dependencyManagement",
        "build",
        "profiles",
        "reporting",
        "pluginRepositories",
        "repositories",
        "distributionManagement",
    ]
    .iter()
    .map(|tag| Regex::new(&format!(r"(?s)<{tag}>.*?</{tag}>")).expect("static regex"))
    .collect()
});
static POM_DEPENDENCY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<dependency>(.*?)</dependency>").expect("static regex"));

/// The text of the first `<tag>` element in `xml`.
fn xml_text<'t>(xml: &'t str, tag: &str) -> Option<&'t str> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(xml[start..end].trim())
}

/// `pom.xml`, read lexically: the project's own `artifactId` and `version` (not its parent's),
/// `<module>` members, `<dependency>` counts with `test` scope as dev, and a `mainClass`.
fn maven(path: &str, text: &str) -> Manifest {
    let mut m = Manifest::new(path, Ecosystem::Maven);
    let text = XML_COMMENT.replace_all(text, "");
    let top = POM_NESTED
        .iter()
        .fold(text.to_string(), |top, block| block.replace_all(&top, "").into_owned());
    m.name = xml_text(&top, "artifactId").map(str::to_owned);
    m.version = xml_text(&top, "version").map(str::to_owned);
    let mut modules = &top[..];
    while let Some(module) = xml_text(modules, "module") {
        m.members.push(module.to_owned());
        let at = modules
            .find("</module>")
            .map_or(modules.len(), |i| i + "</module>".len());
        modules = &modules[at..];
    }
    let mut runtime = Vec::new();
    for dependency in POM_DEPENDENCY.captures_iter(&top) {
        let body = dependency.get(1).map_or("", |b| b.as_str());
        if matches!(xml_text(body, "scope"), Some("test" | "provided")) {
            m.dependencies.dev += 1;
        } else if let Some(artifact) = xml_text(body, "artifactId") {
            runtime.push(artifact.to_owned());
        }
    }
    m.runtime_names(runtime);
    if let Some(main) = xml_text(&text, "mainClass") {
        m.targets.push(Target {
            kind: TargetKind::Main,
            name: Some(main.to_owned()),
            path: None,
            symbol: None,
        });
    }
    m
}

static GRADLE_DEPENDENCY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?m)^\s*(implementation|api|compileOnly|runtimeOnly|compile|runtime|kapt|ksp|annotationProcessor|testImplementation|testCompileOnly|testRuntimeOnly|testCompile|androidTestImplementation|debugImplementation)\s*[\(\s]\s*(?:platform\s*\(\s*)?['"]([^'"]+)['"]"#,
    )
    .expect("static regex")
});
static GRADLE_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*version\s*=?\s*['"]([^'"]+)['"]"#).expect("static regex"));
static GRADLE_MAIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"mainClass(?:Name)?\s*(?:=|\.set\s*\()\s*['"]([^'"]+)['"]"#).expect("static regex"));
static GRADLE_ROOT_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"rootProject\.name\s*=\s*['"]([^'"]+)['"]"#).expect("static regex"));
static GRADLE_INCLUDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*include\s*\(?([^\n)]*)"#).expect("static regex"));
static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"['"]([^'"]+)['"]"#).expect("static regex"));

/// `build.gradle[.kts]`, read lexically: string-notation dependencies by configuration (`test*` and
/// `androidTest*` as dev), `version`, `mainClass`, and the name and `include`d projects of the
/// `settings.gradle[.kts]` beside it.
fn gradle(path: &str, dir: &str, text: &str, cx: &Context<'_, '_>) -> Manifest {
    let mut m = Manifest::new(path, Ecosystem::Gradle);
    m.version = GRADLE_VERSION.captures(text).map(|c| c[1].to_owned());
    let mut runtime = Vec::new();
    for dependency in GRADLE_DEPENDENCY.captures_iter(text) {
        if dependency[1].starts_with("test") || dependency[1].starts_with("androidTest") {
            m.dependencies.dev += 1;
        } else {
            let coordinate = &dependency[2];
            let mut parts = coordinate.split(':');
            let name = parts.nth(1).unwrap_or(coordinate);
            runtime.push(name.to_owned());
        }
    }
    m.runtime_names(runtime);
    if let Some(main) = GRADLE_MAIN.captures(text) {
        m.targets.push(Target {
            kind: TargetKind::Main,
            name: Some(main[1].to_owned()),
            path: None,
            symbol: None,
        });
    }
    let settings = ["settings.gradle", "settings.gradle.kts"]
        .iter()
        .filter_map(|name| join(dir, name))
        .find_map(|p| (cx.read)(&p, MANIFEST_BYTES_MAX as usize));
    if let Some(settings) = settings {
        m.name = GRADLE_ROOT_NAME.captures(&settings).map(|c| c[1].to_owned());
        for include in GRADLE_INCLUDE.captures_iter(&settings) {
            m.members
                .extend(QUOTED.captures_iter(&include[1]).map(|c| c[1].to_owned()));
        }
    }
    m
}

#[cfg(test)]
#[path = "manifests_tests.rs"]
mod tests;
