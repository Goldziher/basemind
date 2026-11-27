//! Rust file-level module resolution (ADR-0047 decision 4).
//!
//! - `mod x;` maps to `x.rs` or `x/mod.rs` in the declaring file's module directory (the file's own
//!   directory for `mod.rs` and crate roots, else `<dir>/<stem>/`), or to its `#[path]` file.
//! - A `use` path starting with `crate::`, `self::`, `super::` or a module the file declares maps to
//!   the longest prefix of it that is a module file under the crate root, the crate root being
//!   found from the nearest `Cargo.toml`: a target `path` it declares (`[lib]`, `[[bin]]`,
//!   `[[test]]`, `[[example]]`, `[[bench]]`, `package.build`), else Cargo's standard layout
//!   (`src/lib.rs`, `src/main.rs`, `src/bin/*`, `tests/*`, `examples/*`, `benches/*`,
//!   `build.rs`; a file in `tests/x/` belongs to `tests/x/main.rs`, else `tests/x/` is a module
//!   shared by every crate in `tests/` that declares `mod x;`, and by `tests/x.rs`, which loads it
//!   through `#[path]`). A file is a crate root only by that layout, never by its name: a
//!   `src/foo/main.rs` below `src/lib.rs` is the module `foo::main`. With no module file at all,
//!   the path names an item of the crate root file.
//! - A path starting at a crate name is classified by the package's `Cargo.toml`
//!   ([`crate::intel::module_rust_crates`]): the standard library and registry dependencies are
//!   external; the package's own library (from its tests, binaries, examples) and path dependencies
//!   inside the repository (workspace members) resolve like a `crate::` path of that crate, to its
//!   module files; a name that is no crate the manifest declares is unresolved. A path through a
//!   name the file binds itself (`use Kind::*` for its own enum, `use io::Write` after
//!   `use std::io`), or any unknown name in a file with a glob `use` that may have brought it in,
//!   is no edge and is not counted. Without a `Cargo.toml` such a path is external.
//!
//! Inline modules (`mod tests { … }`) are scopes inside one file: `use super::*` in one resolves to
//! the file itself, which is not an edge. `pub use` re-exports are not followed: an importer gets an
//! edge to the module it names, the first hop.

use std::rc::Rc;

use ahash::{AHashMap, AHashSet};

use crate::extract::{Import, Symbol, SymbolKind};
use crate::index::keys::EdgeKinds;
use crate::intel::module_resolver::{ModuleImport, ModuleTarget, Probe, join, parent_dir, strip_dir};
use crate::intel::module_rust_crates::{CrateRef, Manifest, classify_crate, lib_root, normalize, parse_manifest};

/// Where a crate's module tree is rooted.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CrateLayout {
    /// The directory module paths start from.
    dir: String,
    /// The crate root file's candidates in order; the first indexed one is the root. Probed only
    /// when a path falls back to the root, so editing `lib.rs` does not re-stitch the whole crate.
    roots: Vec<String>,
    /// For a module directory `<top>/x/` that is no crate of its own (`tests/common/` without a
    /// `main.rs`): `x`, the module the crates directly in `<top>/` load with `mod x;`.
    shared: Option<String>,
}

impl CrateLayout {
    /// A crate whose module paths start from `dir` and whose root is one of `roots`.
    fn new(dir: String, roots: Vec<String>) -> Self {
        Self {
            dir,
            roots,
            shared: None,
        }
    }

    /// Whether `importer` is (a candidate for) the crate root file.
    fn is_root(&self, importer: &str) -> bool {
        self.roots.iter().any(|root| root == importer)
    }

