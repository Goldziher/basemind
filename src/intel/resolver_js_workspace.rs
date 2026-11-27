//! JS/TS workspace packages, found from the manifests alone.
//!
//! A pnpm, npm or yarn monorepo imports its own packages by name (`@acme/ui`). The package manager
//! makes that work by linking each package into `node_modules`; before an install, or in a checkout
//! that never installs, nothing links them and the import has no target. The workspace
//! declaration says where the packages are, so the name is mapped to the package directory here,
//! with no `node_modules`:
//!
//! - the `packages:` globs of the root `pnpm-workspace.yaml`, and the `workspaces` field of the
//!   root `package.json` (an array, or an object with a `packages` array); a `!` glob excludes;
//! - every directory those globs match that holds a `package.json` with a `name`.
//!
//! The packages are found once per resolver, by walking the repository. A package added or renamed
//! after that walk is seen by the next stitch, not by the importers of the current one.

use std::collections::BTreeMap;
use std::path::Path;

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde_json::Value;

/// The root files the workspace declaration is read from.
pub(super) const PNPM_WORKSPACE: &str = "pnpm-workspace.yaml";
pub(super) const ROOT_MANIFEST: &str = "package.json";

/// The fields of a package manifest that name its entry point, most source-like first.
const ENTRY_FIELDS: &[&str] = &["source", "types", "module", "main"];

/// The `exports` conditions that lead to a source or type file, in the order they are preferred.
const EXPORT_CONDITIONS: &[&str] = &["source", "types", "import", "default", "require"];

/// The workspace packages of a repository: name to repo-relative, forward-slashed directory.
#[derive(Default)]
pub(super) struct Workspace {
    packages: BTreeMap<String, String>,
}

impl Workspace {
    /// Read the workspace declaration under `root` and map every package it names.
    pub(super) fn discover(root: &Path) -> Self {
        let Some(matcher) = DirMatcher::declared(root) else {
            return Self::default();
        };
        let mut packages = BTreeMap::new();
        let manifests = ignore::WalkBuilder::new(root)
            .filter_entry(|entry| entry.file_name() != "node_modules")
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .filter(|entry| entry.file_name() == ROOT_MANIFEST);
        for entry in manifests {
            let Some(dir) = entry
                .path()
                .parent()
                .and_then(|dir| dir.strip_prefix(root).ok())
                .and_then(Path::to_str)
                .map(|dir| dir.replace('\\', "/"))
            else {
                continue;
            };
            if !matcher.matches(&dir) {
                continue;
            }
            if let Some(name) = read_json(entry.path()).and_then(|manifest| string_field(&manifest, "name")) {
                packages.entry(name).or_insert(dir);
            }
        }
        Self { packages }
    }

    /// The package `specifier` imports, as its directory and the subpath after the package name
    /// (`""` for the package itself): `@acme/ui/button` is `("packages/ui", "button")`.
    pub(super) fn locate<'a>(&'a self, specifier: &'a str) -> Option<(&'a str, &'a str)> {
        let name_len = match specifier.strip_prefix('@') {
            Some(scoped) => 1 + scoped.find('/')? + 1 + scoped.split('/').nth(1)?.len(),
            None => specifier.split('/').next()?.len(),
        };
        let (name, rest) = specifier.split_at(name_len);
        let dir = self.packages.get(name)?;
        Some((dir.as_str(), rest.trim_start_matches('/')))
    }
}

/// The directories a workspace declaration claims.
struct DirMatcher {
    include: GlobSet,
    exclude: GlobSet,
}

