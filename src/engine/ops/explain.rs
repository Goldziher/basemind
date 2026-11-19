//! `basemind code explain` (#120): one briefing on a symbol or a file, composed from what the other
//! operations already answer, with no index data of its own. A symbol briefing holds the definition
//! (signature, doc, line span, and the body when it is short), its callers with their resolution
//! grades (`code_refs` callers), the names its body calls, the files importing its file, and the
//! commits that changed its body. A file briefing holds the outline, the files it imports and that
//! import it, its `git hotspots` row, its recent commits and their authors.
//!
//! Every list section is cut at `limit` rows, and the whole answer at a token budget; each cut
//! section is listed in `truncated_sections` with the tool call that returns the rest.

use ahash::{AHashMap, AHashSet};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::engine::budget::budget;
use crate::engine::error::EngineError;
use crate::engine::files::{indexed_file, not_indexed_in};
use crate::engine::freshness;
use crate::engine::import_graph::{Direction, WalkLimits, import_graph};
use crate::engine::pick::{is_nested, line_span};
use crate::engine::timing::elapsed_us;
use crate::engine::tokens::bytes_to_tokens;
use crate::engine::types::code::kind_to_str;
use crate::engine::types::explain::{
    ExplainCommit, ExplainDefinition, ExplainOutlineRow, ExplainOwner, ExplainParams, ExplainResponse, TruncatedSection,
};
use crate::engine::types::git::GitLogResult;
use crate::engine::types::refs::CodeRefsResult;
use crate::engine::workspace::Workspace;
use crate::extract::{FileMapL1, Symbol, SymbolKind};
use crate::path::RelPath;

use super::graph_report::{notes as graph_notes, tiers};

/// Rows per list section when the caller passes no `limit`, and the most it may ask for.
const LIMIT_DEFAULT: u32 = 20;
const LIMIT_MAX: u32 = 200;
/// A symbol's body is listed when it spans at most this many lines.
const BODY_LINES_MAX: u32 = 60;
/// Commits a briefing's history lists.
const HISTORY_MAX: u32 = 5;
/// Commits of a file whose authors are its owners.
const OWNER_COMMITS: u32 = 20;
/// Definitions of one name a symbol target reads.
const DEFINITIONS_MAX: usize = 256;
/// Definitions an ambiguity error lists.
const CANDIDATES_LISTED: usize = 20;
/// Bytes of a doc comment listed.
const DOC_BYTES_MAX: usize = 1_000;
/// Hotspot rows read to find the file's.
const HOTSPOT_ROWS: u32 = 200;
/// Rows `code_refs` returns at most, for a continuation call.
const REFS_LIMIT_MAX: usize = 1_000;

const SYMBOL_SECTIONS: &[&str] = &["body", "callers", "callees", "importers", "history"];
const FILE_SECTIONS: &[&str] = &["outline", "imports", "importers", "hotspot", "history", "owners"];

/// What a target names.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    File(RelPath),
    /// The innermost definition holding a 1-based line of a file.
    At(RelPath, u32),
    Name(String),
}

/// An indexed file as it is now: its outline and bytes, re-extracted when it changed since the scan.
struct Loaded {
    l1: std::sync::Arc<FileMapL1>,
    bytes: Vec<u8>,
    stale: bool,
    language: String,
}

/// The definition a symbol target names, and the other definitions of its name.
struct Chosen {
    path: RelPath,
    loaded: Loaded,
    /// Index of the definition in `loaded.l1.symbols`.
    index: usize,
    also_defined: Vec<String>,
}

/// What the caller asked of every section.
struct Asked<'a> {
    limit: usize,
    code: bool,
    want: &'a dyn Fn(&str) -> bool,
}

/// The answer being built, and the call that continues each section.
struct Briefing {
    response: ExplainResponse,
    next: AHashMap<&'static str, String>,
}

