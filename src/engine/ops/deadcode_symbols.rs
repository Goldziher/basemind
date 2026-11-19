//! The unreferenced symbols of `basemind code deadcode` (#123), and the one text pass over the
//! indexed code that the whole report reads.
//!
//! Use is judged by name, so "unreferenced" is a lower bound on use (ADR-0017): a definition whose
//! name no other identifier in the indexed code spells is `high`; one whose name is spelled only in
//! comments is `medium`; one whose name is spelled only in strings is `low`, since a lookup by name
//! (serde, reflection, a registry) may use it. Exported names, entry points and trait-impl members
//! are never candidates; a language feature the index cannot see (dispatch, decorators, attributes,
//! outside importers) caps the tier and says why.

use ahash::{AHashMap, AHashSet};
use memchr::memmem;
use rayon::prelude::*;

use super::deadcode_files::FileFacts;
use super::deadcode_lex::{Class, identifiers, is_ident_byte};
use crate::engine::files::Sweep;
use crate::engine::freshness;
use crate::engine::l1_cache::FileMeta;
use crate::engine::types::code::kind_to_str;
use crate::engine::types::deadcode::{DeadSymbol, Tier};
use crate::extract::{FileMapL1, SymbolKind};
use crate::path::RelPath;

/// The languages whose definitions are judged, each with a rule for what it exports.
pub(super) const SYMBOL_LANGUAGES: &[&str] = &["rust", "go", "python", "javascript", "typescript", "tsx", "java"];

/// Rust attributes that make an item an entry point a harness or the linker calls.
const RUST_ENTRY_ATTRIBUTES: &[&str] = &[
    "test",
    "bench",
    "no_mangle",
    "export_name",
    "proc_macro",
    "proc_macro_derive",
    "proc_macro_attribute",
    "macro_export",
    "main",
    "tokio::main",
    "tokio::test",
    "rstest",
    "test_case",
    "wasm_bindgen",
    "napi",
    "pyfunction",
    "pyclass",
    "pymodule",
];

/// Rust attributes that generate no use of the item they mark.
const RUST_INERT_ATTRIBUTES: &[&str] = &[
    "derive",
    "doc",
    "allow",
    "warn",
    "deny",
    "expect",
    "must_use",
    "inline",
    "cold",
    "track_caller",
    "non_exhaustive",
    "repr",
    "cfg",
    "cfg_attr",
    "deprecated",
    "serde",
];

/// The Rust block a definition sits in, for the rules that depend on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Container {
    /// A module, an inherent `impl` or a function body.
    #[default]
    None,
    /// A `trait` body: implementations and dispatch use its methods.
    Trait,
    /// An `impl Trait for Type` body: the trait requires its members.
    TraitImpl,
}

/// A definition that may be unused.
#[derive(Debug, Clone)]
pub(super) struct Candidate {
    pub path: RelPath,
    pub language: Box<str>,
    pub name: String,
    pub kind: SymbolKind,
    pub start_byte: u32,
    pub line: u32,
    pub signature: Option<String>,
    pub decorator: Option<String>,
}

/// What the text around a candidate says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Local {
    /// Rust attribute paths directly above the item (`derive`, `tokio::test`).
    pub attributes: Vec<String>,
    /// JS/TS: `export` on the declaration.
    pub exported: bool,
    /// JS/TS: `export default`: an importer binds it by any name, so its own name says nothing.
    pub default_export: bool,
    /// Python: the file defines `__all__`, and whether it names the candidate.
    pub in_all: Option<bool>,
    /// Rust: the block the item sits in.
    pub container: Container,
}

/// One file of the text pass.
pub(super) struct ScanFile<'a> {
    pub path: &'a RelPath,
    pub meta: &'a FileMeta,
    /// Indices into the candidate list of the candidates defined in this file.
    pub candidates: Vec<usize>,
    /// Unresolved specifiers whose line to find.
    pub specifiers: &'a [String],
}

/// What the text pass read.
#[derive(Default)]
pub(super) struct TextScan {
    /// Per file read: the facts its entry conventions read.
    pub facts: AHashMap<RelPath, FileFacts>,
    /// Per candidate name (index into the name table): identifiers spelling it in code, in strings
    /// and in comments.
    pub occurrences: Vec<[u32; 3]>,
    /// Per candidate: the text around it, when its file was read fresh.
    pub locals: AHashMap<usize, Local>,
    /// Per file: the 1-based line of each unresolved specifier, in its order.
    pub specifier_lines: AHashMap<RelPath, Vec<Option<u32>>>,
}