impl DirMatcher {
    /// The matcher for the declaration at the root of `root`, or `None` when it declares nothing.
    fn declared(root: &Path) -> Option<Self> {
        let mut patterns = std::fs::read_to_string(root.join(PNPM_WORKSPACE))
            .map(|yaml| pnpm_packages(&yaml))
            .unwrap_or_default();
        if let Some(manifest) = read_json(&root.join(ROOT_MANIFEST)) {
            patterns.extend(manifest_workspaces(&manifest));
        }
        if patterns.is_empty() {
            return None;
        }
        let (excluded, included): (Vec<_>, Vec<_>) = patterns.iter().partition(|pattern| pattern.starts_with('!'));
        let excluded: Vec<&str> = excluded.iter().map(|pattern| &pattern[1..]).collect();
        Some(Self {
            include: glob_set(included.iter().map(|pattern| pattern.as_str()))?,
            exclude: glob_set(excluded.into_iter())?,
        })
    }

    fn matches(&self, dir: &str) -> bool {
        !dir.is_empty() && self.include.is_match(dir) && !self.exclude.is_match(dir)
    }
}

fn glob_set<'a>(patterns: impl Iterator<Item = &'a str>) -> Option<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let pattern = pattern.trim_start_matches("./").trim_end_matches('/');
        let glob: Glob = GlobBuilder::new(pattern).literal_separator(true).build().ok()?;
        builder.add(glob);
    }
    builder.build().ok()
}

/// The entries of the `packages:` list of a `pnpm-workspace.yaml`. The file is a flat mapping of
/// lists, so the entries are the `- item` lines indented under the `packages:` key.
fn pnpm_packages(yaml: &str) -> Vec<String> {
    let mut in_packages = false;
    let mut patterns = Vec::new();
    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !line.starts_with([' ', '\t', '-']) {
            in_packages = trimmed.trim_end() == "packages:";
            continue;
        }
        if let Some(item) = in_packages.then(|| trimmed.strip_prefix('-')).flatten() {
            let item = item.split(" #").next().unwrap_or(item).trim();
            patterns.push(item.trim_matches(['\'', '"']).to_string());
        }
    }
    patterns
}

/// The globs of a manifest's `workspaces` field, in either of its two shapes.
fn manifest_workspaces(manifest: &Value) -> Vec<String> {
    let field = manifest.get("workspaces");
    let list = field.and_then(|workspaces| workspaces.get("packages").or(Some(workspaces)));
    list.and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// The files a package may be entered through, as `./`-relative specifiers of its directory, in the
/// order they are tried: for the package itself the manifest's entry fields and `exports["."]`, for
/// `subpath` its `exports` entry; then the conventional source locations.
pub(super) fn entry_candidates(manifest: &Value, subpath: &str) -> Vec<String> {
    let exports = manifest.get("exports");
    let mut candidates: Vec<String> = Vec::new();
    if subpath.is_empty() {
        candidates.extend(exports.and_then(|exports| export_target(exports, ".")));
        candidates.extend(ENTRY_FIELDS.iter().filter_map(|field| string_field(manifest, field)));
        candidates.extend(["src/index", "index"].map(str::to_string));
    } else {
        candidates.extend(exports.and_then(|exports| export_target(exports, &format!("./{subpath}"))));
        candidates.extend([subpath.to_string(), format!("src/{subpath}")]);
    }
    candidates
        .into_iter()
        .map(|candidate| format!("./{}", candidate.trim_start_matches("./")))
        .collect()
}

/// The file `exports` maps `key` (`.` or `./sub`) to: a string, or the first of
/// [`EXPORT_CONDITIONS`] that holds one, at any depth.
fn export_target(exports: &Value, key: &str) -> Option<String> {
    let entry = match exports {
        Value::Object(map) if map.keys().any(|name| name.starts_with('.')) => map.get(key)?,
        other if key == "." => other,
        _ => return None,
    };
    condition_target(entry)
}

fn condition_target(entry: &Value) -> Option<String> {
    match entry {
        Value::String(path) => Some(path.clone()),
        Value::Object(map) => EXPORT_CONDITIONS
            .iter()
            .filter_map(|condition| map.get(*condition))
            .find_map(condition_target),
        _ => None,
    }
}

/// The parsed JSON file at `path`, or `None` when it is missing or not JSON.
pub(super) fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn string_field(manifest: &Value, field: &str) -> Option<String> {
    manifest.get(field)?.as_str().map(str::to_string)
}
