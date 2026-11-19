//! Which definition a `code explain` symbol target names, and the doc comment above it.

use crate::engine::error::EngineError;
use crate::engine::freshness;
use crate::engine::pick::line_span;
use crate::engine::types::code::kind_to_str;
use crate::engine::workspace::Workspace;
use crate::extract::{Symbol, SymbolKind};
use crate::path::RelPath;

use super::{Loaded, load};

/// Definitions of one name a symbol target considers; past it the name is refused as too common.
const DEFINITIONS_MAX: usize = 256;
/// Definitions an ambiguity error lists.
const CANDIDATES_LISTED: usize = 20;
/// Bytes of a doc comment listed.
const DOC_BYTES_MAX: usize = 1_000;

/// The definition a symbol target names, and the other definitions of its name.
pub(super) struct Chosen {
    pub(super) path: RelPath,
    pub(super) loaded: Loaded,
    /// Index of the definition in `loaded.l1.symbols`.
    pub(super) index: usize,
    /// The other definitions of the name, as `path:line`.
    pub(super) also_defined: Vec<String>,
    /// The name the target looked up, for the `also_defined` continuation.
    pub(super) looked_up: String,
    /// Definitions left out because their file could not be read.
    pub(super) notes: Vec<String>,
}

/// A definition of the looked-up name, as the index holds it.
struct Candidate {
    path: RelPath,
    kind: SymbolKind,
    start_byte: u32,
    /// 1-based line of the indexed definition.
    line: u32,
}

impl Candidate {
    fn at(&self) -> String {
        format!("{}:{}", self.path, self.line)
    }
}

pub(super) fn callable(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor
    )
}

fn not_ready(ws: &Workspace) -> Option<EngineError> {
    super::super::graph_report::index_not_ready(ws, "code explain")
}

/// The innermost definition of `path` whose lines hold `line`.
pub(super) fn symbol_at(ws: &Workspace, path: RelPath, line: u32) -> Result<Chosen, EngineError> {
    let loaded = load(ws, &path)?;
    let symbols = &loaded.l1.symbols;
    let holding = symbols
        .iter()
        .enumerate()
        .filter(|(_, symbol)| {
            let (first, last) = line_span(symbol, Some(&loaded.bytes));
            (first..=last).contains(&line)
        })
        .min_by_key(|(_, symbol)| symbol.end_byte.saturating_sub(symbol.start_byte))
        .map(|(index, _)| index);
    let Some(index) = holding else {
        return Err(EngineError::invalid_argument(format!(
            "`code explain`: no definition of `{path}` holds line {line}; pass `{path}` alone for a briefing on \
             the file"
        )));
    };
    let block = &symbols[index];
    if block.kind == SymbolKind::Impl {
        let members: Vec<String> = symbols
            .iter()
            .filter(|inner| encloses(block, inner))
            .take(CANDIDATES_LISTED)
            .map(|inner| format!("`{}` line {}", inner.name, inner.start_row + 1))
            .collect();
        return Err(EngineError::invalid_argument(format!(
            "`code explain`: line {line} of `{path}` is inside `impl {}` but in none of its definitions ({}); pass \
             the line of one of them, or the type's name",
            block.name,
            if members.is_empty() {
                "it has none".to_string()
            } else {
                members.join(", ")
            }
        )));
    }
    Ok(Chosen {
        looked_up: block.name.clone(),
        path,
        loaded,
        index,
        also_defined: Vec::new(),
        notes: Vec::new(),
    })
}

/// `name` split into an optional qualifier (`Type` of `Type::name` or `Type.name`, its last
/// segment) and the bare name.
fn split_qualified(name: &str) -> (Option<&str>, &str) {
    match name.rsplit_once("::").or_else(|| name.rsplit_once('.')) {
        Some((qualifier, bare)) if !bare.is_empty() && !qualifier.is_empty() => {
            (qualifier.rsplit("::").next().and_then(|q| q.rsplit('.').next()), bare)
        }
        _ => (None, name),
    }
}