impl Workspace {
    /// `basemind code explain`: a briefing on a symbol, a file, or the definition at `path:line`.
    pub(crate) fn code_explain(&self, p: ExplainParams) -> Result<ExplainResponse, EngineError> {
        let started = std::time::Instant::now();
        let limit = p.limit.unwrap_or(LIMIT_DEFAULT);
        if limit == 0 || limit > LIMIT_MAX {
            return Err(shape(&format!("`limit` is 1 to {LIMIT_MAX}")));
        }
        let target = classify(self, &p.target)?;
        let file = matches!(target, Target::File(_));
        let wanted = sections(
            p.sections.as_deref(),
            if file { FILE_SECTIONS } else { SYMBOL_SECTIONS },
        )?;
        let want = |section: &str| wanted.contains(&section);
        let asked = Asked {
            limit: limit as usize,
            code: !p.no_code,
            want: &want,
        };
        let mut briefing = match target {
            Target::File(path) => file_briefing(self, path, &asked)?,
            Target::At(path, line) => symbol_briefing(self, symbol_at(self, path, line)?, &asked)?,
            Target::Name(name) => symbol_briefing(self, symbol_named(self, &name)?, &asked)?,
        };
        fit(&mut briefing, budget(p.max_tokens).map_or(u64::MAX, u64::from));
        let mut response = briefing.response;
        response.notice = self.lifecycle_notice();
        response.elapsed_us = elapsed_us(started);
        Ok(response)
    }
}

fn shape(problem: &str) -> EngineError {
    EngineError::invalid_argument(format!(
        "`code explain`: {problem}; pass a symbol name, an indexed file, or `path:line`, with optional \
         `sections`, `limit` (1 to {LIMIT_MAX}) and `no_code`"
    ))
}

/// The sections `asked` names (comma-separated), every one of `known` when absent.
fn sections(asked: Option<&str>, known: &'static [&'static str]) -> Result<Vec<&'static str>, EngineError> {
    let Some(asked) = asked else {
        return Ok(known.to_vec());
    };
    let mut out = Vec::new();
    for name in asked.split(',').map(str::trim).filter(|name| !name.is_empty()) {
        let section = known.iter().find(|known| **known == name).ok_or_else(|| {
            shape(&format!(
                "this target has no section `{name}`; its sections are {}",
                known.join(", ")
            ))
        })?;
        out.push(*section);
    }
    Ok(out)
}

/// Whether `target` is a file, a definition at `path:line`, or a symbol name.
fn classify(ws: &Workspace, target: &str) -> Result<Target, EngineError> {
    let target = target.trim();
    if target.is_empty() {
        return Err(shape("`target` is empty"));
    }
    let path = target.strip_prefix("./").unwrap_or(target);
    let store = ws.store_read();
    let indexed = |path: &str| store.lookup(path.as_bytes()).is_some();
    if let Some((file, line)) = path.rsplit_once(':')
        && !line.is_empty()
        && line.bytes().all(|b| b.is_ascii_digit())
    {
        let line: u32 = line
            .parse()
            .map_err(|_| shape(&format!("line `{line}` is out of range")))?;
        if line == 0 {
            return Err(shape("lines are 1-based"));
        }
        if indexed(file) {
            return Ok(Target::At(RelPath::from(file), line));
        }
        return Err(not_indexed_in(&store, &ws.root, "code explain", &RelPath::from(file)));
    }
    if indexed(path) {
        return Ok(Target::File(RelPath::from(path)));
    }
    if path.contains('/') {
        return Err(not_indexed_in(&store, &ws.root, "code explain", &RelPath::from(path)));
    }
    Ok(Target::Name(target.to_string()))
}

/// `path` as it is now.
fn load(ws: &Workspace, path: &RelPath) -> Result<Loaded, EngineError> {
    let (indexed, entry, file) = {
        let store = ws.store_read();
        let not_indexed = || not_indexed_in(&store, &ws.root, "code explain", path);
        let l1 = crate::query::file_outline(&store, path).map_err(|_| not_indexed())?;
        let entry = store.lookup(path).cloned().ok_or_else(not_indexed)?;
        (l1, entry, indexed_file(ws, &store, "code explain", path)?)
    };
    let max_bytes = ws.config.scan.max_file_bytes;
    let live = freshness::load(&file, (&entry).into(), max_bytes)
        .map_err(|reason| freshness::unloadable("code explain", path, reason, max_bytes))?;
    let l1 = if live.fresh {
        std::sync::Arc::new(indexed)
    } else {
        freshness::reparse(ws, "code explain", path, &live.bytes)?
    };
    Ok(Loaded {
        l1,
        bytes: live.bytes,
        stale: !live.fresh,
        language: entry.language,
    })
}

