//! The dependencies a repository's manifests and lockfiles name, per ecosystem (ADR-0049): which
//! packages are direct, and the version each resolves to when the lockfile says so.

use std::path::Path;

use ahash::{AHashMap, AHashSet};
use serde::Deserialize;

use super::{DepUnavailable, Ecosystem, REASON_UNSUPPORTED_SOURCE};

/// Largest manifest or lockfile read; a larger one is skipped as if absent.
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

/// One dependency a manifest names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Wanted {
    pub ecosystem: Ecosystem,
    pub name: String,
    /// The version the lockfile resolves, when there is one.
    pub version: Option<String>,
}

/// Every dependency of the repository at `root`, sorted, and those whose source basemind does not
/// read (a git or path dependency).
pub(super) fn wanted(root: &Path, direct_only: bool) -> (Vec<Wanted>, Vec<DepUnavailable>) {
    let mut out = Vec::new();
    let mut unsupported = Vec::new();
    cargo(root, direct_only, &mut out, &mut unsupported);
    npm(root, direct_only, &mut out);
    python(root, direct_only, &mut out);
    go(root, direct_only, &mut out);
    out.sort();
    out.dedup();
    (out, unsupported)
}

pub(super) fn read_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_MANIFEST_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

#[derive(Deserialize)]
struct CargoLock {
    #[serde(default)]
    package: Vec<CargoPackage>,
}

#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    version: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

fn is_registry(source: &str) -> bool {
    source.starts_with("registry+") || source.starts_with("sparse+")
}

/// `Cargo.toml` with `Cargo.lock`. The lockfile's packages without a `source` are the workspace's
/// own (members and path dependencies); what they depend on is direct.
fn cargo(root: &Path, direct_only: bool, out: &mut Vec<Wanted>, unsupported: &mut Vec<DepUnavailable>) {
    if !root.join("Cargo.toml").is_file() {
        return;
    }
    let Some(lock) = read_text(&root.join("Cargo.lock")).and_then(|text| toml::from_str::<CargoLock>(&text).ok())
    else {
        return;
    };
    let mut push = |package: &CargoPackage| match package.source.as_deref() {
        None => {}
        Some(source) if is_registry(source) => out.push(Wanted {
            ecosystem: Ecosystem::Cargo,
            name: package.name.clone(),
            version: Some(package.version.clone()),
        }),
        Some(_) => unsupported.push(DepUnavailable::new(
            Ecosystem::Cargo,
            &package.name,
            Some(&package.version),
            REASON_UNSUPPORTED_SOURCE,
        )),
    };
    if !direct_only {
        lock.package.iter().for_each(&mut push);
        return;
    }
    for local in lock.package.iter().filter(|package| package.source.is_none()) {
        for entry in &local.dependencies {
            let mut parts = entry.split_whitespace();
            let Some(name) = parts.next() else { continue };
            let version = parts.next();
            let mut matching = lock
                .package
                .iter()
                .filter(|package| package.name == name && version.is_none_or(|v| package.version == v));
            if let (Some(package), None) = (matching.next(), matching.next()) {
                push(package);
            }
        }
    }
}

/// `package.json`, with `package-lock.json` for versions and, when `direct_only` is off, the
/// transitive packages. `pnpm-lock.yaml` is not read: the installed `package.json` gives the version.
fn npm(root: &Path, direct_only: bool, out: &mut Vec<Wanted>) {
    let Some(manifest) =
        read_text(&root.join("package.json")).and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    else {
        return;
    };
    let lock = read_text(&root.join("package-lock.json"))
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let locked = lock.as_ref().map(npm_locked).unwrap_or_default();
    let mut names: AHashSet<String> = AHashSet::new();
    for table in [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ] {
        if let Some(map) = manifest.get(table).and_then(serde_json::Value::as_object) {
            names.extend(map.keys().cloned());
        }
    }
    if !direct_only {
        names.extend(locked.keys().cloned());
    }
    out.extend(names.into_iter().map(|name| Wanted {
        ecosystem: Ecosystem::Npm,
        version: locked.get(&name).cloned(),
        name,
    }));
}

