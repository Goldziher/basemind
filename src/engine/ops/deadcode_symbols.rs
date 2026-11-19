//! The unreferenced symbols of `basemind code deadcode` (#123), and the one text pass over the
//! indexed code that the whole report reads.
//!
//! Use is judged by name, so "unreferenced" is a lower bound on use (ADR-0017): a definition whose
//! name no other identifier in the indexed code spells is `high`; one whose name is spelled only in
//! comments is `medium`; one whose name is spelled only in strings is `low`, since a lookup by name
//! (serde, reflection, a registry) may use it. Exported names, entry points and trait-impl members
//! are never candidates; a language feature the index cannot see (dispatch, decorators, attributes,
//! outside importers) caps the tier and says why.
//!
//! A definition spells its own name once, so each definition's token is taken out of the code count
//! when the pass lexed it as code. Definitions are read from a file's outline only while the file is
//! the content the index describes; an edited file's tokens all count as uses, which can only hide a
//! finding, never invent one.

use std::sync::Arc;

use ahash::{AHashMap, AHashSet};
use memchr::memmem;
use rayon::prelude::*;

use super::deadcode_files::{FileFacts, java_test_file};
use super::deadcode_lex::{Class, identifiers, is_ident_byte};
use super::deadcode_rust::{self as rust, Container, HiddenModules, RUST_INERT_ATTRIBUTES};
use crate::engine::files::Sweep;
use crate::engine::freshness;
use crate::engine::l1_cache::FileMeta;
use crate::engine::types::code::kind_to_str;
use crate::engine::types::deadcode::{DeadSymbol, Tier};
use crate::extract::{FileMapL1, SymbolKind};
use crate::path::RelPath;

/// The languages whose definitions are judged, each with a rule for what it exports.
pub(super) const SYMBOL_LANGUAGES: &[&str] = &["rust", "go", "python", "javascript", "typescript", "tsx", "java"];

/// The candidate names, each once, by index.
#[derive(Debug, Default)]
pub(super) struct Names {
    index: AHashMap<Box<[u8]>, u32>,
    names: Vec<Box<str>>,
}

impl Names {
    pub(super) fn intern(&mut self, name: &str) -> u32 {
        if let Some(&at) = self.index.get(name.as_bytes()) {
            return at;
        }
        let at = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        self.index.insert(name.as_bytes().into(), at);
        self.names.push(name.into());
        at
    }

    pub(super) fn get(&self, token: &[u8]) -> Option<u32> {
        self.index.get(token).copied()
    }

    pub(super) fn name(&self, at: u32) -> &str {
        &self.names[at as usize]
    }

    pub(super) fn len(&self) -> usize {
        self.names.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// Rust: the signature starts with `pub`.
pub(super) const PUB: u8 = 1;
/// Java: `public` or `protected`.
pub(super) const JAVA_VISIBLE: u8 = 2;
/// Java: the declaration carries an annotation.
pub(super) const ANNOTATED: u8 = 4;

/// A definition that may be unused: indices into the file list and the name table, and the bits of
/// its signature the rules read, so the whole repository's candidates stay small.
#[derive(Debug, Clone)]
pub(super) struct Candidate {
    pub file: u32,
    pub language: &'static str,
    pub name: u32,
    pub kind: SymbolKind,
    pub start_byte: u32,
    pub line: u32,
    pub flags: u8,
    /// Python: the first decorator.
    pub decorator: Option<Box<str>>,
}

/// What the text around a candidate says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Local {
    /// Rust: an attribute marks the item an entry point (`#[test]`, `#[unsafe(no_mangle)]`).
    pub entry_attribute: bool,
    /// Rust: the first attribute that may generate a use of the item.
    pub effect_attribute: Option<Box<str>>,
    /// JS/TS: `export` on the declaration.
    pub exported: bool,
    /// JS/TS: `export default`: an importer binds it by any name, so its own name says nothing.
    pub default_export: bool,
    /// Python: the file's `__all__` was read, and whether it names the candidate.
    pub in_all: Option<bool>,
    /// Rust: the block the item sits in.
    pub container: Container,
    /// The file is an entry point a loader or tool runs, which may call its names by spelling.
    pub entry_file: bool,
}

/// One file of the text pass.
pub(super) struct ScanFile<'a> {
    pub path: &'a RelPath,
    pub meta: &'a FileMeta,
    /// The candidates defined in this file, a range of the candidate list.
    pub candidates: std::ops::Range<usize>,
    /// Unresolved specifiers whose line to find.
    pub specifiers: &'a [String],
    /// A loader or tool runs this file (see [`Local::entry_file`]).
    pub entry_file: bool,
}

