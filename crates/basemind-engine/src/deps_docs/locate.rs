//! Where a dependency is installed, and which of its files are documentation (ADR-0049).
//!
//! Every package root is one of a fixed list per ecosystem. A root inside the repository is read
//! as is; one outside it only under the operator grant, and without it no file under it is even
//! looked at. Paths are canonicalized and a package that resolves out of its root (a symlinked
//! `node_modules/<pkg>`) is refused whatever the grant. Walks below a package never follow a
//! symlink.

use std::path::{Path, PathBuf};

use super::lockfiles::{Wanted, normalize_python, read_text};
use super::{
    DependencyRoots, Ecosystem, REASON_ESCAPES_ROOT, REASON_NO_VERSION, REASON_NOT_GRANTED, REASON_NOT_INSTALLED,
};

/// Extensions of documentation files; an extension-less `README` / `CHANGELOG` is read as text.
const DOC_EXTENSIONS: &[&str] = &["md", "markdown", "mdx", "rst", "txt", "adoc", "asciidoc"];
/// Directory entries one package walk visits at most, so a huge `docs/` cannot stall a scan.
const MAX_WALK_ENTRIES: usize = 10_000;
/// Documentation files taken from one package at most.
const MAX_FILES_PER_PACKAGE: usize = 500;
/// Directory depth under `docs/` a walk descends to.
const MAX_DOCS_DEPTH: usize = 8;

/// An installed package: its canonical directory and the version installed there.
#[derive(Debug)]
pub(super) struct Located {
    pub dir: PathBuf,
    pub version: String,
    /// A Python distribution's `.dist-info` directory, whose `METADATA` carries the README.
    pub dist_info: bool,
}

/// Why a package was not found, as the `reason` of its unavailable entry.
pub(super) type Missing = &'static str;

/// A package root and whether it may be read: inside the repository, or granted.
struct Root {
    path: PathBuf,
    canonical: PathBuf,
}

/// Tracks the strongest reason a package was not found across the roots tried.
struct Search<'a> {
    roots: &'a DependencyRoots,
    missing: Missing,
}

impl<'a> Search<'a> {
    fn new(roots: &'a DependencyRoots) -> Self {
        Self {
            roots,
            missing: REASON_NOT_INSTALLED,
        }
    }

    fn note(&mut self, reason: Missing) {
        // An escape outranks a missing grant, which outranks a package that is simply absent. ~keep
        let rank = |reason: Missing| match reason {
            REASON_ESCAPES_ROOT => 2,
            REASON_NOT_GRANTED => 1,
            _ => 0,
        };
        if rank(reason) > rank(self.missing) {
            self.missing = reason;
        }
    }

    /// `path` as a root, when it exists and may be read. An outside root is not touched at all
    /// without the grant: not even its existence is checked.
    fn root(&mut self, path: &Path, inside_tree: bool) -> Option<Root> {
        if !inside_tree && !self.roots.granted {
            self.note(REASON_NOT_GRANTED);
            return None;
        }
        let canonical = path.canonicalize().ok()?;
        if !canonical.starts_with(&self.roots.repo) && !self.roots.granted {
            // An in-tree root that resolves outside the repository (a symlinked `node_modules`). ~keep
            self.note(REASON_NOT_GRANTED);
            return None;
        }
        Some(Root {
            path: path.to_path_buf(),
            canonical,
        })
    }

    /// `rel` under `root`, canonical, when it exists and stays inside the root.
    fn package(&mut self, root: &Root, rel: &str) -> Option<PathBuf> {
        let candidate = root.path.join(rel);
        std::fs::symlink_metadata(&candidate).ok()?;
        let canonical = candidate.canonicalize().ok()?;
        if !canonical.starts_with(&root.canonical) {
            self.note(REASON_ESCAPES_ROOT);
            return None;
        }
        canonical.is_dir().then_some(canonical)
    }
}

