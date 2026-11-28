//! What a `Cargo.toml` tells the Rust module resolver ([`crate::intel::module_rust`]): the crate
//! roots its targets declare, and which crate a name at the head of a `use` path is.
//!
//! A path that starts neither in the importer's own crate (`crate::`, `self::`, `super::`, a module
//! the file declares) nor at a name the file binds names a crate. That crate is:
//!
//! - the standard library (`std`, `core`, `alloc`, `proc_macro`, `test`): external;
//! - the package's own library, from one of its binaries, tests, examples or benches (`use demo::x`
//!   in `tests/it.rs` of the package `demo`): a crate of the repository;
//! - a declared dependency (`[dependencies]`, `[dev-dependencies]`, `[build-dependencies]` and their
//!   `[target.….*]` forms, under the name code uses: the key with `-` read as `_`). A path
//!   dependency inside the repository, a `workspace = true` one whose `[workspace.dependencies]`
//!   entry has a path, or one the workspace `[patch.*]`es to a path, is a crate of the repository
//!   (a workspace member such as `crates/agents`); any other is external;
//! - anything else: unresolved, since a name that is neither in scope nor a crate the manifest
//!   declares cannot compile, so the resolver failed to place it.

use ahash::AHashMap;

use crate::intel::module_resolver::join;

/// Crates every Rust crate can name without declaring them.
pub(crate) const STD_CRATES: [&str; 5] = ["std", "core", "alloc", "proc_macro", "test"];

/// Where a dependency's source lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencySource {
    /// A registry or git dependency, unless the workspace patches it to a path.
    Registry,
    /// A path dependency: the repo-relative directory of its package.
    Path(String),
    /// A path dependency outside the repository.
    Outside,
    /// `workspace = true`: the workspace's `[workspace.dependencies]` entry says.
    Workspace,
}

/// A declared dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// The key as written, which `[workspace.dependencies]` is keyed by too.
    pub key: String,
    /// The package name (`package = "…"`, else the key).
    pub package: String,
    pub source: DependencySource,
}

/// The `[workspace]` side of a manifest: path dependencies it declares and patches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceSection {
    /// `[workspace.dependencies]` by key: the package name and its source (never `Workspace`).
    pub dependencies: AHashMap<String, Dependency>,
    /// `[patch.*]` entries with a path in the repository: package name -> package directory.
    pub patches: AHashMap<String, String>,
}

/// What the resolver reads from one `Cargo.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    /// The repo-relative root files of the targets declaring a `path`.
    pub targets: Vec<String>,
    /// The library's root file when `[lib]` declares a `path`.
    pub lib_path: Option<String>,
    /// The name the package's library has in code (`[lib] name`, else the package name with `-`
    /// read as `_`); `None` for a manifest with no `[package]`.
    pub lib_name: Option<String>,
    /// Declared dependencies by the name code uses.
    pub dependencies: AHashMap<String, Dependency>,
    /// The `[workspace]` side, when the manifest has one.
    pub workspace: Option<WorkspaceSection>,
}

/// Which crate a name at the head of a `use` path is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrateRef {
    /// The standard library or a dependency outside the repository.
    External,
    /// A crate of the repository: the directory of its package.
    Local(String),
    /// Neither: a name no manifest declares.
    Unknown,
}

/// The name a crate has in code: `-` read as `_`.
pub(crate) fn code_name(name: &str) -> String {
    name.replace('-', "_")
}

