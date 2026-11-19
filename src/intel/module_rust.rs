//! Rust file-level module resolution (ADR-0047 decision 4).
//!
//! - `mod x;` maps to `x.rs` or `x/mod.rs` in the declaring file's module directory (the file's own
//!   directory for `mod.rs` and crate roots, else `<dir>/<stem>/`), or to its `#[path]` file.
//! - A `use` path starting with `crate::`, `self::`, `super::` or a module the file declares maps to
//!   the longest prefix of it that is a module file under the crate root, the crate root being
//!   found from the nearest `Cargo.toml` (`src/`, `src/bin/*`, `tests/*`, `examples/*`,
//!   `benches/*`; a file in `tests/x/` belongs to `tests/x/main.rs`, else to `tests/x.rs`, which
//!   loads it through `#[path]`). With no module file at all, the path names an item of the crate
//!   root file.
//! - Anything else (`std::`, a dependency, `::x`) is external.
//!
//! Inline modules (`mod tests { … }`) are scopes inside one file: `use super::*` in one resolves to
//! the file itself, which is not an edge. `pub use` re-exports are not followed: an importer gets an
//! edge to the module it names, the first hop.

use ahash::{AHashMap, AHashSet};

use crate::extract::{Import, Symbol, SymbolKind};
use crate::intel::module_resolver::{ModuleImport, ModuleTarget, Probe, join, parent_dir, strip_dir};

/// Where a crate's module tree is rooted.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CrateLayout {
    /// The directory module paths start from.
    dir: String,
    /// The crate root file's candidates in order; the first indexed one is the root. Probed only
    /// when a path falls back to the root, so editing `lib.rs` does not re-stitch the whole crate.
    roots: Vec<String>,
}

impl CrateLayout {
    /// Whether `importer` is (a candidate for) the crate root file.
    fn is_root(&self, importer: &str) -> bool {
        self.roots.iter().any(|root| root == importer)
    }

    /// The crate root file, when one is indexed.
    fn root_file(&self, probe: &mut Probe<'_>) -> Option<String> {
        self.roots.iter().find(|root| probe.indexed(root)).cloned()
    }
}

/// A module declared in the importer: its name and, for an inline one, its body's byte span.
struct DeclaredModule {
    name: String,
    start: u32,
    end: u32,
    inline: bool,
}

/// Rust module resolver. Caches which directories hold a `Cargo.toml`.
#[derive(Default)]
pub(crate) struct RustModules {
    manifests: AHashMap<String, bool>,
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
        let layout = self.crate_layout(probe, importer);
        let file_scope = layout.as_ref().and_then(|layout| module_path(layout, importer));
        let is_root = layout.as_ref().is_some_and(|layout| layout.is_root(importer));
        let names: AHashSet<&str> = modules.iter().map(|module| module.name.as_str()).collect();