/// What the text pass read.
#[derive(Default)]
pub(super) struct TextScan {
    /// Per file read: the facts its entry conventions read.
    pub facts: AHashMap<RelPath, FileFacts>,
    /// Per candidate name: identifiers spelling it in code, in strings and in comments.
    pub occurrences: Vec<[u32; 3]>,
    /// Per candidate name: definitions whose own token was lexed as code, in files read fresh.
    pub definitions: Vec<u32>,
    /// Per candidate: the text around it, when its file was read fresh.
    pub locals: Vec<Option<Local>>,
    /// Per file: the 1-based line of each unresolved specifier, in its order.
    pub specifier_lines: AHashMap<RelPath, Vec<Option<u32>>>,
    /// Files that could not be read: deleted, or grown past `scan.max_file_bytes`, since the scan.
    pub unreadable: usize,
    /// Files edited since the scan: their definitions are not taken out of the code count.
    pub edited: usize,
}

/// The candidates of one file's outline: eligible kinds with an identifier name, minus the names a
/// harness or runtime calls by their spelling.
pub(super) fn candidates_of(
    file: u32,
    path: &str,
    language: &'static str,
    l1: &FileMapL1,
    names: &mut Names,
    out: &mut Vec<Candidate>,
) {
    for symbol in &l1.symbols {
        let signature = symbol.signature.as_deref().unwrap_or("");
        if !eligible(symbol.kind)
            || !is_identifier(&symbol.name)
            || entry_name(path, language, &symbol.name, symbol.kind, signature)
        {
            continue;
        }
        let mut flags = 0;
        if signature.starts_with("pub ") || signature.starts_with("pub\t") {
            flags |= PUB;
        }
        if language == "java" {
            let mut words = signature.split(|c: char| !(c.is_alphanumeric() || c == '_'));
            if words.any(|word| word == "public" || word == "protected") {
                flags |= JAVA_VISIBLE;
            }
            if signature.contains('@') {
                flags |= ANNOTATED;
            }
        }
        out.push(Candidate {
            file,
            language,
            name: names.intern(&symbol.name),
            kind: symbol.kind,
            start_byte: symbol.start_byte,
            line: symbol.start_row + 1,
            flags,
            decorator: (language == "python")
                .then(|| symbol.decorators.first().map(|d| d.as_str().into()))
                .flatten(),
        });
    }
}

fn eligible(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function
            | SymbolKind::Method
            | SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Class
            | SymbolKind::Interface
            | SymbolKind::Trait
            | SymbolKind::Type
            | SymbolKind::Const
            | SymbolKind::Macro
            | SymbolKind::Variable
    )
}

/// A name one identifier token can spell.
fn is_identifier(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty() && !bytes[0].is_ascii_digit() && bytes.iter().all(|&b| is_ident_byte(b)) && name != "_"
}

/// `unittest` and `pytest` methods the runner calls by name.
const UNITTEST_HOOKS: &[&str] = &[
    "setUp",
    "tearDown",
    "setUpClass",
    "tearDownClass",
    "setUpModule",
    "tearDownModule",
    "asyncSetUp",
    "asyncTearDown",
    "setup_method",
    "teardown_method",
    "setup_class",
    "teardown_class",
];

/// JUnit 4 and 5 annotations whose method or class the runner calls.
const JUNIT_ANNOTATIONS: &[&str] = &[
    "Test",
    "ParameterizedTest",
    "RepeatedTest",
    "TestFactory",
    "TestTemplate",
    "BeforeEach",
    "AfterEach",
    "BeforeAll",
    "AfterAll",
    "Before",
    "After",
    "BeforeClass",
    "AfterClass",
    "Nested",
];