    /// The crate root files a path falling back to the root names: the first indexed root, or for a
    /// shared module every crate including it.
    ///
    /// A shared module such as `tests/common/mod.rs` belongs to each test crate that declares
    /// `mod common;`, so its `crate::` and `super::` paths name a module of each of them. A module
    /// file below it resolves the same in all of them, since they share the directory; an item of
    /// the crate root resolves to every including root, plus `tests/common.rs`, which conventionally
    /// loads `tests/common/` through `#[path]`. The listing of `<top>/` is a probe, so a crate
    /// added, removed or edited there re-stitches the module.
    fn root_files(&self, probe: &mut Probe<'_>, includers: &mut AHashMap<String, Vec<String>>) -> Vec<String> {
        let Some(name) = &self.shared else {
            return self
                .roots
                .iter()
                .find(|root| probe.indexed(root))
                .cloned()
                .into_iter()
                .collect();
        };
        probe.probes.insert_dir(&self.dir);
        let (root, indexed) = (probe.root, probe.indexed);
        includers
            .entry(join(&self.dir, name))
            .or_insert_with(|| {
                let mut files = including_crates(root, &self.dir, name, indexed);
                files.extend(self.roots.iter().filter(|root| indexed(root)).cloned());
                files.sort_unstable();
                files.dedup();
                files
            })
            .clone()
    }
}

/// The indexed `.rs` files directly in `dir` that declare `mod name;`.
fn including_crates(root: &std::path::Path, dir: &str, name: &str, indexed: &dyn Fn(&str) -> bool) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|file| file.ends_with(".rs"))
        .map(|file| join(dir, &file))
        .filter(|rel| indexed(rel))
        .filter(|rel| {
            std::fs::read_to_string(root.join(rel)).is_ok_and(|source| {
                tokens(&source)
                    .windows(3)
                    .any(|window| window == [Token::Ident("mod"), Token::Ident(name), Token::Semi])
            })
        })
        .collect()
}

/// A module declared in the importer: its name and, for an inline one, its body's byte span.
struct DeclaredModule {
    name: String,
    start: u32,
    end: u32,
    inline: bool,
}

/// Rust module resolver. Caches each `Cargo.toml` path's parse (`None` when absent) and each shared
/// module directory's including crates.
#[derive(Default)]
pub(crate) struct RustModules {
    manifests: AHashMap<String, Option<Rc<Manifest>>>,
    includers: AHashMap<String, Vec<String>>,
}

/// The package an importer belongs to: its manifest's directory and parse.
type Package = (String, Rc<Manifest>);

/// The names a file binds itself: its items and the names its `use`s bring in. A glob `use` can
/// bring in any name, so with one no unknown name is evidence of a failed resolution.
struct Bound<'a> {
    names: AHashSet<&'a str>,
    glob: bool,
}

impl RustModules {
    /// Resolve one importer whose outline (`symbols`, `imports`) was extracted from the content
    /// `content_hash`. The source is re-read only to tell `mod x;` from `mod x { … }` and to find
    /// `#[path]`; when it no longer hashes to `content_hash` the outline's offsets do not fit it,
    /// so no module declaration is read (the next scan of the edited file re-stitches it).
    pub(crate) fn resolve(
        &mut self,
        probe: &mut Probe<'_>,
        importer: &str,
        content_hash: &str,
        symbols: &[Symbol],
        imports: &[Import],
    ) -> Vec<ModuleImport> {
        let source = symbols
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Module)
            .then(|| std::fs::read(probe.root.join(importer)).ok())
            .flatten()
            .filter(|bytes| crate::hashing::hex(&crate::hashing::hash_bytes(bytes)) == content_hash);
        let modules = declared_modules(symbols, source.as_deref());
        let package = self.manifest(probe, importer);
        let layout = package
            .as_ref()
            .and_then(|(dir, manifest)| crate_layout(probe, importer, dir, &manifest.targets));
        let file_scope = layout.as_ref().and_then(|layout| module_path(layout, importer));
        // With no `Cargo.toml` there is no layout to read roots from (a `rustc` or Bazel crate), so
        // a `lib.rs` or `main.rs` is then taken for the root its name suggests.
        let is_root = match &layout {
            Some(layout) => layout.is_root(importer),
            None => matches!(importer.rsplit('/').next(), Some("lib.rs" | "main.rs")),
        };
        let names: AHashSet<&str> = modules.iter().map(|module| module.name.as_str()).collect();
        let leaves: Vec<(u32, UseLeaf)> = imports
            .iter()
            .filter(|import| import.module.is_none())
            .flat_map(|import| {
                use_leaves(&import.raw)
                    .into_iter()
                    .map(|leaf| (import.start_byte, leaf))
            })
            .collect();
        let mut bound: AHashSet<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
        bound.extend(leaves.iter().filter_map(|(_, leaf)| leaf.binds.as_deref()));
        let bound = Bound {
            names: bound,
            glob: leaves.iter().any(|(_, leaf)| leaf.glob),
        };