        let mut out = Vec::new();
        for module in modules.iter().filter(|module| !module.inline) {
            let target = declared_module_file(probe, importer, is_root, &modules, module, source.as_deref());
            out.push(ModuleImport {
                anchor: module.start,
                specifier: format!("mod {}", module.name),
                target,
            });
        }
        for import in imports {
            if let Some(name) = &import.module {
                let target = if name == "self" {
                    ModuleTarget::SelfFile
                } else {
                    ModuleTarget::External
                };
                out.push(ModuleImport {
                    anchor: import.start_byte,
                    specifier: format!("extern crate {name}"),
                    target,
                });
                continue;
            }
            let scope = file_scope
                .as_ref()
                .map(|base| scope_at(base, &modules, import.start_byte));
            for path in use_paths(&import.raw) {
                let target = match (&layout, &scope) {
                    _ if !is_internal(&path, &names) => ModuleTarget::External,
                    (Some(layout), Some(scope)) => resolve_path(probe, layout, scope, &path, &names, importer),
                    _ => ModuleTarget::Unresolved,
                };
                out.push(ModuleImport {
                    anchor: import.start_byte,
                    specifier: path.join("::"),
                    target,
                });
            }
        }
        out.sort_by_key(|import| import.anchor);
        out
    }

    /// The crate `importer` belongs to, from the nearest `Cargo.toml` above it.
    fn crate_layout(&mut self, probe: &mut Probe<'_>, importer: &str) -> Option<CrateLayout> {
        let manifest_dir = self.manifest_dir(probe, importer)?;
        let rel = strip_dir(importer, &manifest_dir)?;
        for top in ["src/bin", "tests", "examples", "benches"] {
            let Some(rest) = rel.strip_prefix(top).and_then(|rest| rest.strip_prefix('/')) else {
                continue;
            };
            let top_dir = join(&manifest_dir, top);
            return Some(match rest.split_once('/') {
                None => CrateLayout {
                    dir: top_dir,
                    roots: vec![importer.to_string()],
                },
                Some((target, _)) => {
                    let dir = join(&top_dir, target);
                    let roots = vec![join(&dir, "main.rs"), format!("{dir}.rs")];
                    CrateLayout { dir, roots }
                }
            });
        }
        if rel.starts_with("src/") {
            let dir = join(&manifest_dir, "src");
            let roots = if rel == "src/main.rs" {
                vec![importer.to_string()]
            } else {
                vec![join(&dir, "lib.rs"), join(&dir, "main.rs")]
            };
            return Some(CrateLayout { dir, roots });
        }
        (rel == "build.rs").then(|| CrateLayout {
            dir: manifest_dir,
            roots: vec![importer.to_string()],
        })
    }

    /// The directory of the nearest `Cargo.toml` above `importer`; every candidate is a probe.
    fn manifest_dir(&mut self, probe: &mut Probe<'_>, importer: &str) -> Option<String> {
        let mut dir = parent_dir(importer);
        loop {
            let manifest = join(dir, "Cargo.toml");
            probe.record(&manifest);
            let root = probe.root;
            let present = *self
                .manifests
                .entry(manifest)
                .or_insert_with_key(|manifest| root.join(manifest).is_file());
            if present {
                return Some(dir.to_string());
            }
            if dir.is_empty() {
                return None;
            }
            dir = parent_dir(dir);
        }
    }
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
        Some(stem) if !matches!(file, "mod.rs" | "lib.rs" | "main.rs") && !is_root => join(dir, stem),
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

/// `path` with `.` and `..` segments applied, or `None` when it leaves the repository.
fn normalize(path: &str) -> Option<String> {
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
    if matches!(path.as_slice(), [only] if only == "lib" || only == "main") {
        path.clear();
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
    if found.is_none() {
        found = layout.root_file(probe);
    }
    match found {
        Some(file) if file == importer => ModuleTarget::SelfFile,
        Some(file) => ModuleTarget::Files(vec![file]),
        None => ModuleTarget::Unresolved,
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
            b'{' | b'}' | b'(' | b')' | b',' | b'*' => {
                out.push(match byte {
                    b'{' => Token::Open,
                    b'}' => Token::Close,
                    b'(' => Token::OpenParen,
                    b')' => Token::CloseParen,
                    b',' => Token::Comma,
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

/// The leaf paths of a `use` declaration's tree, `self` leaves folded into their module and a
/// leading `::` kept as an empty first segment: `pub use crate::a::{self, b::C as D, e::*};` gives
/// `crate::a`, `crate::a::b::C` and `crate::a::e`.
fn use_paths(raw: &str) -> Vec<Vec<String>> {
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
    for path in &mut out {
        if path.len() > 1 && path.last().is_some_and(|last| last == "self") {
            path.pop();
        }
    }
    out.retain(|path| !path.is_empty() && path.as_slice() != ["self"]);
    out
}

fn use_tree(tokens: &[Token<'_>], at: &mut usize, mut prefix: Vec<String>, out: &mut Vec<Vec<String>>) {
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
                if tokens.get(*at) == Some(&Token::Ident("as")) {
                    *at += 2;
                }
                out.push(prefix);
                return;
            }
            Some(Token::Star) => {
                *at += 1;
                out.push(prefix);
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