/// The calls and doc comments of `loaded`, extracted from its current bytes when it is stale.
fn calls_and_docs(ws: &Workspace, path: &RelPath, loaded: &Loaded) -> Option<crate::extract::FileMapL2> {
    if !loaded.stale {
        let store = ws.store_read();
        if let Ok(l2) = crate::query::file_outline_l2(&store, path, &ws.root) {
            return Some(l2);
        }
    }
    let lang = crate::lang::intern(&loaded.language)?;
    crate::extract::l2::extract_l2(lang, &loaded.bytes).ok()
}

/// The innermost definition of `path` whose lines hold `line`.
fn symbol_at(ws: &Workspace, path: RelPath, line: u32) -> Result<Chosen, EngineError> {
    let loaded = load(ws, &path)?;
    let holding = loaded
        .l1
        .symbols
        .iter()
        .enumerate()
        .filter(|(_, symbol)| {
            let (first, last) = line_span(symbol, Some(&loaded.bytes));
            (first..=last).contains(&line)
        })
        .min_by_key(|(_, symbol)| symbol.end_byte.saturating_sub(symbol.start_byte))
        .map(|(index, _)| index);
    match holding {
        Some(index) => Ok(Chosen {
            path,
            loaded,
            index,
            also_defined: Vec::new(),
        }),
        None => Err(EngineError::invalid_argument(format!(
            "`code explain`: no definition of `{path}` holds line {line}; pass `{path}` alone for a briefing on \
             the file"
        ))),
    }
}

fn callable(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor
    )
}

/// The definition `name` names: the only one, or the only callable one (the others listed as
/// `path:line`). `Type::name` or `Type.name` narrows the definitions of `name` to those inside a
/// definition of `Type`.
fn symbol_named(ws: &Workspace, name: &str) -> Result<Chosen, EngineError> {
    let (qualifier, bare) = match name.rsplit_once("::").or_else(|| name.rsplit_once('.')) {
        Some((qualifier, bare)) if !bare.is_empty() && !qualifier.is_empty() => {
            (qualifier.rsplit("::").next().and_then(|q| q.rsplit('.').next()), bare)
        }
        _ => (None, name),
    };
    let mut paths: Vec<RelPath> = {
        let store = ws.store_read();
        let view = store.read_index_view().ok_or_else(|| {
            EngineError::busy("the symbol lookup needs the index, which another basemind process holds: retry")
        })?;
        view.symbols_by_name_lookup(bare, DEFINITIONS_MAX)
            .into_iter()
            .filter(|(found, _, path, _)| found == bare && store.index.files.contains_key(path))
            .map(|(_, _, path, _)| path)
            .collect()
    };
    paths.sort_unstable();
    paths.dedup();
    let mut candidates: Vec<(RelPath, std::sync::Arc<Loaded>, usize)> = Vec::new();
    for path in paths {
        let loaded = std::sync::Arc::new(load(ws, &path)?);
        for (index, symbol) in loaded.l1.symbols.iter().enumerate() {
            if symbol.name == bare && qualifier.is_none_or(|q| inside_named(&loaded.l1.symbols, symbol, q)) {
                candidates.push((path.clone(), loaded.clone(), index));
            }
        }
    }
    let at = |(path, loaded, index): &(RelPath, std::sync::Arc<Loaded>, usize)| {
        let symbol = &loaded.l1.symbols[*index];
        format!("{path}:{}", line_span(symbol, Some(&loaded.bytes)).0)
    };
    let chosen = match candidates.len() {
        0 => {
            return Err(EngineError::invalid_argument(format!(
                "`code explain`: no definition named `{name}` is indexed; find the exact name with `basemind code \
                 symbols {bare} --fuzzy`, or pass a file path"
            )));
        }
        1 => 0,
        _ => {
            let mut callables = candidates
                .iter()
                .enumerate()
                .filter(|(_, (_, loaded, index))| callable(loaded.l1.symbols[*index].kind));
            match (callables.next(), callables.next()) {
                (Some((only, _)), None) => only,
                _ => {
                    let listed: Vec<String> = candidates
                        .iter()
                        .take(CANDIDATES_LISTED)
                        .map(|candidate| {
                            format!(
                                "{} ({})",
                                at(candidate),
                                kind_to_str(candidate.1.l1.symbols[candidate.2].kind)
                            )
                        })
                        .collect();
                    let more = candidates.len().saturating_sub(CANDIDATES_LISTED);
                    let more = if more > 0 {
                        format!(", and {more} more")
                    } else {
                        String::new()
                    };
                    return Err(EngineError::invalid_argument(format!(
                        "`code explain`: `{name}` names {} definitions: {}{more}. Pass the one you mean as `path:line`.",
                        candidates.len(),
                        listed.join(", ")
                    )));
                }
            }
        }
    };
    let also_defined: Vec<String> = candidates
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != chosen)
        .map(|(_, candidate)| at(candidate))
        .collect();
    let (path, loaded, index) = candidates.swap_remove(chosen);
    drop(candidates);
    let loaded = std::sync::Arc::try_unwrap(loaded).unwrap_or_else(|shared| Loaded {
        l1: shared.l1.clone(),
        bytes: shared.bytes.clone(),
        stale: shared.stale,
        language: shared.language.clone(),
    });
    Ok(Chosen {
        path,
        loaded,
        index,
        also_defined,
    })
}