/// The candidates of one file's outline: eligible kinds with an identifier name, minus the names a
/// harness or runtime calls by their spelling.
pub(super) fn candidates_of(path: &RelPath, language: &str, l1: &FileMapL1, out: &mut Vec<Candidate>) {
    let path_str = path.to_str_lossy();
    for symbol in &l1.symbols {
        if !eligible(symbol.kind)
            || !is_identifier(&symbol.name)
            || entry_name(&path_str, language, &symbol.name, symbol.kind)
        {
            continue;
        }
        out.push(Candidate {
            path: path.clone(),
            language: language.into(),
            name: symbol.name.clone(),
            kind: symbol.kind,
            start_byte: symbol.start_byte,
            line: symbol.start_row + 1,
            signature: symbol.signature.clone(),
            decorator: symbol.decorators.first().cloned(),
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

/// A Python file a test runner collects: every class in it may be a test case.
fn python_test_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.starts_with("test_") || name.ends_with("_test.py") || name == "tests.py"
}

/// A name a harness, runtime or tool calls by its spelling.
pub(super) fn entry_name(path: &str, language: &str, name: &str, kind: SymbolKind) -> bool {
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
                || UNITTEST_HOOKS.contains(&name)
                || (kind == SymbolKind::Class && python_test_file(path))
        }
        _ => false,
    }
}

/// Read every file once: count the identifiers spelling a candidate name by class, read the facts the entry
/// conventions need, the text around each candidate and the line of each unresolved specifier.
pub(super) fn text_pass(
    sweep: &Sweep<'_>,
    files: &[ScanFile<'_>],
    candidates: &[Candidate],
    names: &AHashMap<&str, u32>,
) -> TextScan {
    let max = sweep.max_file_bytes();
    let by_bytes: AHashMap<&[u8], u32> = names.iter().map(|(name, at)| (name.as_bytes(), *at)).collect();
    let per_file: Vec<FileScan> = files
        .par_iter()
        .filter_map(|file| {
            let disk = sweep.path(file.path)?;
            let live = freshness::load(&disk, file.meta.into(), max).ok()?;
            Some(scan_one(file, &live.bytes, live.fresh, candidates, &by_bytes))
        })
        .collect();
    let mut out = TextScan {
        occurrences: vec![[0; 3]; names.len()],
        ..TextScan::default()
    };
    for scan in per_file {
        for (name, counts) in scan.counts {
            for (total, count) in out.occurrences[name as usize].iter_mut().zip(counts) {
                *total += count;
            }
        }
        out.locals.extend(scan.locals);
        if let Some(lines) = scan.specifier_lines {
            out.specifier_lines.insert(scan.path.clone(), lines);
        }
        out.facts.insert(scan.path.clone(), scan.facts);
    }
    out
}

struct FileScan<'a> {
    path: &'a RelPath,
    facts: FileFacts,
    counts: Vec<(u32, [u32; 3])>,
    locals: Vec<(usize, Local)>,
    specifier_lines: Option<Vec<Option<u32>>>,
}

fn scan_one<'a>(
    file: &ScanFile<'a>,
    bytes: &[u8],
    fresh: bool,
    candidates: &[Candidate],
    names: &AHashMap<&[u8], u32>,
) -> FileScan<'a> {
    let language = &*file.meta.language;
    let mut counts: AHashMap<u32, [u32; 3]> = AHashMap::new();
    if !names.is_empty() {
        identifiers(language, bytes, |token, class: Class| {
            if let Some(name) = names.get(token) {
                counts.entry(*name).or_default()[class as usize] += 1;
            }
        });
    }
    let facts = facts_of(language, bytes);
    let locals = if fresh {
        let has_all = language == "python" && memmem::find(bytes, b"__all__").is_some();
        file.candidates
            .iter()
            .map(|&at| (at, local_of(&candidates[at], bytes, has_all)))
            .collect()
    } else {
        Vec::new()
    };
    let specifier_lines = (!file.specifiers.is_empty()).then(|| {
        file.specifiers
            .iter()
            .map(|specifier| {
                let found = memmem::find(bytes, specifier.as_bytes()).filter(|_| fresh)?;
                u32::try_from(memchr::memchr_iter(b'\n', &bytes[..found]).count() + 1).ok()
            })
            .collect()
    });
    FileScan {
        path: file.path,
        facts,
        counts: counts.into_iter().collect(),
        locals,
        specifier_lines,
    }
}

/// The facts of a file that its language's entry conventions read.
pub(super) fn facts_of(language: &str, bytes: &[u8]) -> FileFacts {
    let mut facts = FileFacts::default();
    match language {
        "go" => {
            facts.go_package = bytes
                .split(|&b| b == b'\n')
                .map(|line| String::from_utf8_lossy(line).trim().to_string())
                .find_map(|line| {
                    line.strip_prefix("package ")
                        .and_then(|rest| rest.split_whitespace().next().map(str::to_string))
                });
        }
        "python" => {
            facts.main_guard = memmem::find(bytes, b"__name__ == \"__main__\"").is_some()
                || memmem::find(bytes, b"__name__ == '__main__'").is_some();
        }
        "java" => facts.java_main = memmem::find(bytes, b"static void main(").is_some(),
        _ => {}
    }
    facts
}