/// A Python file a test runner collects: every class in it may be a test case.
fn python_test_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.starts_with("test_") || name.ends_with("_test.py") || name == "tests.py"
}

/// Whether a Java signature carries a JUnit annotation (`@Test`, `@org.junit.jupiter.api.Test`).
fn junit_annotated(signature: &str) -> bool {
    signature.split('@').skip(1).any(|annotation| {
        let path = annotation
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
            .next()
            .unwrap_or_default();
        JUNIT_ANNOTATIONS.contains(&path.rsplit('.').next().unwrap_or(path))
    })
}

/// A name a harness, runtime or tool calls by its spelling.
pub(super) fn entry_name(path: &str, language: &str, name: &str, kind: SymbolKind, signature: &str) -> bool {
    if name == "main" {
        return true;
    }
    match language {
        "go" => {
            name == "init"
                || (path.ends_with("_test.go")
                    && ["Test", "Benchmark", "Example", "Fuzz"]
                        .iter()
                        .any(|prefix| name.starts_with(prefix)))
        }
        "python" => {
            (name.len() > 2 && name.starts_with('_') && name.ends_with('_'))
                || name.starts_with("test_")
                || name.starts_with("pytest_")
                || UNITTEST_HOOKS.contains(&name)
                || (kind == SymbolKind::Class && python_test_file(path))
        }
        "java" => {
            (matches!(kind, SymbolKind::Class | SymbolKind::Interface) && java_test_file(path))
                || junit_annotated(signature)
        }
        _ => false,
    }
}