/// Whether `symbol` lies inside a definition of `symbols` named `qualifier` (an `impl` block's name
/// may be `Trait for Type`, so its last word counts too).
fn inside_named(symbols: &[Symbol], symbol: &Symbol, qualifier: &str) -> bool {
    symbols.iter().any(|outer| {
        let named = outer.name == qualifier || outer.name.rsplit(' ').next() == Some(qualifier);
        named
            && outer.start_byte <= symbol.start_byte
            && symbol.end_byte <= outer.end_byte
            && (outer.start_byte, outer.end_byte) != (symbol.start_byte, symbol.end_byte)
    })
}

/// The doc comment ending just above `symbol`, with only whitespace between them.
fn doc_above(docs: &[crate::extract::DocComment], symbol: &Symbol, source: &[u8]) -> Option<String> {
    let start = symbol.start_byte as usize;
    let doc = docs
        .iter()
        .filter(|doc| doc.end_byte as usize <= start)
        .max_by_key(|doc| doc.end_byte)?;
    let gap = source.get(doc.end_byte as usize..start)?;
    if !gap.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    let text = doc.text.trim();
    let mut cut = text.len().min(DOC_BYTES_MAX);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Some(text[..cut].to_string())
}

/// `tool` with `args`, as a continuation names it.
fn call(tool: &str, args: Value) -> String {
    format!("{tool} {args}")
}

/// Another operation's arguments, built from JSON so a field it gains later takes its default.
fn params<P: DeserializeOwned>(args: Value) -> Result<P, EngineError> {
    serde_json::from_value(args).map_err(|error| EngineError::invalid_argument(format!("`code explain`: {error}")))
}

/// Cut `rows` to `limit`, listing the section as cut when it held more.
fn cut<T>(briefing: &mut Briefing, section: &'static str, rows: &mut Vec<T>, total: usize, limit: usize) {
    if rows.len() > limit {
        rows.truncate(limit);
    }
    if total > rows.len() {
        briefing.response.truncated_sections.push(TruncatedSection {
            section,
            shown: rows.len(),
            total: Some(total),
            reason: "limit",
            next: briefing.next.get(section).cloned().unwrap_or_default(),
        });
    }
}