/// Parse the `Cargo.toml` in `manifest_dir`. A manifest that does not parse declares nothing.
pub(crate) fn parse_manifest(text: &str, manifest_dir: &str) -> Manifest {
    let Ok(manifest) = toml::from_str::<toml::Table>(text) else {
        return Manifest::default();
    };
    fn path_of(target: &toml::Value) -> Option<&str> {
        target.get("path").and_then(toml::Value::as_str)
    }
    let resolve = |path: &str| normalize(&join(manifest_dir, path));
    let lib_path = manifest.get("lib").and_then(path_of).and_then(resolve);
    let mut targets: Vec<String> = lib_path.iter().cloned().collect();
    for kind in ["bin", "test", "example", "bench"] {
        let declared = manifest.get(kind).and_then(toml::Value::as_array);
        targets.extend(declared.into_iter().flatten().filter_map(path_of).filter_map(resolve));
    }
    let package = manifest.get("package");
    let build = package.and_then(|package| package.get("build"));
    targets.extend(build.and_then(toml::Value::as_str).and_then(resolve));

    let package_name = package
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str);
    let lib_name = manifest
        .get("lib")
        .and_then(|lib| lib.get("name"))
        .and_then(toml::Value::as_str)
        .or(package_name)
        .map(code_name);

    let mut dependencies = AHashMap::new();
    let mut tables: Vec<&toml::Value> = Vec::new();
    let sections = ["dependencies", "dev-dependencies", "build-dependencies"];
    tables.extend(sections.iter().filter_map(|section| manifest.get(*section)));
    let platforms = manifest.get("target").and_then(toml::Value::as_table);
    for platform in platforms.into_iter().flat_map(|platforms| platforms.values()) {
        tables.extend(sections.iter().filter_map(|section| platform.get(*section)));
    }
    for table in tables.into_iter().filter_map(toml::Value::as_table) {
        for (key, spec) in table {
            dependencies.insert(code_name(key), dependency(key, spec, manifest_dir));
        }
    }

    let workspace = manifest.get("workspace").map(|workspace| {
        let declared = workspace.get("dependencies").and_then(toml::Value::as_table);
        let dependencies = declared
            .into_iter()
            .flatten()
            .map(|(key, spec)| (key.clone(), dependency(key, spec, manifest_dir)))
            .collect();
        let patches = manifest
            .get("patch")
            .and_then(toml::Value::as_table)
            .into_iter()
            .flat_map(|registries| registries.values().filter_map(toml::Value::as_table))
            .flatten()
            .filter_map(|(key, spec)| match dependency(key, spec, manifest_dir) {
                Dependency {
                    package,
                    source: DependencySource::Path(dir),
                    ..
                } => Some((package, dir)),
                _ => None,
            })
            .collect();
        WorkspaceSection { dependencies, patches }
    });

    Manifest {
        targets,
        lib_path,
        lib_name,
        dependencies,
        workspace,
    }
}

/// One dependency entry: `name = "1"` or `name = { package, path, workspace, … }`.
fn dependency(key: &str, spec: &toml::Value, manifest_dir: &str) -> Dependency {
    let field = |name: &str| spec.get(name);
    let package = field("package")
        .and_then(toml::Value::as_str)
        .unwrap_or(key)
        .to_string();
    let source = if let Some(path) = field("path").and_then(toml::Value::as_str) {
        normalize(&join(manifest_dir, path)).map_or(DependencySource::Outside, DependencySource::Path)
    } else if field("workspace").and_then(toml::Value::as_bool) == Some(true) {
        DependencySource::Workspace
    } else {
        DependencySource::Registry
    };
    Dependency {
        key: key.to_string(),
        package,
        source,
    }
}

/// `path` with `.` and `..` segments applied, or `None` when it leaves the repository.
pub(crate) fn normalize(path: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            _ => segments.push(segment),
        }
    }
    Some(segments.join("/"))
}

/// Which crate `name` is for code of the package in `manifest_dir` described by `manifest`.
/// `workspace` is the section of the workspace holding the package, when there is one; it settles
/// `workspace = true` dependencies and patched ones.
pub(crate) fn classify_crate(
    name: &str,
    manifest_dir: &str,
    manifest: &Manifest,
    workspace: Option<&WorkspaceSection>,
) -> CrateRef {
    if STD_CRATES.contains(&name) {
        return CrateRef::External;
    }
    if manifest.lib_name.as_deref() == Some(name) {
        return CrateRef::Local(manifest_dir.to_string());
    }
    let Some(dependency) = manifest.dependencies.get(name) else {
        return CrateRef::Unknown;
    };
    let patched = |package: &str| {
        workspace
            .and_then(|workspace| workspace.patches.get(package))
            .map_or(CrateRef::External, |dir| CrateRef::Local(dir.clone()))
    };
    match &dependency.source {
        DependencySource::Path(dir) => CrateRef::Local(dir.clone()),
        DependencySource::Outside => CrateRef::External,
        DependencySource::Registry => patched(&dependency.package),
        DependencySource::Workspace => {
            let declared = workspace.and_then(|workspace| workspace.dependencies.get(&dependency.key));
            match declared.map(|declared| &declared.source) {
                Some(DependencySource::Path(dir)) => CrateRef::Local(dir.clone()),
                Some(DependencySource::Outside) => CrateRef::External,
                _ => patched(&dependency.package),
            }
        }
    }
}