/// Read every file once: count the identifiers spelling a candidate name by class and the
/// definitions among them, read the facts the entry conventions need, the text around each
/// candidate and the line of each unresolved specifier. `outline` gives a file's indexed outline.
pub(super) fn text_pass(
    sweep: &Sweep<'_>,
    outline: &(dyn Fn(&RelPath) -> Option<Arc<FileMapL1>> + Sync),
    files: &[ScanFile<'_>],
    candidates: &[Candidate],
    names: &Names,
) -> TextScan {
    let max = sweep.max_file_bytes();
    let per_file: Vec<Option<FileScan>> = files
        .par_iter()
        .map(|file| {
            let disk = sweep.path(file.path)?;
            let live = freshness::load(&disk, file.meta.into(), max).ok()?;
            Some(scan_one(file, &live.bytes, live.fresh, outline, candidates, names))
        })
        .collect();
    let mut out = TextScan {
        occurrences: vec![[0; 3]; names.len()],
        definitions: vec![0; names.len()],
        locals: vec![None; candidates.len()],
        ..TextScan::default()
    };
    for (file, scan) in files.iter().zip(per_file) {
        let Some(scan) = scan else {
            out.unreadable += 1;
            continue;
        };
        for (name, counts) in scan.counts {
            for (total, count) in out.occurrences[name as usize].iter_mut().zip(counts) {
                *total += count;
            }
        }
        for name in scan.defined {
            out.definitions[name as usize] += 1;
        }
        if scan.fresh {
            for (at, local) in file.candidates.clone().zip(scan.locals) {
                out.locals[at] = Some(local);
            }
        } else {
            out.edited += 1;
        }
        if let Some(lines) = scan.specifier_lines {
            out.specifier_lines.insert(file.path.clone(), lines);
        }
        out.facts.insert(file.path.clone(), scan.facts);
    }
    out
}

struct FileScan {
    fresh: bool,
    facts: FileFacts,
    counts: Vec<(u32, [u32; 3])>,
    defined: Vec<u32>,
    locals: Vec<Local>,
    specifier_lines: Option<Vec<Option<u32>>>,
}

fn scan_one(
    file: &ScanFile<'_>,
    bytes: &[u8],
    fresh: bool,
    outline: &(dyn Fn(&RelPath) -> Option<Arc<FileMapL1>> + Sync),
    candidates: &[Candidate],
    names: &Names,
) -> FileScan {
    let language = &*file.meta.language;
    let rust_file = language == "rust";
    let mut counts: AHashMap<u32, [u32; 3]> = AHashMap::new();
    // Name tokens in file order, with their class, for the definition count.
    let mut hits: Vec<(u32, u32, Class)> = Vec::new();
    let mut hidden = HiddenModules::default();
    let mut previous_mod = false;
    if !names.is_empty() || rust_file {
        identifiers(language, bytes, |token, class, at| {
            if let Some(name) = names.get(token) {
                counts.entry(name).or_default()[class as usize] += 1;
                if fresh {
                    hits.push((u32::try_from(at).unwrap_or(u32::MAX), name, class));
                }
            }
            if rust_file && class == Class::Code {
                rust::note_module_token(&mut hidden, previous_mod, bytes, token, at);
                previous_mod = token == b"mod";
            }
        });
    }
    let defined = if fresh && !hits.is_empty() {
        outline(file.path).map_or_else(Vec::new, |l1| defined_names(&l1, &hits, names))
    } else {
        Vec::new()
    };
    let mut facts = facts_of(language, bytes);
    if rust_file {
        hidden.rs_strings = rust::rs_strings(bytes);
        facts.rust = hidden;
    }
    let locals = if fresh {
        locals_of(
            &candidates[file.candidates.clone()],
            names,
            bytes,
            language,
            file.entry_file,
        )
    } else {
        Vec::new()
    };
    let specifier_lines = (!file.specifiers.is_empty()).then(|| {
        file.specifiers
            .iter()
            .map(|specifier| specifier_line(bytes, specifier).filter(|_| fresh))
            .collect()
    });
    FileScan {
        fresh,
        facts,
        counts: counts.into_iter().collect(),
        defined,
        locals,
        specifier_lines,
    }
}

/// The candidate names an outline defines whose own token (the first token of that name in the
/// definition's span) was lexed as code: each such token is a definition, not a use.
fn defined_names(l1: &FileMapL1, hits: &[(u32, u32, Class)], names: &Names) -> Vec<u32> {
    let mut taken = vec![false; hits.len()];
    let mut out = Vec::new();
    for symbol in &l1.symbols {
        if symbol.kind == SymbolKind::Impl {
            continue;
        }
        let Some(name) = names.get(symbol.name.as_bytes()) else {
            continue;
        };
        let end = symbol.end_byte.max(symbol.start_byte.saturating_add(1));
        let from = hits.partition_point(|hit| hit.0 < symbol.start_byte);
        let own = (from..hits.len())
            .take_while(|&at| hits[at].0 < end)
            .find(|&at| hits[at].1 == name && !taken[at]);
        if let Some(at) = own {
            taken[at] = true;
            if hits[at].2 == Class::Code {
                out.push(name);
            }
        }
    }
    out
}

/// The text around each candidate of one fresh file: one container pass for Rust, `__all__` read
/// once for Python.
fn locals_of(candidates: &[Candidate], names: &Names, bytes: &[u8], language: &str, entry_file: bool) -> Vec<Local> {
    let containers = if language == "rust" {
        let starts: Vec<usize> = candidates.iter().map(|c| c.start_byte as usize).collect();
        rust::containers(bytes, &starts)
    } else {
        Vec::new()
    };
    let all = if language == "python" { python_all(bytes) } else { None };
    candidates
        .iter()
        .enumerate()
        .map(|(at, candidate)| {
            let mut local = local_of(candidate, names.name(candidate.name), bytes, all.as_ref());
            if let Some(container) = containers.get(at) {
                local.container = *container;
            }
            local.entry_file = entry_file;
            local
        })
        .collect()
}

/// The names a Python file's `__all__` lists, read from each `__all__ = [...]`, `+= (...)` or
/// `.extend([...])`; `None` when the file has no `__all__` or one is not a literal list.
pub(super) fn python_all(bytes: &[u8]) -> Option<AHashSet<Box<str>>> {
    let mut out = AHashSet::new();
    let mut any = false;
    for at in memmem::find_iter(bytes, b"__all__") {
        let line_end = memchr::memchr(b'\n', &bytes[at..]).map_or(bytes.len(), |len| at + len);
        let open = bytes[at + 7..line_end]
            .iter()
            .position(|&b| b == b'[' || b == b'(')
            .map(|p| at + 7 + p)?;
        let mut depth = 0usize;
        let mut cursor = open;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'[' | b'(' => depth += 1,
                b']' | b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                quote @ (b'"' | b'\'') => {
                    let body = cursor + 1;
                    let close = memchr::memchr(quote, &bytes[body..]).map_or(bytes.len(), |len| body + len);
                    out.insert(String::from_utf8_lossy(&bytes[body..close]).into());
                    cursor = close;
                }
                _ => {}
            }
            cursor += 1;
        }
        any = true;
    }
    any.then_some(out)
}

