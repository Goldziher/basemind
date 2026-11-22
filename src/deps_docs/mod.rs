//! Documentation of the repository's dependencies (#132, ADR-0049).
//!
//! With `[documents.dependencies]` on, a full working-tree scan resolves the dependencies the
//! manifests and lockfiles name (`lockfiles`), finds each one's installed copy under a fixed list
//! of package roots (`locate`) and hands the scan its README, CHANGELOG and `docs/**` as
//! documents under the virtual path `deps/<ecosystem>/<name>@<version>/<file>`. The document tier
//! then extracts, stores and searches them like any other document; `docs_search`'s `path` glob
//! (`deps/npm/react@*/**`) filters by package.
//!
//! What is not indexed is listed: a package not installed, behind a root the operator did not
//! grant, resolving out of its root, from a source basemind does not read, or past
//! `max_packages`. The full scan returns the list and records it beside the status sidecar, where
//! `index_status` reads it as `deps_unavailable`.

mod locate;
mod lockfiles;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ahash::AHashMap;
use serde::{Deserialize, Serialize};

use crate::config::engine::DependencyHomes;
use crate::config::{Config, DependenciesConfig};

/// First segment of every virtual dependency path.
pub const VIRTUAL_ROOT: &str = "deps";

/// The package is named by a manifest but no installed copy was found.
pub const REASON_NOT_INSTALLED: &str = "not_installed";
/// The installed copy is under a root outside the repository, and the operator did not grant it.
pub const REASON_NOT_GRANTED: &str = "not_granted";
/// The installed copy resolves out of its package root (a symlink); never read.
pub const REASON_ESCAPES_ROOT: &str = "escapes_root";
/// A git or path source, which has no package root basemind reads.
pub const REASON_UNSUPPORTED_SOURCE: &str = "unsupported_source";
/// No version is known for the package, so it has no virtual path.
pub const REASON_NO_VERSION: &str = "no_version";
/// Past `[documents.dependencies] max_packages`.
pub const REASON_OVER_MAX_PACKAGES: &str = "over_max_packages";

/// Name of the record of the last full scan's unavailable dependencies, beside `status.json`.
const UNAVAILABLE_FILE: &str = "deps-unavailable.json";

/// Manifests and lockfiles whose change re-resolves the dependencies, at the repository root.
const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "pyproject.toml",
    "uv.lock",
    "poetry.lock",
    "go.mod",
    "go.sum",
];

/// A package ecosystem, as the second segment of a virtual path names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ecosystem {
    Cargo,
    Npm,
    Pypi,
    Go,
}

impl Ecosystem {
    pub const ALL: [Self; 4] = [Self::Cargo, Self::Npm, Self::Pypi, Self::Go];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Npm => "npm",
            Self::Pypi => "pypi",
            Self::Go => "go",
        }
    }

    fn parse(segment: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|eco| eco.as_str() == segment)
    }
}

/// A dependency whose documentation is not indexed, and why (one of the `REASON_*` values).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DepUnavailable {
    pub ecosystem: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub reason: String,
}

impl DepUnavailable {
    fn new(ecosystem: Ecosystem, name: &str, version: Option<&str>, reason: &str) -> Self {
        Self {
            ecosystem: ecosystem.as_str().to_string(),
            name: name.to_string(),
            version: version.map(String::from),
            reason: reason.to_string(),
        }
    }
}

/// Where the package roots are and whether the ones outside the repository may be read.
#[derive(Debug, Clone)]
pub struct DependencyRoots {
    /// The repository root, canonical.
    pub repo: PathBuf,
    pub homes: DependencyHomes,
    /// The operator granted roots outside the repository (`BASEMIND_ALLOW_DEPENDENCY_ROOTS`).
    pub granted: bool,
}

/// One documentation file of an installed package.
#[derive(Debug, Clone)]
pub struct DepDocFile {
    /// The package directory, canonical: the root the file is read under.
    pub package_dir: Arc<PathBuf>,
    /// The file's path inside the package directory, forward-slash separated.
    pub rel: String,
    /// The MIME type to extract it as, where its name does not say (an extension-less README).
    pub mime: Option<&'static str>,
}

/// The dependency documentation of one scan.
#[derive(Debug, Default)]
pub struct DepDocs {
    /// Virtual path to file.
    pub files: AHashMap<String, DepDocFile>,
    pub unavailable: Vec<DepUnavailable>,
    /// Packages whose documentation was collected.
    pub packages: usize,
}

impl DepDocs {
    /// The virtual paths, sorted.
    pub fn virtual_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self.files.keys().cloned().collect();
        paths.sort_unstable();
        paths
    }
}

/// A virtual dependency path, split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualPath<'a> {
    pub ecosystem: Ecosystem,
    pub name: &'a str,
    pub version: &'a str,
    pub file: &'a str,
}

