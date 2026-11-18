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
mod tests {
    use super::*;
    use crate::intel::resolver::ProbeSet;

    fn paths(raw: &str) -> Vec<String> {
        use_paths(raw).into_iter().map(|path| path.join("::")).collect()
    }

    #[test]
    fn use_trees_expand_to_leaf_paths() {
        assert_eq!(paths("use crate::a::B;"), ["crate::a::B"]);
        assert_eq!(
            paths("pub(crate) use crate::a::{self, b::C as D, e::*, /* x */ f};"),
            ["crate::a", "crate::a::b::C", "crate::a::e", "crate::a::f"]
        );
        assert_eq!(paths("use super::*;"), ["super"]);
        assert_eq!(paths("use ::std::fmt;"), ["::std::fmt"]);
        assert_eq!(
            paths("use std::{fmt, io::{self, Read}};"),
            ["std::fmt", "std::io", "std::io::Read"]
        );
        assert_eq!(paths("pub use r#async::run;"), ["async::run"]);
        assert_eq!(paths("use a::b as _;"), ["a::b"]);
        assert!(paths("extern crate foo;").is_empty());
    }

    #[test]
    fn path_attribute_reads_the_attribute_above_a_module() {
        let src = b"#[cfg(unix)]\n#[path = \"sys/unix.rs\"]\n// the platform layer\nmod sys;\n";
        let at = src.windows(4).position(|w| w == b"mod ").unwrap();
        assert_eq!(path_attribute(src, at).as_deref(), Some("sys/unix.rs"));
        let plain = b"fn f() {}\nmod a;\n";
        let at = plain.windows(4).position(|w| w == b"mod ").unwrap();
        assert_eq!(path_attribute(plain, at), None);
        assert_eq!(normalize("a/./b/../c.rs").as_deref(), Some("a/c.rs"));
        assert_eq!(normalize("../x.rs"), None);
    }