/// The 1-based line of the first occurrence of `specifier` outside a comment line; a grouped Rust
/// `use a::{b, c}` is found by its `a::` prefix.
fn specifier_line(bytes: &[u8], specifier: &str) -> Option<u32> {
    let mut needles = vec![specifier.to_string()];
    if let Some((prefix, _)) = specifier.rsplit_once("::") {
        needles.push(format!("{prefix}::"));
    }
    needles.iter().find_map(|needle| {
        memmem::find_iter(bytes, needle.as_bytes()).find_map(|at| {
            let line_start = memchr::memrchr(b'\n', &bytes[..at]).map_or(0, |p| p + 1);
            let line = bytes[line_start..at].trim_ascii_start();
            let comment = [b"//".as_slice(), b"#", b"*", b"/*"]
                .iter()
                .any(|marker| line.starts_with(marker));
            (!comment).then(|| u32::try_from(memchr::memchr_iter(b'\n', &bytes[..at]).count() + 1).ok())?
        })
    })
}

/// The facts of a file that its language's entry conventions read.
pub(super) fn facts_of(language: &str, bytes: &[u8]) -> FileFacts {
    let mut facts = FileFacts::default();
    match language {
        "go" => {
            for line in bytes.split(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(line);
                let line = line.trim();
                if facts.go_package.is_none()
                    && let Some(package) = line.strip_prefix("package ")
                {
                    facts.go_package = package.split_whitespace().next().map(str::to_string);
                }
                // cgo: `//export Name` exports a function to C.
                if let Some(name) = line.strip_prefix("//export ") {
                    facts.go_exports.push(name.trim().to_string());
                }
            }
        }
        "python" => {
            facts.main_guard = bytes.split(|&b| b == b'\n').any(|line| {
                let line = line.trim_ascii();
                line.starts_with(b"if")
                    && memmem::find(line, b"__name__").is_some()
                    && memmem::find(line, b"__main__").is_some()
                    && memmem::find(line, b"==").is_some()
            });
            facts.module_strings = module_strings(bytes);
        }
        // `static void main(`, `static public void main(` and a Java 21 instance `void main(`.
        "java" => facts.java_main = memmem::find(bytes, b"void main(").is_some(),
        _ => {}
    }
    facts
}

/// Dotted module paths kept per file.
const MODULE_STRINGS_MAX: usize = 4096;

/// The dotted module paths spelled as string literals of a Python file (`"app.urls"`,
/// `'pkg.tasks:run'` gives `pkg.tasks`): what a framework loads by name.
fn module_strings(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(len) = memchr::memchr2(b'"', b'\'', &bytes[at..]) {
        let open = at + len;
        let quote = bytes[open];
        let body = open + 1;
        let Some(close) = bytes[body..]
            .iter()
            .position(|&b| b == quote || b == b'\n')
            .map(|p| body + p)
        else {
            break;
        };
        at = close + 1;
        if bytes[close] != quote {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes[body..close]);
        let module = text.split(':').next().unwrap_or_default();
        let dotted = module.split('.').count() >= 2
            && module.split('.').all(|part| {
                part.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
                    && part.chars().all(|c| c.is_alphanumeric() || c == '_')
            });
        if dotted && out.len() < MODULE_STRINGS_MAX {
            out.push(module.to_string());
        }
    }
    out
}