/// The text around one candidate in its file's fresh bytes.
pub(super) fn local_of(candidate: &Candidate, bytes: &[u8], has_all: bool) -> Local {
    let start = (candidate.start_byte as usize).min(bytes.len());
    let line_start = memchr::memrchr(b'\n', &bytes[..start]).map_or(0, |at| at + 1);
    let prefix = &bytes[line_start..start];
    let mut local = Local::default();
    match &*candidate.language {
        "rust" => {
            local.attributes = rust_attributes(bytes, line_start, prefix);
            local.container = rust_container(bytes, start);
        }
        "javascript" | "typescript" | "tsx" => {
            let mut words = prefix.split(|&b| !is_ident_byte(b)).filter(|word| !word.is_empty());
            local.exported = words.any(|word| word == b"export");
            local.default_export = local.exported && words.next() == Some(b"default".as_slice());
        }
        "python" if has_all => {
            let quoted = |q: &str| format!("{q}{}{q}", candidate.name);
            local.in_all = Some(
                memmem::find(bytes, quoted("\"").as_bytes()).is_some()
                    || memmem::find(bytes, quoted("'").as_bytes()).is_some(),
            );
        }
        _ => {}
    }
    local
}

/// The attribute paths on the lines directly above `line_start` (and before the item on its own
/// line), skipping comments; a multi-line attribute is read from its opening line.
fn rust_attributes(bytes: &[u8], line_start: usize, prefix: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    collect_attributes(prefix, &mut out);
    let mut end = line_start;
    let mut open_tail = false;
    for _ in 0..40 {
        if end == 0 {
            break;
        }
        let begin = memchr::memrchr(b'\n', &bytes[..end - 1]).map_or(0, |at| at + 1);
        let line = String::from_utf8_lossy(&bytes[begin..end - 1]);
        let line = line.trim();
        end = begin;
        if line.starts_with("#[") {
            collect_attributes(line.as_bytes(), &mut out);
            open_tail = false;
            continue;
        }
        if line.starts_with("//") {
            continue;
        }
        if line.is_empty() || !(open_tail || line.ends_with(']') || line.ends_with(',') || line.ends_with(')')) {
            break;
        }
        open_tail = true;
    }
    out
}

/// Bytes [`rust_container`] walks back at most.
const CONTAINER_SCAN_BYTES: usize = 256 * 1024;

/// The block around byte `start`, found by walking back to the unmatched `{` and reading the header
/// before it. Braces inside strings or comments can mislead it; the outline holds no impl nested in a
/// function, so the text is read instead.
fn rust_container(bytes: &[u8], start: usize) -> Container {
    let floor = start.saturating_sub(CONTAINER_SCAN_BYTES);
    let mut depth = 0usize;
    let mut at = start;
    let open = loop {
        if at <= floor {
            return Container::None;
        }
        at -= 1;
        match bytes[at] {
            b'}' => depth += 1,
            b'{' if depth == 0 => break at,
            b'{' => depth -= 1,
            _ => {}
        }
    };
    let header_start = bytes[..open]
        .iter()
        .rposition(|&b| matches!(b, b';' | b'}' | b'{'))
        .map_or(0, |at| at + 1);
    let header = String::from_utf8_lossy(&bytes[header_start..open]);
    let words: Vec<&str> = header
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//") && !line.starts_with("#["))
        .flat_map(str::split_whitespace)
        .collect();
    let keyword = words
        .iter()
        .position(|word| *word == "impl" || word.starts_with("impl<") || *word == "trait");
    match keyword.map(|at| (words[at], &words[at..])) {
        Some(("trait", _)) => Container::Trait,
        Some((_, rest)) if rest.contains(&"for") => Container::TraitImpl,
        _ => Container::None,
    }
}

/// The path of each `#[path…]` in `text`.
fn collect_attributes(text: &[u8], out: &mut Vec<String>) {
    let mut from = 0;
    while let Some(at) = memmem::find(&text[from..], b"#[") {
        let start = from + at + 2;
        let end = text[start..]
            .iter()
            .position(|&b| !(is_ident_byte(b) || b == b':'))
            .map_or(text.len(), |len| start + len);
        if end > start {
            out.push(String::from_utf8_lossy(&text[start..end]).into_owned());
        }
        from = end.max(start);
    }
}