/// Find the installed copy of `wanted`.
pub(super) fn locate(wanted: &Wanted, roots: &DependencyRoots) -> Result<Located, Missing> {
    let mut search = Search::new(roots);
    let found = match wanted.ecosystem {
        Ecosystem::Cargo => cargo(wanted, &mut search),
        Ecosystem::Npm => npm(wanted, &mut search),
        Ecosystem::Pypi => python(wanted, &mut search),
        Ecosystem::Go => go(wanted, &mut search),
    };
    match found {
        Some(result) => result,
        None => Err(search.missing),
    }
}

fn plain(dir: PathBuf, version: String) -> Option<Result<Located, Missing>> {
    Some(Ok(Located {
        dir,
        version,
        dist_info: false,
    }))
}

/// `vendor/<name>-<version>` or `vendor/<name>` (when its manifest has the version), then
/// `<cargo home>/registry/src/<index>/<name>-<version>`.
fn cargo(wanted: &Wanted, search: &mut Search<'_>) -> Option<Result<Located, Missing>> {
    let Some(version) = wanted.version.clone() else {
        return Some(Err(REASON_NO_VERSION));
    };
    let versioned = format!("{}-{version}", wanted.name);
    if let Some(vendor) = search.root(&search.roots.repo.join("vendor"), true) {
        if let Some(dir) = search.package(&vendor, &versioned) {
            return plain(dir, version);
        }
        if let Some(dir) = search.package(&vendor, &wanted.name)
            && cargo_manifest_version(&dir).as_deref() == Some(version.as_str())
        {
            return plain(dir, version);
        }
    }
    let cargo_home = search.roots.homes.cargo_home.clone()?;
    let registry = search.root(&cargo_home.join("registry").join("src"), false)?;
    let mut indexes: Vec<PathBuf> = std::fs::read_dir(&registry.path)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
        .map(PathBuf::from)
        .collect();
    indexes.sort();
    for index in indexes {
        let rel = index.join(&versioned);
        if let Some(dir) = rel.to_str().and_then(|rel| search.package(&registry, rel)) {
            return plain(dir, version);
        }
    }
    None
}

fn cargo_manifest_version(dir: &Path) -> Option<String> {
    let manifest = toml::from_str::<toml::Table>(&read_text(&dir.join("Cargo.toml"))?).ok()?;
    Some(manifest.get("package")?.get("version")?.as_str()?.to_string())
}

/// `node_modules/<name>`, versioned by its own `package.json`.
fn npm(wanted: &Wanted, search: &mut Search<'_>) -> Option<Result<Located, Missing>> {
    let modules = search.root(&search.roots.repo.join("node_modules"), true)?;
    let dir = search.package(&modules, &wanted.name)?;
    let manifest =
        read_text(&dir.join("package.json")).and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let version = manifest
        .as_ref()
        .and_then(|manifest| manifest.get("version"))
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .or_else(|| wanted.version.clone());
    match version {
        Some(version) => plain(dir, version),
        None => Some(Err(REASON_NO_VERSION)),
    }
}

/// The `<name>-<version>.dist-info` directory in a `site-packages` of `.venv` or `$VIRTUAL_ENV`.
fn python(wanted: &Wanted, search: &mut Search<'_>) -> Option<Result<Located, Missing>> {
    let repo_venv = search.roots.repo.join(".venv");
    let mut venvs = vec![(repo_venv.clone(), true)];
    if let Some(active) = search.roots.homes.virtual_env.clone()
        && active != repo_venv
    {
        let inside = active
            .canonicalize()
            .is_ok_and(|canonical| canonical.starts_with(&search.roots.repo));
        venvs.push((active, inside));
    }
    for (venv, inside) in venvs {
        let Some(root) = search.root(&venv, inside) else {
            continue;
        };
        for site in site_packages(&root.path) {
            let Some((dist_info, version)) = dist_info_of(&site, &wanted.name) else {
                continue;
            };
            let Ok(rel) = site.join(&dist_info).strip_prefix(&root.path).map(Path::to_path_buf) else {
                continue;
            };
            if let Some(dir) = rel.to_str().and_then(|rel| search.package(&root, rel)) {
                return Some(Ok(Located {
                    dir,
                    version,
                    dist_info: true,
                }));
            }
        }
    }
    None
}