fn symbol_briefing(ws: &Workspace, chosen: Chosen, asked: &Asked<'_>) -> Result<Briefing, EngineError> {
    let Chosen {
        path,
        loaded,
        index,
        also_defined,
    } = chosen;
    let (limit, want) = (asked.limit, asked.want);
    let symbol = &loaded.l1.symbols[index];
    let (line, end_line) = line_span(symbol, Some(&loaded.bytes));
    let name = symbol.name.clone();
    let l2 = calls_and_docs(ws, &path, &loaded);
    let path_text = path.to_string();
    let mut briefing = Briefing {
        response: ExplainResponse {
            target: "symbol",
            also_defined,
            ..ExplainResponse::default()
        },
        next: AHashMap::new(),
    };
    let read = call(
        "code_read",
        json!({"path": path_text, "line": line, "end_line": end_line}),
    );
    briefing.next.insert("body", read.clone());
    briefing.next.insert("callees", read);
    briefing.next.insert(
        "callers",
        call(
            "code_refs",
            json!({"name": name, "path": path_text, "line": line, "limit": REFS_LIMIT_MAX}),
        ),
    );
    briefing.next.insert(
        "importers",
        call("code_refs", json!({"relation": "importers", "path": path_text})),
    );
    briefing.next.insert(
        "history",
        call("git_log", json!({"path": path_text, "symbol": name, "line": line})),
    );

    if want("body") && asked.code {
        let lines = end_line - line + 1;
        if lines <= BODY_LINES_MAX {
            let start = (symbol.start_byte as usize).min(loaded.bytes.len());
            let end = (symbol.end_byte as usize).min(loaded.bytes.len()).max(start);
            briefing.response.body = Some(String::from_utf8_lossy(&loaded.bytes[start..end]).into_owned());
        } else {
            let next = briefing.next["body"].clone();
            briefing.response.truncated_sections.push(TruncatedSection {
                section: "body",
                shown: 0,
                total: Some(lines as usize),
                reason: "limit",
                next,
            });
        }
    }

    if want("callees") {
        let mut seen = AHashSet::new();
        let mut callees: Vec<String> = l2
            .iter()
            .flat_map(|l2| l2.calls.iter())
            .filter(|c| symbol.start_byte <= c.start_byte && c.start_byte < symbol.end_byte && c.callee != name)
            .filter(|c| seen.insert(c.callee.as_str()))
            .map(|c| c.callee.clone())
            .collect();
        let total = callees.len();
        cut(&mut briefing, "callees", &mut callees, total, limit);
        briefing.response.callees = Some(callees);
    }

    if want("callers") {
        callers(ws, &mut briefing, &path, &name, line, limit);
    }
    if want("importers") {
        importers(ws, &mut briefing, &path, limit);
    }
    if want("history") {
        let args = json!({"path": path_text, "symbol": name, "line": line, "limit": HISTORY_MAX});
        match git_log(ws, args) {
            Ok(GitLogResult::Symbol(answer)) => {
                briefing.response.history = Some(
                    answer
                        .history
                        .into_iter()
                        .map(|entry| ExplainCommit {
                            sha: entry.sha,
                            author: entry.author,
                            time: entry.author_time_unix,
                            summary: entry.summary,
                        })
                        .collect(),
                );
            }
            Ok(_) => {}
            Err(note) => briefing.response.notes.push(note),
        }
    }

    briefing.response.definition = Some(ExplainDefinition {
        path,
        name,
        kind: kind_to_str(symbol.kind),
        line,
        end_line,
        signature: symbol.signature.clone(),
        doc: l2.as_ref().and_then(|l2| doc_above(&l2.docs, symbol, &loaded.bytes)),
        stale: loaded.stale,
    });
    Ok(briefing)
}

/// The call sites of the definition at `path:line`, as `code_refs` callers lists them.
fn callers(ws: &Workspace, briefing: &mut Briefing, path: &RelPath, name: &str, line: u32, limit: usize) {
    let args = json!({"name": name, "relation": "callers", "path": path.to_string(), "line": line, "limit": limit});
    let answer = params(args).and_then(|p| ws.code_refs(p));
    let answer = match answer {
        Ok(CodeRefsResult::Callers(answer)) => serde_json::to_value(answer).unwrap_or(Value::Null),
        Ok(_) => Value::Null,
        Err(error) => {
            briefing.response.notes.push(format!("callers: {}", error));
            return;
        }
    };
    let mut rows: Vec<(String, Value)> = answer
        .get("hits")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .flat_map(|(file, list)| {
            list.as_array()
                .into_iter()
                .flatten()
                .map(move |row| (file.clone(), row.clone()))
        })
        .collect();
    let total = answer
        .get("total")
        .and_then(Value::as_u64)
        .map_or(rows.len(), |total| total as usize);
    cut(briefing, "callers", &mut rows, total, limit);
    let response = &mut briefing.response;
    response.callers_total = Some(total);
    response.callers_resolved = answer.get("resolved_total").and_then(Value::as_u64);
    response.callers_resolution_default = answer.get("resolution_default").cloned();
    if let Some(note) = answer.get("resolution_note").and_then(Value::as_str) {
        response.notes.push(format!("callers: {note}"));
    }
    response.callers = Some(rows);
}

/// The files importing `path`, one hop over runtime imports.
fn importers(ws: &Workspace, briefing: &mut Briefing, path: &RelPath, limit: usize) {
    match neighbours(ws, path, Direction::Importers) {
        Ok((mut files, notes)) => {
            let total = files.len();
            cut(briefing, "importers", &mut files, total, limit);
            briefing.response.importers = Some(files);
            push_new(&mut briefing.response.notes, notes);
        }
        Err(note) => briefing.response.notes.push(note),
    }
}