        let mut out = Vec::new();
        for module in modules.iter().filter(|module| !module.inline) {
            let target = declared_module_file(probe, importer, is_root, &modules, module, source.as_deref());
            out.push(ModuleImport {
                anchor: module.start,
                specifier: format!("mod {}", module.name),
                target,
                kind: EdgeKinds::MODULE_DECL,
            });
        }
        for import in imports {
            let Some(name) = &import.module else {
                continue;
            };
            let target = if name == "self" {
                ModuleTarget::SelfFile
            } else {
                self.crate_path(probe, package.as_ref(), importer, std::slice::from_ref(name))
            };
            out.push(ModuleImport {
                anchor: import.start_byte,
                specifier: format!("extern crate {name}"),
                target,
                kind: EdgeKinds::IMPORT,
            });
        }
        for &(anchor, ref leaf) in &leaves {
            let path = &leaf.path;
            let scope = file_scope.as_ref().map(|base| scope_at(base, &modules, anchor));
            let target = match (&layout, &scope) {
                _ if !is_internal(path, &names) => self.external_path(probe, package.as_ref(), importer, leaf, &bound),
                (Some(layout), Some(scope)) => {
                    let includers = &mut self.includers;
                    resolve_path(probe, includers, layout, scope, path, &names, importer)
                }
                _ => ModuleTarget::Unresolved,
            };
            out.push(ModuleImport {
                anchor,
                specifier: path.join("::"),
                target,
                kind: EdgeKinds::IMPORT,
            });
        }
        out.sort_by_key(|import| import.anchor);
        out
    }

    /// The target of a `use` path that does not start in the importer's own crate: a path through a
    /// name the file binds, or one starting at a crate name (see the module docs).
    fn external_path(
        &mut self,
        probe: &mut Probe<'_>,
        package: Option<&Package>,
        importer: &str,
        leaf: &UseLeaf,
        bound: &Bound<'_>,
    ) -> ModuleTarget {
        let (absolute, path) = match leaf.path.split_first() {
            Some((first, rest)) if first.is_empty() => (true, rest),
            _ => (false, leaf.path.as_slice()),
        };
        let Some(first) = path.first() else {
            return ModuleTarget::External;
        };
        // `use serde;` binds the name it names: that binding is no evidence the name is in scope.
        let binds_itself = path.len() == 1 && leaf.binds.as_ref() == Some(first);
        let in_scope = !absolute && !binds_itself && (bound.glob || bound.names.contains(first.as_str()));
        match self.crate_path(probe, package, importer, path) {
            ModuleTarget::Unresolved if in_scope => ModuleTarget::InScope,
            target => target,
        }
    }

    /// The target of `path`, whose first segment is a crate name, for code of `package`.
    fn crate_path(
        &mut self,
        probe: &mut Probe<'_>,
        package: Option<&Package>,
        importer: &str,
        path: &[String],
    ) -> ModuleTarget {
        let Some(((dir, manifest), name)) = package.zip(path.first()) else {
            return ModuleTarget::External;
        };
        let workspace = if manifest.dependencies.contains_key(name.as_str()) {
            self.workspace(probe, dir)
        } else {
            None
        };
        let section = workspace.as_ref().and_then(|manifest| manifest.workspace.as_ref());
        match classify_crate(name, dir, manifest, section) {
            CrateRef::External => ModuleTarget::External,
            CrateRef::Unknown => ModuleTarget::Unresolved,
            CrateRef::Local(package_dir) => {
                let member = self.manifest_in(probe, &package_dir);
                let root = lib_root(&package_dir, member.as_deref());
                let layout = CrateLayout::new(parent_dir(&root).to_string(), vec![root]);
                let mut absolute = vec!["crate".to_string()];
                absolute.extend(path[1..].iter().cloned());
                let includers = &mut self.includers;
                resolve_path(probe, includers, &layout, &[], &absolute, &AHashSet::new(), importer)
            }
        }
    }

    /// The manifest of the workspace holding the package in `package_dir`: the nearest `Cargo.toml`
    /// at or above it with a `[workspace]` table. Every candidate is a probe.
    fn workspace(&mut self, probe: &mut Probe<'_>, package_dir: &str) -> Option<Rc<Manifest>> {
        ancestors(package_dir).find_map(|dir| {
            self.manifest_in(probe, dir)
                .filter(|manifest| manifest.workspace.is_some())
        })
    }

    /// The parse of `dir/Cargo.toml`, recorded as a probe; `None` when there is none.
    fn manifest_in(&mut self, probe: &mut Probe<'_>, dir: &str) -> Option<Rc<Manifest>> {
        let manifest = join(dir, "Cargo.toml");
        probe.record(&manifest);
        let root = probe.root;
        self.manifests
            .entry(manifest)
            .or_insert_with_key(|manifest| {
                let text = std::fs::read_to_string(root.join(manifest)).ok()?;
                Some(Rc::new(parse_manifest(&text, parent_dir(manifest))))
            })
            .clone()
    }

    /// The directory of the nearest `Cargo.toml` above `importer` and its parse; every candidate is
    /// a probe.
    fn manifest(&mut self, probe: &mut Probe<'_>, importer: &str) -> Option<Package> {
        ancestors(parent_dir(importer)).find_map(|dir| Some((dir.to_string(), self.manifest_in(probe, dir)?)))
    }
}