/// `lib/python*/site-packages`, `lib64/python*/site-packages` and `Lib/site-packages` of `venv`.
fn site_packages(venv: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for lib in ["lib", "lib64"] {
        let Ok(entries) = std::fs::read_dir(venv.join(lib)) else {
            continue;
        };
        let mut pythons: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("python"))
            })
            .map(|entry| entry.path().join("site-packages"))
            .collect();
        pythons.sort();
        out.extend(pythons);
    }
    out.push(venv.join("Lib").join("site-packages"));
    out.retain(|site| site.is_dir());
    out
}

/// The dist-info directory name in `site` of the distribution `name` (normalised), and its version.
fn dist_info_of(site: &Path, name: &str) -> Option<(String, String)> {
    let mut found: Vec<(String, String)> = std::fs::read_dir(site)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|dir| {
            let stem = dir.strip_suffix(".dist-info")?;
            let (dist, version) = stem.split_once('-')?;
            (normalize_python(dist) == name && !version.is_empty()).then(|| (dir.clone(), version.to_string()))
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

/// The module cache's escaping of a path or version: an upper-case letter is `!` and its lower case.
fn escape_module(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `vendor/<module>` when `vendor/modules.txt` records the version, then
/// `<module cache>/<escaped module>@<escaped version>`.
fn go(wanted: &Wanted, search: &mut Search<'_>) -> Option<Result<Located, Missing>> {
    let Some(version) = wanted.version.clone() else {
        return Some(Err(REASON_NO_VERSION));
    };
    let record = format!("# {} {version}", wanted.name);
    if let Some(vendor) = search.root(&search.roots.repo.join("vendor"), true)
        && read_text(&vendor.path.join("modules.txt")).is_some_and(|text| text.lines().any(|line| line == record))
        && let Some(dir) = search.package(&vendor, &wanted.name)
    {
        return plain(dir, version);
    }
    let cache = search.roots.homes.go_mod_cache.clone()?;
    let cache = search.root(&cache, false)?;
    let rel = format!("{}@{}", escape_module(&wanted.name), escape_module(&version));
    let dir = search.package(&cache, &rel)?;
    plain(dir, version)
}

/// A documentation file of a package: its path relative to the package directory and size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DocFile {
    pub rel: String,
    pub size: u64,
}

fn has_doc_extension(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| DOC_EXTENSIONS.iter().any(|doc| ext.eq_ignore_ascii_case(doc)))
}

fn is_top_level_doc(name: &str, dist_info: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.split('.').next().unwrap_or_default();
    let named = matches!(stem, "readme" | "changelog") || (dist_info && name == "METADATA");
    named && (!name.contains('.') || has_doc_extension(name))
}

/// The documentation of the package at `dir`: its README and CHANGELOG (a Python distribution's
/// `METADATA`), then `docs/**` in path order, each file kept while the package stays within
/// `max_bytes`. Symlinks are skipped, never followed.
pub(super) fn doc_files(located: &Located, max_bytes: u64) -> Vec<DocFile> {
    let mut candidates: Vec<DocFile> = Vec::new();
    let mut visited = 0usize;
    let Ok(entries) = std::fs::read_dir(&located.dir) else {
        return Vec::new();
    };
    let mut top: Vec<(String, std::fs::Metadata)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.metadata().ok()?)))
        .collect();
    // The README is read before the CHANGELOG: when the budget cannot hold both, it is the one kept.
    top.sort_by_key(|(name, _)| (!name.to_ascii_lowercase().starts_with("readme"), name.clone()));
    for (name, meta) in &top {
        visited += 1;
        if meta.is_file() && is_top_level_doc(name, located.dist_info) {
            candidates.push(DocFile {
                rel: name.clone(),
                size: meta.len(),
            });
        }
    }
    if top.iter().any(|(name, meta)| name == "docs" && meta.is_dir()) {
        walk_docs(&located.dir, "docs", 1, &mut visited, &mut candidates);
    }
    let mut spent = 0u64;
    candidates
        .into_iter()
        .filter(|file| {
            let fits = spent.saturating_add(file.size) <= max_bytes;
            if fits {
                spent += file.size;
            }
            fits
        })
        .take(MAX_FILES_PER_PACKAGE)
        .collect()
}