/// `path`'s neighbours one hop away in `direction`, in path order, and the graph's notes.
fn neighbours(ws: &Workspace, path: &RelPath, direction: Direction) -> Result<(Vec<RelPath>, Vec<String>), String> {
    let graph = import_graph(ws).map_err(|error| format!("import graph: {}", error))?;
    let notes = graph_notes(&graph);
    let Some(node) = graph.node(&path.to_str_lossy()) else {
        return Err(format!(
            "import graph: `{path}` is in no language with an import resolver, so its imports are unknown"
        ));
    };
    let walk = graph.walk(&[node], direction, WalkLimits::new(Some(1)), tiers(false, false));
    let mut files: Vec<RelPath> = walk.reached.iter().map(|hit| graph.path(hit.node).clone()).collect();
    files.sort_unstable();
    Ok((files, notes))
}

fn push_new(notes: &mut Vec<String>, more: Vec<String>) {
    for note in more {
        if !notes.contains(&note) {
            notes.push(note);
        }
    }
}

/// `git_log` with `args`; a failure (no repository, an unreadable history) is a note.
fn git_log(ws: &Workspace, args: Value) -> Result<GitLogResult, String> {
    params(args)
        .and_then(|p| ws.git_log(p))
        .map_err(|error| format!("history: {}", error))
}

fn file_briefing(ws: &Workspace, path: RelPath, asked: &Asked<'_>) -> Result<Briefing, EngineError> {
    let (limit, want) = (asked.limit, asked.want);
    let loaded = load(ws, &path)?;
    let path_text = path.to_string();
    let mut briefing = Briefing {
        response: ExplainResponse {
            target: "file",
            language: Some(loaded.language.clone()),
            lines: Some(line_count(&loaded.bytes)),
            symbols: Some(loaded.l1.symbols.len()),
            ..ExplainResponse::default()
        },
        next: AHashMap::new(),
    };
    briefing
        .next
        .insert("outline", call("code_outline", json!({"path": path_text})));
    briefing.next.insert(
        "importers",
        call("code_refs", json!({"relation": "importers", "path": path_text})),
    );
    briefing
        .next
        .insert("history", call("git_log", json!({"path": path_text})));

    if want("outline") {
        let symbols = &loaded.l1.symbols;
        let mut rows: Vec<ExplainOutlineRow> = symbols
            .iter()
            .filter(|symbol| !is_nested(symbols, symbol))
            .map(|symbol| ExplainOutlineRow {
                name: symbol.name.clone(),
                kind: kind_to_str(symbol.kind),
                line: symbol.start_row + 1,
            })
            .collect();
        let total = rows.len();
        cut(&mut briefing, "outline", &mut rows, total, limit);
        briefing.response.outline = Some(rows);
    }
    if want("imports") {
        match neighbours(ws, &path, Direction::Dependencies) {
            Ok((mut files, notes)) => {
                let total = files.len();
                briefing
                    .next
                    .insert("imports", call("code_outline", json!({"path": path_text})));
                cut(&mut briefing, "imports", &mut files, total, limit);
                briefing.response.imports = Some(files);
                push_new(&mut briefing.response.notes, notes);
            }
            Err(note) => briefing.response.notes.push(note),
        }
    }
    if want("importers") {
        importers(ws, &mut briefing, &path, limit);
    }
    if want("hotspot") {
        hotspot(ws, &mut briefing, &path);
    }
    if want("history") || want("owners") {
        match git_log(ws, json!({"path": path_text, "limit": OWNER_COMMITS})) {
            Ok(GitLogResult::Touching(answer)) => {
                if want("owners") {
                    briefing.response.owners = Some(owners(answer.commits.iter().map(|c| c.author.as_str())));
                }
                if want("history") {
                    briefing.response.history = Some(
                        answer
                            .commits
                            .into_iter()
                            .take(HISTORY_MAX as usize)
                            .map(|commit| ExplainCommit {
                                sha: commit.sha,
                                author: commit.author,
                                time: commit.author_time_unix,
                                summary: commit.summary,
                            })
                            .collect(),
                    );
                }
            }
            Ok(_) => {}
            Err(note) => briefing.response.notes.push(note),
        }
    }
    briefing.response.path = Some(path);
    Ok(briefing)
}

/// Lines of `bytes`, a final unterminated line counted.
fn line_count(bytes: &[u8]) -> u32 {
    let newlines = memchr::memchr_iter(b'\n', bytes).count();
    let open = usize::from(bytes.last().is_some_and(|b| *b != b'\n'));
    u32::try_from(newlines + open).unwrap_or(u32::MAX)
}