/// `dir` and each directory above it, up to the repository root `""`.
fn ancestors(dir: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(dir), |dir| (!dir.is_empty()).then(|| parent_dir(dir)))
}

/// The crate `importer` belongs to, in the package in `manifest_dir` declaring `targets`: the target
/// root it is, else the standard-layout crate it falls in, unless a declared target root's directory
/// holds it more closely.
fn crate_layout(probe: &mut Probe<'_>, importer: &str, manifest_dir: &str, targets: &[String]) -> Option<CrateLayout> {
    if targets.iter().any(|target| target == importer) {
        return Some(CrateLayout::new(
            parent_dir(importer).to_string(),
            vec![importer.to_string()],
        ));
    }
    let standard = standard_layout(probe, manifest_dir, importer);
    let declared = targets
        .iter()
        .filter(|target| strip_dir(importer, parent_dir(target)).is_some())
        .max_by(|a, b| (parent_dir(a).len(), *a).cmp(&(parent_dir(b).len(), *b)));
    match (standard, declared) {
        (Some(standard), Some(target)) if parent_dir(target).len() <= standard.dir.len() => Some(standard),
        (_, Some(target)) => Some(CrateLayout::new(parent_dir(target).to_string(), vec![target.clone()])),
        (standard, None) => standard,
    }
}

/// The crate holding `importer` in Cargo's standard layout of the package in `manifest_dir`. A file
/// in `tests/x/` (likewise `src/bin/`, `examples/`, `benches/`) belongs to the crate rooted at
/// `tests/x/main.rs`, a probe, when there is one; else `tests/x/` is a module directory shared by
/// the crates in `tests/` (see [`CrateLayout::root_files`]).
fn standard_layout(probe: &mut Probe<'_>, manifest_dir: &str, importer: &str) -> Option<CrateLayout> {
    let rel = strip_dir(importer, manifest_dir)?;
    for top in ["src/bin", "tests", "examples", "benches"] {
        let Some(rest) = rel.strip_prefix(top).and_then(|rest| rest.strip_prefix('/')) else {
            continue;
        };
        let top_dir = join(manifest_dir, top);
        return Some(match rest.split_once('/') {
            None => CrateLayout::new(top_dir, vec![importer.to_string()]),
            Some((target, _)) => {
                let dir = join(&top_dir, target);
                let main = join(&dir, "main.rs");
                if probe.indexed(&main) {
                    CrateLayout::new(dir, vec![main])
                } else {
                    CrateLayout {
                        roots: vec![format!("{dir}.rs")],
                        dir: top_dir,
                        shared: Some(target.to_string()),
                    }
                }
            }
        });
    }
    if rel.starts_with("src/") {
        let dir = join(manifest_dir, "src");
        let roots = if rel == "src/main.rs" {
            vec![importer.to_string()]
        } else {
            vec![join(&dir, "lib.rs"), join(&dir, "main.rs")]
        };
        return Some(CrateLayout::new(dir, roots));
    }
    (rel == "build.rs").then(|| CrateLayout::new(manifest_dir.to_string(), vec![importer.to_string()]))
}