/// The definition `name` names: the only one, or the only callable one (the others listed in
/// `also_defined`). `Type::name` or `Type.name` narrows the definitions of `name` to those inside a
/// definition of `Type`. Candidates come from the index alone; only the chosen file is read, and a
/// chosen file that cannot be read is left out (with a note) for the next choice.
pub(super) fn symbol_named(ws: &Workspace, name: &str) -> Result<Chosen, EngineError> {
    let (qualifier, bare) = split_qualified(name);
    let (mut candidates, capped) = candidates(ws, bare, qualifier)?;
    let mut notes = Vec::new();
    loop {
        if candidates.is_empty() {
            if let Some(refusal) = not_ready(ws) {
                return Err(refusal);
            }
            return Err(EngineError::invalid_argument(format!(
                "`code explain`: no definition named `{name}` is indexed; find the exact name with `basemind code \
                 symbols {bare} --fuzzy`, or pass a file path{}",
                if notes.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", notes.join("; "))
                }
            )));
        }
        let chosen = choose(&candidates, name, capped)?;
        let candidate = &candidates[chosen];
        match load(ws, &candidate.path).and_then(|loaded| locate(loaded, candidate, bare, qualifier)) {
            Ok((loaded, index)) => {
                let candidate = candidates.swap_remove(chosen);
                candidates.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
                if capped {
                    notes.push(format!(
                        "`{bare}` has more than {DEFINITIONS_MAX} definitions; `also_defined` lists some of them"
                    ));
                }
                return Ok(Chosen {
                    path: candidate.path,
                    loaded,
                    index,
                    also_defined: candidates.iter().map(Candidate::at).collect(),
                    looked_up: bare.to_string(),
                    notes,
                });
            }
            Err(error) => {
                let skipped = candidates.swap_remove(chosen);
                notes.push(format!("skipped `{}`: {}", skipped.at(), error));
            }
        }
    }
}

/// The indexed definitions of `bare` (inside a definition named `qualifier`, when given), in path
/// and line order, and whether the lookup hit its cap.
fn candidates(ws: &Workspace, bare: &str, qualifier: Option<&str>) -> Result<(Vec<Candidate>, bool), EngineError> {
    let store = ws.store_read();
    let Some(view) = store.read_index_view() else {
        drop(store);
        return Err(not_ready(ws).unwrap_or_else(|| {
            EngineError::busy(
                "`code explain`: the symbol lookup needs the index, which another basemind process holds: retry",
            )
        }));
    };
    let rows = view.symbols_by_name_lookup(bare, DEFINITIONS_MAX + 1);
    let capped = rows.len() > DEFINITIONS_MAX;
    let cache = ws.cache.load_full();
    let mut out: Vec<Candidate> = rows
        .into_iter()
        .take(DEFINITIONS_MAX)
        .filter(|(found, _, path, _)| found == bare && store.index.files.contains_key(path))
        .filter_map(|(_, kind, path, start_byte)| {
            let (_, l1) = freshness::indexed_outline(&store, &cache, &path)?;
            let symbol = l1
                .symbols
                .iter()
                .find(|symbol| symbol.start_byte == start_byte && symbol.name == bare)?;
            if qualifier.is_some_and(|q| !inside_named(&l1.symbols, symbol, q)) {
                return None;
            }
            Some(Candidate {
                line: symbol.start_row + 1,
                path,
                kind,
                start_byte,
            })
        })
        .collect();
    out.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    out.dedup_by(|a, b| a.path == b.path && a.start_byte == b.start_byte);
    Ok((out, capped))
}

/// The index of the candidate to brief: the only one, or the only callable one. Refused, listing
/// them, when that does not settle it.
fn choose(candidates: &[Candidate], name: &str, capped: bool) -> Result<usize, EngineError> {
    if candidates.len() == 1 && !capped {
        return Ok(0);
    }
    let mut callables = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| callable(candidate.kind));
    if !capped && let (Some((only, _)), None) = (callables.next(), callables.next()) {
        return Ok(only);
    }
    let listed: Vec<String> = candidates
        .iter()
        .take(CANDIDATES_LISTED)
        .map(|candidate| format!("{} ({})", candidate.at(), kind_to_str(candidate.kind)))
        .collect();
    let count = if capped {
        format!("more than {DEFINITIONS_MAX}")
    } else {
        candidates.len().to_string()
    };
    let more = candidates.len().saturating_sub(CANDIDATES_LISTED);
    let more = match (more, capped) {
        (0, false) => String::new(),
        (_, true) => ", and more".to_string(),
        (more, false) => format!(", and {more} more"),
    };
    Err(EngineError::invalid_argument(format!(
        "`code explain`: `{name}` names {count} definitions: {}{more}. Pass the one you mean as `path:line`.",
        listed.join(", ")
    )))
}