/// Top-level package versions of a `package-lock.json` (v2/v3 `packages`, else v1 `dependencies`).
fn npm_locked(lock: &serde_json::Value) -> AHashMap<String, String> {
    let mut out = AHashMap::new();
    let version = |entry: &serde_json::Value| {
        entry
            .get("version")
            .and_then(serde_json::Value::as_str)
            .map(String::from)
    };
    if let Some(packages) = lock.get("packages").and_then(serde_json::Value::as_object) {
        for (key, entry) in packages {
            let Some(name) = key.strip_prefix("node_modules/") else {
                continue;
            };
            if name.contains("/node_modules/") {
                continue;
            }
            if let Some(version) = version(entry) {
                out.insert(name.to_string(), version);
            }
        }
    } else if let Some(deps) = lock.get("dependencies").and_then(serde_json::Value::as_object) {
        for (name, entry) in deps {
            if let Some(version) = version(entry) {
                out.insert(name.clone(), version);
            }
        }
    }
    out
}

/// PEP 503 normalisation: lower case, every run of `-`, `_` and `.` one `-`.
pub(super) fn normalize_python(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut separator = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            separator = true;
            continue;
        }
        if separator && !out.is_empty() {
            out.push('-');
        }
        separator = false;
        out.extend(c.to_lowercase());
    }
    out
}

/// The distribution name a PEP 508 requirement starts with.
fn requirement_name(requirement: &str) -> Option<String> {
    let name: String = requirement
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    (!name.is_empty()).then(|| normalize_python(&name))
}

/// `pyproject.toml` (PEP 621 tables, `[dependency-groups]` and Poetry's tables), with `uv.lock` or
/// `poetry.lock` for versions and, when `direct_only` is off, the transitive packages.
fn python(root: &Path, direct_only: bool, out: &mut Vec<Wanted>) {
    let Some(manifest) =
        read_text(&root.join("pyproject.toml")).and_then(|text| toml::from_str::<toml::Table>(&text).ok())
    else {
        return;
    };
    let mut names: AHashSet<String> = AHashSet::new();
    let requirements = |value: Option<&toml::Value>, names: &mut AHashSet<String>| {
        let Some(list) = value.and_then(toml::Value::as_array) else {
            return;
        };
        names.extend(list.iter().filter_map(toml::Value::as_str).filter_map(requirement_name));
    };
    let project = manifest.get("project");
    requirements(project.and_then(|p| p.get("dependencies")), &mut names);
    for table in [
        project.and_then(|p| p.get("optional-dependencies")),
        manifest.get("dependency-groups"),
    ] {
        if let Some(groups) = table.and_then(toml::Value::as_table) {
            for group in groups.values() {
                requirements(Some(group), &mut names);
            }
        }
    }
    let poetry = manifest.get("tool").and_then(|tool| tool.get("poetry"));
    let mut poetry_tables: Vec<&toml::Value> = ["dependencies", "dev-dependencies"]
        .iter()
        .filter_map(|table| poetry.and_then(|p| p.get(*table)))
        .collect();
    if let Some(groups) = poetry.and_then(|p| p.get("group")).and_then(toml::Value::as_table) {
        poetry_tables.extend(groups.values().filter_map(|group| group.get("dependencies")));
    }
    for table in poetry_tables.into_iter().filter_map(toml::Value::as_table) {
        names.extend(
            table
                .keys()
                .filter(|key| *key != "python")
                .map(|key| normalize_python(key)),
        );
    }
    let locked = python_locked(root);
    if !direct_only {
        names.extend(locked.keys().cloned());
    }
    out.extend(names.into_iter().map(|name| Wanted {
        ecosystem: Ecosystem::Pypi,
        version: locked.get(&name).cloned(),
        name,
    }));
}