/// Every module the file declares, in source order, inline ones with their body span. A `mod` item
/// is bodiless when its text ends in `;`; without the source no module can be told apart, so none is
/// returned.
fn declared_modules(symbols: &[Symbol], source: Option<&[u8]>) -> Vec<DeclaredModule> {
    let Some(source) = source else {
        return Vec::new();
    };
    let mut modules: Vec<DeclaredModule> = symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Module && symbol.end_byte > symbol.start_byte)
        .filter_map(|symbol| {
            let last = *source.get(symbol.end_byte as usize - 1)?;
            Some(DeclaredModule {
                name: symbol.name.clone(),
                start: symbol.start_byte,
                end: symbol.end_byte,
                inline: last == b'}',
            })
        })
        .collect();
    modules.sort_by_key(|module| module.start);
    modules
}

/// The names of the inline modules enclosing byte `at`, outermost first.
fn enclosing_inline(modules: &[DeclaredModule], at: u32) -> impl Iterator<Item = &str> {
    modules
        .iter()
        .filter(move |module| module.inline && module.start < at && at < module.end)
        .map(|module| module.name.as_str())
}

/// The module path in scope at byte `at`: the file's own path plus the inline modules around `at`.
fn scope_at(base: &[String], modules: &[DeclaredModule], at: u32) -> Vec<String> {
    let mut scope = base.to_vec();
    scope.extend(enclosing_inline(modules, at).map(str::to_string));
    scope
}

/// The file a `mod x;` declaration loads: its `#[path]` file, else `x.rs` or `x/mod.rs` in the
/// directory of the enclosing module.
fn declared_module_file(
    probe: &mut Probe<'_>,
    importer: &str,
    is_root: bool,
    modules: &[DeclaredModule],
    module: &DeclaredModule,
    source: Option<&[u8]>,
) -> ModuleTarget {
    let dir = parent_dir(importer);
    let mut inline = enclosing_inline(modules, module.start).peekable();
    if inline.peek().is_none()
        && let Some(path) = source.and_then(|source| path_attribute(source, module.start as usize))
    {
        return match normalize(&join(dir, &path)) {
            Some(file) if probe.indexed(&file) => ModuleTarget::Files(vec![file]),
            _ => ModuleTarget::Unresolved,
        };
    }
    let file = importer.rsplit('/').next().unwrap_or(importer);
    let mut base = match file.strip_suffix(".rs") {
        Some(stem) if file != "mod.rs" && !is_root => join(dir, stem),
        _ => dir.to_string(),
    };
    for name in inline {
        base = join(&base, name);
    }
    let base = join(&base, &module.name);
    module_file_at(probe, &base).map_or(ModuleTarget::Unresolved, |file| ModuleTarget::Files(vec![file]))
}

/// `base.rs` or `base/mod.rs`, whichever is indexed.
fn module_file_at(probe: &mut Probe<'_>, base: &str) -> Option<String> {
    [format!("{base}.rs"), format!("{base}/mod.rs")]
        .into_iter()
        .find(|candidate| probe.indexed(candidate))
}

/// The `#[path = "…"]` attribute directly above byte `start`, skipping other attributes and line
/// comments.
fn path_attribute(source: &[u8], start: usize) -> Option<String> {
    let mut before = std::str::from_utf8(source.get(..start)?).ok()?;
    loop {
        before = before.trim_end();
        if before.ends_with(']') {
            let open = before.rfind("#[")?;
            let attribute = &before[open + 2..before.len() - 1];
            if let Some(value) = attribute
                .trim()
                .strip_prefix("path")
                .map(str::trim_start)
                .and_then(|rest| rest.strip_prefix('='))
            {
                return Some(value.trim().trim_matches('"').to_string());
            }
            before = &before[..open];
        } else {
            let line_start = before.rfind('\n').map_or(0, |at| at + 1);
            if !before[line_start..].trim_start().starts_with("//") {
                return None;
            }
            before = &before[..line_start];
        }
    }
}