/// What decides a candidate besides its own text.
pub(super) struct Judge<'a> {
    /// Whether a file belongs to a library surface whose exported names outside code may use.
    pub surface: &'a dyn Fn(&RelPath, &str) -> bool,
    /// Per candidate name: identifiers spelling it in code (its definitions taken out), in strings
    /// and in comments.
    pub others: &'a [[i64; 3]],
}

/// The tier and reason of one candidate, or `None` when it is used, exported or an entry point.
pub(super) fn judge(
    candidate: &Candidate,
    local: &Local,
    facts: Option<&FileFacts>,
    name: u32,
    j: &Judge<'_>,
) -> Option<DeadSymbol> {
    if local.container == Container::TraitImpl
        || local.default_export
        || local
            .attributes
            .iter()
            .any(|attribute| RUST_ENTRY_ATTRIBUTES.contains(&attribute.as_str()))
    {
        return None;
    }
    if exported(candidate, local, facts, j) {
        return None;
    }
    let [code, strings, comments] = j.others[name as usize];
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
    for (cap, caveat) in caps(candidate, local) {
        tier = tier.min(cap);
        reason.push_str("; ");
        reason.push_str(&caveat);
    }
    Some(DeadSymbol {
        path: candidate.path.clone(),
        line: candidate.line,
        name: candidate.name.clone(),
        kind: kind_to_str(candidate.kind),
        language: candidate.language.to_string(),
        tier,
        reason,
    })
}

/// Whether the language's export rule puts the candidate on a public surface.
fn exported(candidate: &Candidate, local: &Local, facts: Option<&FileFacts>, j: &Judge<'_>) -> bool {
    let signature = candidate.signature.as_deref().unwrap_or("");
    let words = || signature.split(|c: char| !(c.is_alphanumeric() || c == '_'));
    match &*candidate.language {
        "rust" => {
            let public = signature.starts_with("pub ") || signature.starts_with("pub\t");
            public && local.container == Container::None && (j.surface)(&candidate.path, "rust")
        }
        "go" => {
            let upper = candidate.name.starts_with(|c: char| c.is_uppercase());
            let main = facts.and_then(|f| f.go_package.as_deref()) == Some("main");
            upper && !main && !candidate.path.as_bytes().ends_with(b"_test.go")
        }
        "javascript" | "typescript" | "tsx" => local.exported && (j.surface)(&candidate.path, "js"),
        "java" => words().any(|word| word == "public" || word == "protected"),
        "python" => local.in_all == Some(true),
        _ => false,
    }
}

/// The caps a language puts on a candidate, each with its reason.
fn caps(candidate: &Candidate, local: &Local) -> Vec<(Tier, String)> {
    let mut out = Vec::new();
    let method = matches!(
        candidate.kind,
        SymbolKind::Method | SymbolKind::Getter | SymbolKind::Setter
    );
    match &*candidate.language {
        "rust" => {
            if let Some(attribute) = local
                .attributes
                .iter()
                .find(|attribute| !RUST_INERT_ATTRIBUTES.contains(&attribute.as_str()))
            {
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
            if !candidate.name.starts_with('_') && local.in_all.is_none() {
                out.push((
                    Tier::Low,
                    "a public module-level name may be imported from outside the repository".to_string(),
                ));
            }
        }
        "java" => {
            if method {
                out.push((Tier::Low, "a method may override or implement another".to_string()));
            }
            if candidate.signature.as_deref().is_some_and(|s| s.contains('@')) {
                out.push((Tier::Low, "an annotation may register it".to_string()));
            }
        }
        _ => {}
    }
    out
}

/// Definitions per candidate name across the files the text pass read: each spells its own name
/// once, so they are not uses.
pub(super) fn definitions(
    cache: &crate::engine::map_cache::MapCache,
    read: &AHashSet<&RelPath>,
    names: &AHashMap<&str, u32>,
) -> Vec<u32> {
    let mut out = vec![0u32; names.len()];
    cache.for_each(|path, l1| {
        if !read.contains(path) {
            return;
        }
        for symbol in &l1.symbols {
            if symbol.kind == SymbolKind::Impl {
                continue;
            }
            if let Some(&at) = names.get(symbol.name.as_str()) {
                out[at as usize] += 1;
            }
        }
    });
    out
}

#[cfg(test)]
pub(super) fn attributes_for_test(bytes: &[u8], start: usize) -> Vec<String> {
    let line_start = memchr::memrchr(b'\n', &bytes[..start]).map_or(0, |at| at + 1);
    rust_attributes(bytes, line_start, &bytes[line_start..start])
}

#[cfg(test)]
pub(super) fn container_for_test(bytes: &[u8], start: usize) -> Container {
    rust_container(bytes, start)
}