/// The definition `candidate` names in `loaded`: the same bytes when the file is unchanged, else
/// the definition of the name nearest its indexed line.
fn locate(
    loaded: Loaded,
    candidate: &Candidate,
    bare: &str,
    qualifier: Option<&str>,
) -> Result<(Loaded, usize), EngineError> {
    let symbols = &loaded.l1.symbols;
    let named = |symbol: &Symbol| symbol.name == bare && qualifier.is_none_or(|q| inside_named(symbols, symbol, q));
    let index = if loaded.stale {
        symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| named(symbol))
            .min_by_key(|(_, symbol)| (symbol.start_row + 1).abs_diff(candidate.line))
            .map(|(index, _)| index)
    } else {
        symbols
            .iter()
            .position(|symbol| symbol.start_byte == candidate.start_byte && named(symbol))
    };
    match index {
        Some(index) => Ok((loaded, index)),
        None => Err(EngineError::invalid_argument(format!(
            "`{}` no longer defines `{bare}` since the scan",
            candidate.path
        ))),
    }
}

/// Whether `inner` lies strictly inside `outer`.
fn encloses(outer: &Symbol, inner: &Symbol) -> bool {
    outer.start_byte <= inner.start_byte
        && inner.end_byte <= outer.end_byte
        && (outer.start_byte, outer.end_byte) != (inner.start_byte, inner.end_byte)
}

/// Whether `symbol` lies inside a definition of `symbols` named `qualifier`. An `impl` block's name
/// may be `Trait for Type` or `Type<T>`, so its last word with any generic arguments dropped counts.
fn inside_named(symbols: &[Symbol], symbol: &Symbol, qualifier: &str) -> bool {
    symbols.iter().any(|outer| {
        let last = outer.name.rsplit(' ').next().unwrap_or_default();
        let bare = last.split('<').next().unwrap_or_default();
        (outer.name == qualifier || last == qualifier || bare == qualifier) && encloses(outer, symbol)
    })
}

/// Whether `gap` holds only whitespace and attributes or decorators (`#[...]` spanning lines,
/// `@name(...)`), so a doc comment above them still documents the definition below.
fn only_attributes(gap: &[u8]) -> bool {
    let mut at = 0;
    while at < gap.len() {
        let byte = gap[at];
        if byte.is_ascii_whitespace() {
            at += 1;
            continue;
        }
        let rust = gap[at..].starts_with(b"#[");
        if !rust && byte != b'@' {
            return false;
        }
        let (open, close) = if rust { (b'[', b']') } else { (b'(', b')') };
        let (mut depth, mut quoted, mut end) = (0i32, false, at + 1);
        while end < gap.len() {
            let c = gap[end];
            end += 1;
            if quoted {
                if c == b'\\' {
                    end += 1;
                } else if c == b'"' {
                    quoted = false;
                }
            } else if c == b'"' {
                quoted = true;
            } else if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if rust && depth == 0 {
                    break;
                }
            } else if c == b'\n' && !rust && depth <= 0 {
                break;
            }
        }
        if rust && depth != 0 {
            return false;
        }
        at = end;
    }
    true
}

/// The doc comment ending just above `symbol`, with only whitespace, attributes or decorators
/// between them; and the 1-based line it starts on.
pub(super) fn doc_above(docs: &[crate::extract::DocComment], symbol: &Symbol, source: &[u8]) -> Option<(String, u32)> {
    let start = symbol.start_byte as usize;
    let doc = docs
        .iter()
        .filter(|doc| doc.end_byte as usize <= start)
        .max_by_key(|doc| doc.end_byte)?;
    let gap = source.get(doc.end_byte as usize..start)?;
    if !only_attributes(gap) {
        return None;
    }
    let text = doc.text.trim();
    let mut cut = text.len().min(DOC_BYTES_MAX);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let line = source
        .get(..doc.start_byte as usize)
        .map_or(0, |before| memchr::memchr_iter(b'\n', before).count()) as u32
        + 1;
    Some((text[..cut].to_string(), line))
}