/// The text around one candidate in its file's fresh bytes (the Rust container is read separately,
/// for the whole file at once).
pub(super) fn local_of(candidate: &Candidate, name: &str, bytes: &[u8], all: Option<&AHashSet<Box<str>>>) -> Local {
    let start = (candidate.start_byte as usize).min(bytes.len());
    let line_start = memchr::memrchr(b'\n', &bytes[..start]).map_or(0, |at| at + 1);
    let prefix = &bytes[line_start..start];
    let mut local = Local::default();
    match candidate.language {
        "rust" => {
            let attributes = rust::attributes(bytes, line_start, prefix);
            local.entry_attribute = attributes.iter().any(|path| rust::is_entry_attribute(path));
            local.effect_attribute = attributes
                .into_iter()
                .find(|path| !RUST_INERT_ATTRIBUTES.contains(&path.as_str()))
                .map(String::into_boxed_str);
        }
        "javascript" | "typescript" | "tsx" => {
            let mut words = prefix.split(|&b| !is_ident_byte(b)).filter(|word| !word.is_empty());
            local.exported = words.any(|word| word == b"export");
            local.default_export = local.exported && words.next() == Some(b"default".as_slice());
        }
        "python" => local.in_all = all.map(|all| all.contains(name)),
        _ => {}
    }
    local
}

/// What decides a candidate besides its own text.
pub(super) struct Judge<'a> {
    /// Whether a file belongs to a library surface whose exported names outside code may use.
    pub surface: &'a dyn Fn(&RelPath, &str) -> bool,
    /// Per candidate name: identifiers spelling it in code (its definitions taken out), in strings
    /// and in comments.
    pub others: &'a [[i64; 3]],
    /// The resolve pass has not covered the index, so a JS package's surface is not fully known.
    pub surface_partial: bool,
    /// JS: whether a file belongs to a package whose declared entry is a build output with no
    /// indexed source, so its published surface is not known.
    pub build_output: &'a dyn Fn(&RelPath) -> bool,
    /// Files the pass could not read: a use in one of them would be missed.
    pub unreadable: usize,
}

/// The tier and reason of one candidate, or `None` when it is used, exported or an entry point.
pub(super) fn judge(
    candidate: &Candidate,
    path: &RelPath,
    name: &str,
    local: &Local,
    facts: Option<&FileFacts>,
    j: &Judge<'_>,
) -> Option<DeadSymbol> {
    let js = matches!(candidate.language, "javascript" | "typescript" | "tsx");
    if local.container == Container::TraitImpl
        || local.default_export
        || local.entry_attribute
        || (js && local.exported && local.entry_file)
        || (candidate.language == "go" && facts.is_some_and(|f| f.go_exports.iter().any(|e| e == name)))
    {
        return None;
    }
    if exported(candidate, path, name, local, facts, j) {
        return None;
    }
    let [code, strings, comments] = j.others[candidate.name as usize];
    let (evidence, mut reason) = if code > 0 {
        return None;
    } else if strings > 0 {
        (
            Tier::Low,
            format!("its name is spelled only in {strings} string(s): a lookup by name may use it"),
        )
    } else if comments > 0 {
        (
            Tier::Medium,
            format!("its name is spelled only in {comments} comment(s), never in code"),
        )
    } else {
        (
            Tier::High,
            "no other identifier in the indexed code spells its name".to_string(),
        )
    };
    let mut tier = evidence;
    let mut caveats = caps(candidate, name, local);
    if js && local.exported {
        if j.surface_partial {
            caveats.push((
                Tier::Low,
                "exported, and the resolve pass has not covered the index, so whether the package publishes it is not known"
                    .to_string(),
            ));
        }
        if (j.build_output)(path) {
            caveats.push((
                Tier::Low,
                "exported by a package whose entry is a build output, so its published surface is not known"
                    .to_string(),
            ));
        }
    }
    if j.unreadable > 0 {
        caveats.push((
            Tier::Medium,
            format!(
                "{} file(s) could not be read, so a use in one would be missed",
                j.unreadable
            ),
        ));
    }
    for (cap, caveat) in caveats {
        tier = tier.min(cap);
        reason.push_str("; ");
        reason.push_str(&caveat);
    }
    Some(DeadSymbol {
        path: path.clone(),
        line: candidate.line,
        name: name.to_string(),
        kind: kind_to_str(candidate.kind),
        language: candidate.language.to_string(),
        tier,
        reason,
    })
}