/// Append the documentation files under `dir/rel` to `out`, depth first in name order.
/// `DirEntry::metadata` does not traverse a symlink, so a link is neither a file nor a directory
/// here and is skipped.
fn walk_docs(dir: &Path, rel: &str, depth: usize, visited: &mut usize, out: &mut Vec<DocFile>) {
    let Ok(entries) = std::fs::read_dir(dir.join(rel)) else {
        return;
    };
    let mut children: Vec<(String, std::fs::Metadata)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.metadata().ok()?)))
        .collect();
    children.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, meta) in children {
        *visited += 1;
        if *visited > MAX_WALK_ENTRIES {
            return;
        }
        let child = format!("{rel}/{name}");
        if meta.is_file() && has_doc_extension(&name) {
            out.push(DocFile {
                rel: child,
                size: meta.len(),
            });
        } else if meta.is_dir() && depth < MAX_DOCS_DEPTH {
            walk_docs(dir, &child, depth + 1, visited, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_paths_escape_upper_case() {
        assert_eq!(
            escape_module("github.com/BurntSushi/toml"),
            "github.com/!burnt!sushi/toml"
        );
        assert_eq!(escape_module("v1.0.0"), "v1.0.0");
    }

    #[test]
    fn readme_and_changelog_are_top_level_docs_in_any_case_and_doc_extension() {
        for name in ["README", "README.md", "readme.rst", "CHANGELOG.md", "Changelog.txt"] {
            assert!(is_top_level_doc(name, false), "{name}");
        }
        for name in ["README.html", "LICENSE", "package.json", "METADATA", "readme.js"] {
            assert!(!is_top_level_doc(name, false), "{name}");
        }
        assert!(is_top_level_doc("METADATA", true));
    }

    #[test]
    fn the_byte_budget_keeps_files_in_order_and_skips_one_that_overruns_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pkg = dir.path();
        std::fs::write(pkg.join("README.md"), vec![b'r'; 600]).unwrap();
        std::fs::write(pkg.join("CHANGELOG.md"), vec![b'c'; 900]).unwrap();
        std::fs::create_dir_all(pkg.join("docs/guide")).unwrap();
        std::fs::write(pkg.join("docs/a.md"), vec![b'a'; 300]).unwrap();
        std::fs::write(pkg.join("docs/guide/b.rst"), vec![b'b'; 100]).unwrap();
        std::fs::write(pkg.join("docs/logo.png"), vec![0; 10]).unwrap();
        let located = Located {
            dir: pkg.to_path_buf(),
            version: "1".into(),
            dist_info: false,
        };
        let rels: Vec<String> = doc_files(&located, 1000).into_iter().map(|f| f.rel).collect();
        assert_eq!(rels, ["README.md", "docs/a.md", "docs/guide/b.rst"]);
        let all: Vec<String> = doc_files(&located, 1 << 20).into_iter().map(|f| f.rel).collect();
        assert_eq!(all, ["README.md", "CHANGELOG.md", "docs/a.md", "docs/guide/b.rst"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_walk_skips_symlinks_out_of_the_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.md"), "secret").unwrap();
        let pkg = dir.path();
        std::fs::create_dir_all(pkg.join("docs")).unwrap();
        std::fs::write(pkg.join("docs/real.md"), "real").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.md"), pkg.join("README.md")).unwrap();
        std::os::unix::fs::symlink(outside.path(), pkg.join("docs/linked")).unwrap();
        let located = Located {
            dir: pkg.to_path_buf(),
            version: "1".into(),
            dist_info: false,
        };
        let rels: Vec<String> = doc_files(&located, 1 << 20).into_iter().map(|f| f.rel).collect();
        assert_eq!(rels, ["docs/real.md"]);
    }
}