/// The module path of `importer` within its crate: `[]` for the crate root, `["a", "b"]` for
/// `a/b.rs` or `a/b/mod.rs`.
fn module_path(layout: &CrateLayout, importer: &str) -> Option<Vec<String>> {
    if layout.is_root(importer) {
        return Some(Vec::new());
    }
    let stem = strip_dir(importer, &layout.dir)?.strip_suffix(".rs")?;
    let mut path: Vec<String> = stem.split('/').map(str::to_string).collect();
    if path.last().is_some_and(|last| last == "mod") {
        path.pop();
    }
    Some(path)
}

/// Whether a `use` path names a module of this crate.
fn is_internal(path: &[String], declared: &AHashSet<&str>) -> bool {
    path.first().is_some_and(|first| {
        matches!(first.as_str(), "crate" | "$crate" | "self" | "super") || declared.contains(first.as_str())
    })
}

/// Resolve an internal `use` path seen in module `scope` to the longest module-file prefix.
fn resolve_path(
    probe: &mut Probe<'_>,
    includers: &mut AHashMap<String, Vec<String>>,
    layout: &CrateLayout,
    scope: &[String],
    path: &[String],
    declared: &AHashSet<&str>,
    importer: &str,
) -> ModuleTarget {
    let mut segments = path.iter().map(String::as_str).peekable();
    let mut absolute: Vec<String> = match segments.peek().copied() {
        Some("crate" | "$crate") => {
            segments.next();
            Vec::new()
        }
        Some(first) if first == "self" || first == "super" || declared.contains(first) => {
            if first == "self" {
                segments.next();
            }
            let mut base = scope.to_vec();
            while segments.next_if_eq(&"super").is_some() {
                if base.pop().is_none() {
                    return ModuleTarget::Unresolved;
                }
            }
            base
        }
        _ => return ModuleTarget::External,
    };
    absolute.extend(segments.map(str::to_string));

    let mut found = None;
    for depth in 1..=absolute.len() {
        match module_file_at(probe, &join(&layout.dir, &absolute[..depth].join("/"))) {
            Some(file) => found = Some(file),
            None => break,
        }
    }
    let found = match found {
        Some(file) => vec![file],
        None => layout.root_files(probe, includers),
    };
    match found.as_slice() {
        [] => ModuleTarget::Unresolved,
        [file] if file == importer => ModuleTarget::SelfFile,
        _ => ModuleTarget::Files(found),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    Ident(&'a str),
    PathSep,
    Open,
    Close,
    OpenParen,
    CloseParen,
    Comma,
    Star,
    Semi,
    Other,
}

fn tokens(source: &str) -> Vec<Token<'_>> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        match byte {
            b if b.is_ascii_whitespace() => at += 1,
            b'/' if next == Some(b'/') => {
                at = memchr::memchr(b'\n', &bytes[at..]).map_or(bytes.len(), |end| at + end);
            }
            b'/' if next == Some(b'*') => {
                at = memchr::memmem::find(&bytes[at + 2..], b"*/").map_or(bytes.len(), |end| at + 2 + end + 2);
            }
            b':' if next == Some(b':') => {
                out.push(Token::PathSep);
                at += 2;
            }
            b'{' | b'}' | b'(' | b')' | b',' | b'*' | b';' => {
                out.push(match byte {
                    b'{' => Token::Open,
                    b'}' => Token::Close,
                    b'(' => Token::OpenParen,
                    b')' => Token::CloseParen,
                    b',' => Token::Comma,
                    b';' => Token::Semi,
                    _ => Token::Star,
                });
                at += 1;
            }
            b if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80 => {
                let start = at;
                while at < bytes.len()
                    && (bytes[at].is_ascii_alphanumeric() || matches!(bytes[at], b'_' | b'$' | 0x80..))
                {
                    at += 1;
                }
                let ident = &source[start..at];
                if ident == "r" && bytes.get(at) == Some(&b'#') {
                    at += 1;
                    continue;
                }
                out.push(Token::Ident(ident));
            }
            _ => {
                out.push(Token::Other);
                at += 1;
            }
        }
    }
    out
}