/// Whether the language's export rule puts the candidate on a public surface.
fn exported(
    candidate: &Candidate,
    path: &RelPath,
    name: &str,
    local: &Local,
    facts: Option<&FileFacts>,
    j: &Judge<'_>,
) -> bool {
    match candidate.language {
        "rust" => candidate.flags & PUB != 0 && local.container == Container::None && (j.surface)(path, "rust"),
        "go" => {
            let upper = name.starts_with(|c: char| c.is_uppercase());
            let main = facts.and_then(|f| f.go_package.as_deref()) == Some("main");
            upper && !main && !path.as_bytes().ends_with(b"_test.go")
        }
        "javascript" | "typescript" | "tsx" => local.exported && (j.surface)(path, "js"),
        "java" => candidate.flags & JAVA_VISIBLE != 0,
        "python" => local.in_all == Some(true),
        _ => false,
    }
}

/// The caps a language puts on a candidate, each with its reason.
fn caps(candidate: &Candidate, name: &str, local: &Local) -> Vec<(Tier, String)> {
    let mut out = Vec::new();
    let method = matches!(
        candidate.kind,
        SymbolKind::Method | SymbolKind::Getter | SymbolKind::Setter
    );
    match candidate.language {
        "rust" => {
            if let Some(attribute) = &local.effect_attribute {
                out.push((Tier::Low, format!("attribute `#[{attribute}]` may generate a use")));
            }
            if local.container == Container::Trait {
                out.push((
                    Tier::Low,
                    "a trait method: implementations and dispatch may use it".to_string(),
                ));
            }
        }
        "go" if method => out.push((Tier::Low, "a method may satisfy an interface".to_string())),
        "javascript" | "typescript" | "tsx" => {
            if method {
                out.push((
                    Tier::Low,
                    "a method may be called through dispatch or a framework".to_string(),
                ));
            }
            if local.exported {
                out.push((
                    Tier::Medium,
                    "exported: an importer outside the repository would not be seen".to_string(),
                ));
            }
        }
        "python" => {
            if method {
                out.push((
                    Tier::Low,
                    "a method may be called through dispatch or a framework".to_string(),
                ));
            }
            if let Some(decorator) = &candidate.decorator {
                out.push((Tier::Low, format!("decorator `{decorator}` may register it")));
            }
            if !name.starts_with('_') && local.in_all.is_none() {
                out.push((
                    Tier::Low,
                    "a public module-level name may be imported from outside the repository".to_string(),
                ));
            }
            if local.entry_file && !method {
                out.push((
                    Tier::Low,
                    "in an entry file a loader or tool runs, which may call it by name".to_string(),
                ));
            }
        }
        "java" => {
            if method {
                out.push((Tier::Low, "a method may override or implement another".to_string()));
            }
            if candidate.flags & ANNOTATED != 0 {
                out.push((Tier::Low, "an annotation may register it".to_string()));
            }
        }
        _ => {}
    }
    out
}

#[cfg(test)]
pub(super) fn container_for_test(bytes: &[u8], start: usize) -> Container {
    rust::containers(bytes, &[start])[0]
}

/// The definitions [`text_pass`] takes out of the code count for `bytes` with outline `l1`.
#[cfg(test)]
pub(super) fn defined_for_test(language: &str, bytes: &[u8], l1: &FileMapL1, names: &Names) -> Vec<u32> {
    let mut hits = Vec::new();
    identifiers(language, bytes, |token, class, at| {
        if let Some(name) = names.get(token) {
            hits.push((u32::try_from(at).unwrap_or(u32::MAX), name, class));
        }
    });
    defined_names(l1, &hits, names)
}

/// The Rust module declarations [`text_pass`] reads from `bytes`.
#[cfg(test)]
pub(super) fn hidden_for_test(bytes: &[u8]) -> HiddenModules {
    let mut hidden = HiddenModules::default();
    let mut previous_mod = false;
    identifiers("rust", bytes, |token, class, at| {
        if class == Class::Code {
            rust::note_module_token(&mut hidden, previous_mod, bytes, token, at);
            previous_mod = token == b"mod";
        }
    });
    hidden.rs_strings = rust::rs_strings(bytes);
    hidden
}