/// The root file of the library of the package in `package_dir`: its `[lib] path`, else
/// `src/lib.rs`.
pub(crate) fn lib_root(package_dir: &str, manifest: Option<&Manifest>) -> String {
    manifest
        .and_then(|manifest| manifest.lib_path.clone())
        .unwrap_or_else(|| join(package_dir, "src/lib.rs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "[package]\nname = \"demo-app\"\n\n[lib]\npath = \"src/lib.rs\"\n\n\
                        [dependencies]\nserde = \"1\"\nagents = { package = \"demo-agents\", path = \"crates/agents\" }\n\
                        shared.workspace = true\nfar = { path = \"../far\" }\ntokio = \"1\"\n\n\
                        [dev-dependencies]\nproptest = \"1\"\n\n\
                        [target.'cfg(unix)'.dependencies]\nlibc = \"0.2\"\n\n\
                        [workspace]\nmembers = [\"crates/*\"]\n\n\
                        [workspace.dependencies]\nshared = { path = \"crates/shared\" }\n\n\
                        [patch.crates-io]\ntokio = { path = \"vendor/tokio\" }\n";

    #[test]
    fn a_manifest_declares_dependencies_under_their_code_names() {
        let manifest = parse_manifest(ROOT, "");
        assert_eq!(manifest.lib_name.as_deref(), Some("demo_app"));
        assert_eq!(manifest.lib_path.as_deref(), Some("src/lib.rs"));
        let mut names: Vec<&str> = manifest.dependencies.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["agents", "far", "libc", "proptest", "serde", "shared", "tokio"]);
        assert_eq!(
            manifest.dependencies["agents"],
            Dependency {
                key: "agents".to_string(),
                package: "demo-agents".to_string(),
                source: DependencySource::Path("crates/agents".to_string()),
            }
        );
        assert_eq!(manifest.dependencies["far"].source, DependencySource::Outside);
        assert_eq!(manifest.dependencies["shared"].source, DependencySource::Workspace);
        let workspace = manifest.workspace.as_ref().unwrap();
        assert_eq!(workspace.patches["tokio"], "vendor/tokio");
        assert_eq!(
            workspace.dependencies["shared"].source,
            DependencySource::Path("crates/shared".to_string())
        );
        let member = parse_manifest("[package]\nname = \"x\"\n[lib]\nname = \"renamed\"\n", "crates/x");
        assert_eq!(member.lib_name.as_deref(), Some("renamed"));
        assert_eq!(parse_manifest("not toml [", ""), Manifest::default());
    }

    #[test]
    fn crate_names_classify_as_std_dependency_workspace_crate_or_unknown() {
        let manifest = parse_manifest(ROOT, "");
        let workspace = manifest.workspace.clone();
        let classify = |name: &str| classify_crate(name, "", &manifest, workspace.as_ref());
        for std in STD_CRATES {
            assert_eq!(classify(std), CrateRef::External, "{std}");
        }
        assert_eq!(classify("serde"), CrateRef::External);
        assert_eq!(classify("proptest"), CrateRef::External, "a dev-dependency");
        assert_eq!(classify("libc"), CrateRef::External, "a target dependency");
        assert_eq!(classify("far"), CrateRef::External, "a path outside the repository");
        assert_eq!(
            classify("demo_app"),
            CrateRef::Local(String::new()),
            "the package's own library"
        );
        assert_eq!(classify("agents"), CrateRef::Local("crates/agents".to_string()));
        assert_eq!(classify("shared"), CrateRef::Local("crates/shared".to_string()));
        assert_eq!(
            classify("tokio"),
            CrateRef::Local("vendor/tokio".to_string()),
            "patched"
        );
        assert_eq!(classify("undeclared"), CrateRef::Unknown);
        assert_eq!(
            classify_crate("tokio", "", &manifest, None),
            CrateRef::External,
            "without the workspace no patch applies"
        );
    }

    #[test]
    fn a_library_root_is_its_declared_path_or_the_standard_one() {
        let member = parse_manifest("[package]\nname = \"x\"\n[lib]\npath = \"lib/x.rs\"\n", "crates/x");
        assert_eq!(lib_root("crates/x", Some(&member)), "crates/x/lib/x.rs");
        assert_eq!(lib_root("crates/y", None), "crates/y/src/lib.rs");
    }
}