/// Split `deps/<ecosystem>/<name>@<version>/<file>`. The name may span segments (a scoped npm
/// package `@scope/name`, a Go module path): it ends at the first segment holding an `@` past its
/// first byte.
pub fn parse_virtual(rel: &str) -> Option<VirtualPath<'_>> {
    let rest = rel.strip_prefix(VIRTUAL_ROOT)?.strip_prefix('/')?;
    let (eco, rest) = rest.split_once('/')?;
    let ecosystem = Ecosystem::parse(eco)?;
    let mut offset = 0;
    for segment in rest.split('/') {
        let end = offset + segment.len();
        if let Some(at) = segment.get(1..).and_then(|tail| tail.find('@')).map(|at| at + 1) {
            let name = &rest[..offset + at];
            let version = &segment[at + 1..];
            let file = rest.get(end + 1..)?;
            return (!version.is_empty() && !file.is_empty()).then_some(VirtualPath {
                ecosystem,
                name,
                version,
                file,
            });
        }
        offset = end + 1;
    }
    None
}

/// Whether `rel` is a virtual dependency path.
pub fn is_virtual(rel: &str) -> bool {
    parse_virtual(rel).is_some()
}

/// `deps/<ecosystem>/<name>@<version>/<file>`.
pub fn virtual_path(ecosystem: Ecosystem, name: &str, version: &str, file: &str) -> String {
    format!("{VIRTUAL_ROOT}/{}/{name}@{version}/{file}", ecosystem.as_str())
}

/// The documentation of the dependencies of the repository under `roots`.
pub fn resolve(cfg: &DependenciesConfig, roots: &DependencyRoots) -> DepDocs {
    let (wanted, unsupported) = lockfiles::wanted(&roots.repo, cfg.direct_only);
    let mut out = DepDocs {
        unavailable: unsupported,
        ..DepDocs::default()
    };
    for dep in &wanted {
        let located = match locate::locate(dep, roots) {
            Ok(located) => located,
            Err(reason) => {
                out.unavailable.push(DepUnavailable::new(
                    dep.ecosystem,
                    &dep.name,
                    dep.version.as_deref(),
                    reason,
                ));
                continue;
            }
        };
        if out.packages >= cfg.max_packages {
            out.unavailable.push(DepUnavailable::new(
                dep.ecosystem,
                &dep.name,
                Some(&located.version),
                REASON_OVER_MAX_PACKAGES,
            ));
            continue;
        }
        out.packages += 1;
        let package_dir = Arc::new(located.dir.clone());
        for file in locate::doc_files(&located, cfg.max_bytes_per_package) {
            let mime = (!file.rel.contains('.')).then_some("text/plain");
            out.files.insert(
                virtual_path(dep.ecosystem, &dep.name, &located.version, &file.rel),
                DepDocFile {
                    package_dir: Arc::clone(&package_dir),
                    rel: file.rel,
                    mime,
                },
            );
        }
    }
    out
}

/// Whether `config` asks for dependency documentation.
pub fn enabled(config: &Config) -> bool {
    config.documents.enabled && config.documents.dependencies.enabled && cfg!(feature = "documents")
}

/// The dependency documentation a full scan of `root` indexes, from the installed engine
/// configuration's package caches and grant; `None` when `config` does not ask for it.
pub fn for_scan(root: &Path, config: &Config) -> Option<DepDocs> {
    if !enabled(config) {
        return None;
    }
    let repo = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let roots = DependencyRoots {
        homes: crate::config::engine::with(|engine| engine.dependency_homes.clone()),
        granted: crate::config::trust::dependency_roots_granted(&repo),
        repo,
    };
    let docs = resolve(&config.documents.dependencies, &roots);
    if docs.unavailable.iter().any(|dep| dep.reason == REASON_NOT_GRANTED) {
        crate::config::trust::warn_once(format!(
            "[documents.dependencies]: package roots outside the repository are not read; set {}=1 \
             in the environment to index their documentation",
            crate::config::trust::ALLOW_DEPENDENCY_ROOTS_ENV
        ));
    }
    Some(docs)
}

/// Whether an incremental pass over `paths` changed a manifest or lockfile of the repository at
/// `root` while dependency documentation is on: the dependencies are then re-resolved by a full
/// pass.
pub fn manifest_touched(root: &Path, config: &Config, paths: &[PathBuf]) -> bool {
    enabled(config)
        && paths.iter().any(|path| {
            path.parent() == Some(root)
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| MANIFESTS.contains(&name))
        })
}

/// Record the unavailable dependencies of the last full working-tree scan of the working view
/// (none when `docs` is `None`), and return them for the scan report. Best effort, like the status
/// sidecar beside it.
pub fn record(basemind_dir: &Path, working_view: bool, docs: Option<&DepDocs>) -> Vec<DepUnavailable> {
    let unavailable = docs.map(|docs| docs.unavailable.clone()).unwrap_or_default();
    if working_view {
        let path = basemind_dir.join(UNAVAILABLE_FILE);
        if unavailable.is_empty() {
            let _ = std::fs::remove_file(path);
        } else if let Ok(bytes) = serde_json::to_vec(&unavailable) {
            let _ = crate::store_blob::write_bytes_atomic(path, &bytes);
        }
    }
    unavailable
}

/// What the last full scan recorded with [`record`].
pub fn read_unavailable(basemind_dir: &Path) -> Vec<DepUnavailable> {
    std::fs::read(basemind_dir.join(UNAVAILABLE_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