/// Package versions of `uv.lock` or `poetry.lock`, by normalised name. The project's own entry in
/// `uv.lock` (an editable or virtual source) is left out.
fn python_locked(root: &Path) -> AHashMap<String, String> {
    let mut out = AHashMap::new();
    for lockfile in ["uv.lock", "poetry.lock"] {
        let Some(lock) = read_text(&root.join(lockfile)).and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        else {
            continue;
        };
        let packages = lock.get("package").and_then(toml::Value::as_array);
        for package in packages.into_iter().flatten() {
            let local = package
                .get("source")
                .and_then(toml::Value::as_table)
                .is_some_and(|source| source.contains_key("editable") || source.contains_key("virtual"));
            let name = package.get("name").and_then(toml::Value::as_str);
            let version = package.get("version").and_then(toml::Value::as_str);
            if let (Some(name), Some(version), false) = (name, version, local) {
                out.insert(normalize_python(name), version.to_string());
            }
        }
        if !out.is_empty() {
            break;
        }
    }
    out
}

/// `go.mod`'s `require` directives; a requirement marked `// indirect` is transitive.
fn go(root: &Path, direct_only: bool, out: &mut Vec<Wanted>) {
    let Some(text) = read_text(&root.join("go.mod")) else {
        return;
    };
    let mut in_block = false;
    for line in text.lines() {
        let (code, comment) = line.split_once("//").unwrap_or((line, ""));
        let code = code.trim();
        let spec = if in_block {
            if code == ")" {
                in_block = false;
                continue;
            }
            code
        } else if let Some(rest) = code.strip_prefix("require") {
            let rest = rest.trim();
            if rest == "(" {
                in_block = true;
                continue;
            }
            rest
        } else {
            continue;
        };
        let mut parts = spec.split_whitespace();
        let (Some(module), Some(version)) = (parts.next(), parts.next()) else {
            continue;
        };
        if direct_only && comment.trim() == "indirect" {
            continue;
        }
        out.push(Wanted {
            ecosystem: Ecosystem::Go,
            name: module.to_string(),
            version: Some(version.to_string()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, name: &str, text: &str) {
        std::fs::write(root.join(name), text).expect("write fixture");
    }

    fn names(wanted: &[Wanted], ecosystem: Ecosystem) -> Vec<(String, Option<String>)> {
        wanted
            .iter()
            .filter(|w| w.ecosystem == ecosystem)
            .map(|w| (w.name.clone(), w.version.clone()))
            .collect()
    }

    #[test]
    fn cargo_direct_dependencies_come_from_the_workspace_packages_of_the_lockfile() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "Cargo.toml", "[package]\nname = \"app\"\n");
        write(
            dir.path(),
            "Cargo.lock",
            "version = 4\n\
             [[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"serde\", \"rand 0.8.5\", \"gitdep\", \"member\"]\n\
             [[package]]\nname = \"member\"\nversion = \"0.1.0\"\n\
             [[package]]\nname = \"serde\"\nversion = \"1.0.200\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\ndependencies = [\"serde_derive\"]\n\
             [[package]]\nname = \"serde_derive\"\nversion = \"1.0.200\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
             [[package]]\nname = \"rand\"\nversion = \"0.8.5\"\nsource = \"sparse+https://index.crates.io/\"\n\
             [[package]]\nname = \"rand\"\nversion = \"0.9.0\"\nsource = \"sparse+https://index.crates.io/\"\n\
             [[package]]\nname = \"gitdep\"\nversion = \"0.2.0\"\nsource = \"git+https://example.com/gitdep#abc\"\n",
        );
        let (wanted, unsupported) = wanted(dir.path(), true);
        assert_eq!(
            names(&wanted, Ecosystem::Cargo),
            vec![
                ("rand".to_string(), Some("0.8.5".to_string())),
                ("serde".to_string(), Some("1.0.200".to_string())),
            ]
        );
        assert_eq!(unsupported.len(), 1);
        assert_eq!(unsupported[0].name, "gitdep");

        let (all, _) = super::wanted(dir.path(), false);
        assert_eq!(names(&all, Ecosystem::Cargo).len(), 4, "every registry package");
    }

    #[test]
    fn npm_direct_dependencies_come_from_package_json_with_lock_versions() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"dependencies":{"react":"^18"},"devDependencies":{"@types/node":"^20"}}"#,
        );
        write(
            dir.path(),
            "package-lock.json",
            r#"{"lockfileVersion":3,"packages":{"":{},"node_modules/react":{"version":"18.3.1"},
                "node_modules/loose-envify":{"version":"1.4.0"},
                "node_modules/react/node_modules/nested":{"version":"9.9.9"}}}"#,
        );
        let (wanted, _) = wanted(dir.path(), true);
        assert_eq!(
            names(&wanted, Ecosystem::Npm),
            vec![
                ("@types/node".to_string(), None),
                ("react".to_string(), Some("18.3.1".to_string())),
            ]
        );
        let (all, _) = super::wanted(dir.path(), false);
        assert_eq!(
            names(&all, Ecosystem::Npm).len(),
            3,
            "plus the top-level lock packages, never a nested one"
        );
    }

    #[test]
    fn python_direct_dependencies_come_from_pyproject_with_lock_versions() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "pyproject.toml",
            "[project]\nname = \"app\"\ndependencies = [\"Requests>=2\", \"typing_extensions; python_version<'3.11'\"]\n\
             [project.optional-dependencies]\ncli = [\"click\"]\n\
             [dependency-groups]\ndev = [\"pytest>=8\", { include-group = \"cli\" }]\n\
             [tool.poetry.dependencies]\npython = \"^3.11\"\nRich = \"*\"\n",
        );
        write(
            dir.path(),
            "uv.lock",
            "[[package]]\nname = \"app\"\nversion = \"0.1.0\"\nsource = { editable = \".\" }\n\
             [[package]]\nname = \"requests\"\nversion = \"2.32.3\"\n\
             [[package]]\nname = \"urllib3\"\nversion = \"2.2.0\"\n",
        );
        let (wanted, _) = wanted(dir.path(), true);
        assert_eq!(
            names(&wanted, Ecosystem::Pypi),
            vec![
                ("click".to_string(), None),
                ("pytest".to_string(), None),
                ("requests".to_string(), Some("2.32.3".to_string())),
                ("rich".to_string(), None),
                ("typing-extensions".to_string(), None),
            ]
        );
        let (all, _) = super::wanted(dir.path(), false);
        assert!(
            names(&all, Ecosystem::Pypi).iter().any(|(name, _)| name == "urllib3"),
            "transitive lock packages"
        );
        assert!(
            !names(&all, Ecosystem::Pypi).iter().any(|(name, _)| name == "app"),
            "the project itself is not a dependency"
        );
    }

    #[test]
    fn go_requirements_marked_indirect_are_transitive() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "go.mod",
            "module example.com/app\n\ngo 1.22\n\nrequire github.com/pkg/errors v0.9.1\n\
             require (\n\tgithub.com/BurntSushi/toml v1.3.2\n\tgolang.org/x/sys v0.20.0 // indirect\n)\n",
        );
        let (wanted, _) = wanted(dir.path(), true);
        assert_eq!(
            names(&wanted, Ecosystem::Go),
            vec![
                ("github.com/BurntSushi/toml".to_string(), Some("v1.3.2".to_string())),
                ("github.com/pkg/errors".to_string(), Some("v0.9.1".to_string())),
            ]
        );
        let (all, _) = super::wanted(dir.path(), false);
        assert_eq!(names(&all, Ecosystem::Go).len(), 3);
    }

    #[test]
    fn python_names_are_normalised() {
        assert_eq!(normalize_python("Typing__Extensions"), "typing-extensions");
        assert_eq!(normalize_python("zope.interface"), "zope-interface");
        assert_eq!(requirement_name(" Django[argon2] >= 5"), Some("django".to_string()));
        assert_eq!(requirement_name(">=1"), None);
    }
}