/// Authors by commits, most first, then by name.
fn owners<'a>(authors: impl Iterator<Item = &'a str>) -> Vec<ExplainOwner> {
    let mut counts: AHashMap<&str, u32> = AHashMap::new();
    for author in authors {
        *counts.entry(author).or_default() += 1;
    }
    let mut owners: Vec<ExplainOwner> = counts
        .into_iter()
        .map(|(author, commits)| ExplainOwner {
            author: author.to_string(),
            commits,
        })
        .collect();
    owners.sort_by(|a, b| b.commits.cmp(&a.commits).then_with(|| a.author.cmp(&b.author)));
    owners
}

/// The file's row of `git hotspots` (without its path), or empty `signals` when it has none.
fn hotspot(ws: &Workspace, briefing: &mut Briefing, path: &RelPath) {
    let answer = params(json!({"limit": HOTSPOT_ROWS})).and_then(|p| ws.git_hotspots(p));
    let answer = match answer {
        Ok(answer) => serde_json::to_value(answer).unwrap_or(Value::Null),
        Err(error) => {
            briefing.response.notes.push(format!("hotspot: {}", error));
            return;
        }
    };
    let path_text = path.to_string();
    let row = answer
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|row| row.get("path").and_then(Value::as_str) == Some(path_text.as_str()))
        .cloned();
    briefing.response.hotspot = Some(match row {
        Some(Value::Object(mut row)) => {
            row.remove("path");
            Value::Object(row)
        }
        _ => json!({"signals": []}),
    });
}

/// Estimated tokens of the answer as written.
fn tokens(response: &ExplainResponse) -> u64 {
    serde_json::to_vec(response).map_or(0, |bytes| bytes_to_tokens(bytes.len() as u64))
}

/// Cut the answer to `budget` tokens: the body first, then the longest list section by half, until it
/// fits or every list holds one row. Each cut is listed in `truncated_sections`.
fn fit(briefing: &mut Briefing, budget: u64) {
    while tokens(&briefing.response) > budget {
        if briefing.response.body.take().is_some() {
            mark(briefing, "body", 0, None);
            continue;
        }
        let response = &briefing.response;
        let lengths = [
            ("callers", response.callers.as_ref().map_or(0, Vec::len)),
            ("callees", response.callees.as_ref().map_or(0, Vec::len)),
            ("importers", response.importers.as_ref().map_or(0, Vec::len)),
            ("imports", response.imports.as_ref().map_or(0, Vec::len)),
            ("outline", response.outline.as_ref().map_or(0, Vec::len)),
            ("history", response.history.as_ref().map_or(0, Vec::len)),
            ("owners", response.owners.as_ref().map_or(0, Vec::len)),
        ];
        let Some((section, len)) = lengths
            .into_iter()
            .filter(|(_, len)| *len > 1)
            .max_by_key(|(_, len)| *len)
        else {
            return;
        };
        let keep = len / 2;
        let response = &mut briefing.response;
        match section {
            "callers" => response.callers.as_mut().map(|rows| rows.truncate(keep)),
            "callees" => response.callees.as_mut().map(|rows| rows.truncate(keep)),
            "importers" => response.importers.as_mut().map(|rows| rows.truncate(keep)),
            "imports" => response.imports.as_mut().map(|rows| rows.truncate(keep)),
            "outline" => response.outline.as_mut().map(|rows| rows.truncate(keep)),
            "history" => response.history.as_mut().map(|rows| rows.truncate(keep)),
            _ => response.owners.as_mut().map(|rows| rows.truncate(keep)),
        };
        mark(briefing, section, keep, Some(len));
    }
}

/// Record that the budget cut `section` to `shown` rows (of `total` when the section had no cut yet).
fn mark(briefing: &mut Briefing, section: &'static str, shown: usize, total: Option<usize>) {
    let sections = &mut briefing.response.truncated_sections;
    if let Some(cut) = sections.iter_mut().find(|cut| cut.section == section) {
        cut.shown = shown;
        cut.reason = "budget";
        return;
    }
    sections.push(TruncatedSection {
        section,
        shown,
        total,
        reason: "budget",
        next: briefing.next.get(section).cloned().unwrap_or_default(),
    });
}

#[cfg(test)]
#[path = "explain_tests.rs"]
mod tests;