    #[test]
    fn module_paths_follow_the_crate_layout() {
        let layout = CrateLayout {
            dir: "core/src".to_string(),
            roots: vec!["core/src/lib.rs".to_string(), "core/src/main.rs".to_string()],
        };
        assert_eq!(module_path(&layout, "core/src/lib.rs"), Some(vec![]));
        assert_eq!(module_path(&layout, "core/src/main.rs"), Some(vec![]));
        assert_eq!(module_path(&layout, "core/src/a/mod.rs"), Some(vec!["a".to_string()]));
        assert_eq!(
            module_path(&layout, "core/src/a/b.rs"),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(module_path(&layout, "other/x.rs"), None);
    }

    fn symbol(name: &str, kind: SymbolKind, start: usize, end: usize) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            start_byte: start as u32,
            end_byte: end as u32,
            start_row: 0,
            start_col: 0,
            signature: None,
            decorators: Vec::new(),
        }
    }

    /// An outline of `source` with its `mod` items and `use`/`extern crate` declarations, found by
    /// line, as the L1 extractor records them.
    fn outline(source: &str) -> (Vec<Symbol>, Vec<Import>) {
        let mut symbols = Vec::new();
        let mut imports = Vec::new();
        let mut offset = 0;
        let mut open: Vec<(String, usize)> = Vec::new();
        for line in source.split_inclusive('\n') {
            let text = line.trim();
            let start = offset + line.find(text).unwrap_or(0);
            if let Some(rest) = text.strip_prefix("pub ").unwrap_or(text).strip_prefix("mod ") {
                let name = rest.trim_end_matches([';', '{', ' ', '}']).to_string();
                if text.ends_with([';', '}']) {
                    symbols.push(symbol(&name, SymbolKind::Module, start, start + text.len()));
                } else {
                    open.push((name, start));
                }
            } else if text == "}" {
                let (name, start) = open.pop().unwrap();
                symbols.push(symbol(
                    &name,
                    SymbolKind::Module,
                    start,
                    offset + line.find('}').unwrap() + 1,
                ));
            } else if text.starts_with("use ") || text.starts_with("pub use ") {
                imports.push(Import {
                    module: None,
                    raw: text.to_string(),
                    start_byte: start as u32,
                    end_byte: (start + text.len()) as u32,
                });
            } else if let Some(rest) = text.strip_prefix("extern crate ") {
                imports.push(Import {
                    module: Some(rest.trim_end_matches(';').split(' ').next().unwrap().to_string()),
                    raw: text.to_string(),
                    start_byte: start as u32,
                    end_byte: (start + text.len()) as u32,
                });
            }
            offset += line.len();
        }
        (symbols, imports)
    }

    /// Resolve `importer` in a repository holding `files` (all indexed) on disk.
    fn resolve(files: &[(&str, &str)], importer: &str) -> (Vec<(String, ModuleTarget)>, ProbeSet) {
        let dir = tempfile::tempdir().unwrap();
        for (rel, body) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let indexed: AHashSet<String> = files
            .iter()
            .filter(|(rel, _)| rel.ends_with(".rs"))
            .map(|(rel, _)| (*rel).to_string())
            .collect();
        let lookup = |rel: &str| indexed.contains(rel);
        let source = files.iter().find(|(rel, _)| *rel == importer).unwrap().1;
        let mut probes = ProbeSet::default();
        let (symbols, imports) = outline(source);
        let content_hash = crate::hashing::hex(&crate::hashing::hash_bytes(source.as_bytes()));
        let imports = RustModules::default().resolve(
            &mut Probe {
                root: dir.path(),
                indexed: &lookup,
                probes: &mut probes,
            },
            importer,
            &content_hash,
            &symbols,
            &imports,
        );
        let out = imports
            .into_iter()
            .map(|import| (import.specifier, import.target))
            .collect();
        (out, probes)
    }

    fn files(rels: &[&str]) -> ModuleTarget {
        ModuleTarget::Files(rels.iter().map(|rel| (*rel).to_string()).collect())
    }

    const MANIFEST: &str = "[package]\nname = \"demo\"\n";

    #[test]
    fn mod_declarations_load_sibling_and_nested_module_files() {
        let lib =
            "mod a;\npub mod b;\nmod gone;\n#[path = \"sys/unix.rs\"]\nmod sys;\nmod inline {\n    mod deep;\n}\n";
        let (resolved, probes) = resolve(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", lib),
                ("src/a.rs", "mod c;\n"),
                ("src/a/c.rs", ""),
                ("src/b/mod.rs", ""),
                ("src/sys/unix.rs", ""),
                ("src/inline/deep.rs", ""),
            ],
            "src/lib.rs",
        );
        assert_eq!(
            resolved,
            [
                ("mod a".to_string(), files(&["src/a.rs"])),
                ("mod b".to_string(), files(&["src/b/mod.rs"])),
                ("mod gone".to_string(), ModuleTarget::Unresolved),
                ("mod sys".to_string(), files(&["src/sys/unix.rs"])),
                ("mod deep".to_string(), files(&["src/inline/deep.rs"])),
            ]
        );
        assert!(probes.contains("src/gone.rs") && probes.contains("src/gone/mod.rs"));
        assert!(probes.contains("src/Cargo.toml") && probes.contains("Cargo.toml"));

        let (resolved, _) = resolve(
            &[("Cargo.toml", MANIFEST), ("src/a.rs", "mod c;\n"), ("src/a/c.rs", "")],
            "src/a.rs",
        );
        assert_eq!(resolved, [("mod c".to_string(), files(&["src/a/c.rs"]))]);
    }

    #[test]
    fn use_paths_resolve_to_the_longest_module_file() {
        let b = "use crate::a::Thing;\nuse crate::a::c::{self, Deep};\nuse crate::Root;\nuse super::sibling::f;\n\
                 use self::inner::g;\nuse std::fmt;\nuse serde::Serialize;\nextern crate alloc;\nmod inner;\n\
                 mod tests {\n    use super::*;\n    use super::super::a::Thing;\n}\n";
        let (resolved, _) = resolve(
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", ""),
                ("src/a.rs", ""),
                ("src/a/c.rs", ""),
                ("src/m/mod.rs", ""),
                ("src/m/b.rs", b),
                ("src/m/sibling.rs", ""),
                ("src/m/b/inner.rs", ""),
            ],
            "src/m/b.rs",
        );
        assert_eq!(
            resolved,
            [
                ("crate::a::Thing".to_string(), files(&["src/a.rs"])),
                ("crate::a::c".to_string(), files(&["src/a/c.rs"])),
                ("crate::a::c::Deep".to_string(), files(&["src/a/c.rs"])),
                ("crate::Root".to_string(), files(&["src/lib.rs"])),
                ("super::sibling::f".to_string(), files(&["src/m/sibling.rs"])),
                ("self::inner::g".to_string(), files(&["src/m/b/inner.rs"])),
                ("std::fmt".to_string(), ModuleTarget::External),
                ("serde::Serialize".to_string(), ModuleTarget::External),
                ("extern crate alloc".to_string(), ModuleTarget::External),
                ("mod inner".to_string(), files(&["src/m/b/inner.rs"])),
                ("super".to_string(), ModuleTarget::SelfFile),
                ("super::super::a::Thing".to_string(), files(&["src/m/mod.rs"])),
            ]
        );
    }

    #[test]
    fn crate_roots_cover_binaries_tests_and_a_missing_manifest() {
        let (resolved, _) = resolve(
            &[
                ("tool/Cargo.toml", MANIFEST),
                ("tool/src/bin/cli.rs", "mod args;\nuse crate::args::Args;\n"),
                ("tool/src/bin/args.rs", ""),
                ("tool/tests/it.rs", "mod common;\n"),
                ("tool/tests/common/mod.rs", ""),
            ],
            "tool/src/bin/cli.rs",
        );
        assert_eq!(
            resolved,
            [
                ("mod args".to_string(), files(&["tool/src/bin/args.rs"])),
                ("crate::args::Args".to_string(), files(&["tool/src/bin/args.rs"])),
            ]
        );
        let (resolved, _) = resolve(
            &[
                ("tool/Cargo.toml", MANIFEST),
                ("tool/tests/it.rs", "mod common;\n"),
                ("tool/tests/common/mod.rs", ""),
            ],
            "tool/tests/it.rs",
        );
        assert_eq!(
            resolved,
            [("mod common".to_string(), files(&["tool/tests/common/mod.rs"]))]
        );

        let (resolved, _) = resolve(
            &[
                ("Cargo.toml", MANIFEST),
                (
                    "tests/suite.rs",
                    "#[path = \"suite/case.rs\"]\nmod case;\nfn shared() {}\n",
                ),
                ("tests/suite/case.rs", "use super::shared;\n"),
            ],
            "tests/suite/case.rs",
        );
        assert_eq!(resolved, [("super::shared".to_string(), files(&["tests/suite.rs"]))]);

        let (resolved, _) = resolve(&[("loose/x.rs", "use crate::y::Z;\n")], "loose/x.rs");
        assert_eq!(resolved, [("crate::y::Z".to_string(), ModuleTarget::Unresolved)]);
    }
}