/// One leaf of a `use` tree: its path and the name it binds in the file (`None` for a glob or `_`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct UseLeaf {
    path: Vec<String>,
    binds: Option<String>,
    /// The leaf is a glob (`a::*`), bringing in every public name of `path`.
    glob: bool,
}

/// The leaf paths of a `use` declaration's tree, `self` leaves folded into their module and a
/// leading `::` kept as an empty first segment: `pub use crate::a::{self, b::C as D, e::*};` gives
/// `crate::a`, `crate::a::b::C` and `crate::a::e`.
#[cfg(test)]
fn use_paths(raw: &str) -> Vec<Vec<String>> {
    use_leaves(raw).into_iter().map(|leaf| leaf.path).collect()
}

/// The leaves of a `use` declaration's tree (see [`use_paths`]) with the names they bind: `D` for
/// `b::C as D`, `a` for `a::{self}`, none for `e::*`.
fn use_leaves(raw: &str) -> Vec<UseLeaf> {
    let tokens = tokens(raw);
    let mut at = 0;
    if tokens.first() == Some(&Token::Ident("pub")) {
        at = 1;
        if tokens.get(at) == Some(&Token::OpenParen) {
            while tokens.get(at).is_some_and(|token| *token != Token::CloseParen) {
                at += 1;
            }
            at += 1;
        }
    }
    if tokens.get(at) != Some(&Token::Ident("use")) {
        return Vec::new();
    }
    at += 1;
    let mut out = Vec::new();
    use_tree(&tokens, &mut at, Vec::new(), &mut out);
    for leaf in &mut out {
        if leaf.path.len() > 1 && leaf.path.last().is_some_and(|last| last == "self") {
            leaf.path.pop();
            if leaf.binds.as_deref() == Some("self") {
                leaf.binds = leaf.path.last().cloned();
            }
        }
    }
    out.retain(|leaf| !leaf.path.is_empty() && leaf.path.as_slice() != ["self"]);
    out
}

fn use_tree(tokens: &[Token<'_>], at: &mut usize, mut prefix: Vec<String>, out: &mut Vec<UseLeaf>) {
    if prefix.is_empty() && tokens.get(*at) == Some(&Token::PathSep) {
        prefix.push(String::new());
        *at += 1;
    }
    loop {
        match tokens.get(*at) {
            Some(Token::Ident(name)) => {
                *at += 1;
                prefix.push((*name).to_string());
                if tokens.get(*at) == Some(&Token::PathSep) {
                    *at += 1;
                    continue;
                }
                let mut binds = Some((*name).to_string());
                if tokens.get(*at) == Some(&Token::Ident("as")) {
                    binds = match tokens.get(*at + 1) {
                        Some(Token::Ident(alias)) if *alias != "_" => Some((*alias).to_string()),
                        _ => None,
                    };
                    *at += 2;
                }
                out.push(UseLeaf {
                    path: prefix,
                    binds,
                    glob: false,
                });
                return;
            }
            Some(Token::Star) => {
                *at += 1;
                out.push(UseLeaf {
                    path: prefix,
                    binds: None,
                    glob: true,
                });
                return;
            }
            Some(Token::Open) => {
                *at += 1;
                loop {
                    if tokens.get(*at) == Some(&Token::Close) {
                        *at += 1;
                        return;
                    }
                    let before = *at;
                    use_tree(tokens, at, prefix.clone(), out);
                    match tokens.get(*at) {
                        Some(Token::Comma) => *at += 1,
                        Some(Token::Close) => {
                            *at += 1;
                            return;
                        }
                        _ if *at == before => return,
                        _ => {}
                    }
                }
            }
            _ => return,
        }
    }
}

#[cfg(test)]
#[path = "module_rust_tests.rs"]
mod tests;
