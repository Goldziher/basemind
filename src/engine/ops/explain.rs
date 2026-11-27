//! `basemind code explain` (#120): one briefing on a symbol or a file, composed from what the other
//! operations already answer, with no index data of its own. A symbol briefing holds the definition
//! (signature, doc, line span, and the body when it is short), its callers with their resolution
//! grades (`code_refs` callers, and with `depth` the callers of those over resolved edges), the
//! names its body calls, the files importing its file, and the commits that changed its body. A file
//! briefing holds the outline, the files it imports and that import it, the calls into and out of it
//! grouped by file, its `git hotspots` row, its recent commits and their authors.
//!
//! List sections are cut at `limit` rows and the whole answer at the token budget; each cut section
//! is listed in `truncated_sections` with the call that continues it. An answer that cannot be cut
//! under the budget is refused with the smallest budget that fits.

use ahash::AHashMap;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::engine::budget::budget;
use crate::engine::error::EngineError;
use crate::engine::files::{indexed_file, not_indexed_in};
use crate::engine::freshness;
use crate::engine::timing::elapsed_us;
use crate::engine::tokens::bytes_to_tokens;
use crate::engine::types::explain::{ExplainParams, ExplainResponse, TruncatedSection};
use crate::engine::workspace::Workspace;
use crate::extract::FileMapL1;
use crate::path::RelPath;

#[path = "explain_file.rs"]
mod file;
#[path = "explain_pick.rs"]
mod pick;
#[path = "explain_symbol.rs"]
mod symbol;

pub(super) use symbol::{callees_of, callers_page, holding_callable};

/// Rows per list section when the caller passes no `limit`, and the most it may ask for.
const LIMIT_DEFAULT: u32 = 20;
const LIMIT_MAX: u32 = 200;
/// Caller hops a symbol briefing walks at most; 1 (direct callers) by default.
const DEPTH_MAX: u32 = 3;

const SYMBOL_SECTIONS: &[&str] = &["body", "callers", "callees", "importers", "history"];
const FILE_SECTIONS: &[&str] = &[
    "outline",
    "imports",
    "importers",
    "calls_in",
    "calls_out",
    "hotspot",
    "history",
    "owners",
];

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

/// What the caller asked of every section.
struct Asked<'a> {
    limit: usize,
    depth: u32,
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
        let depth = p.depth.unwrap_or(1);
        if depth == 0 || depth > DEPTH_MAX {
            return Err(shape(&format!("`depth` is 1 to {DEPTH_MAX}")));
        }
        let target = classify(self, &p.target)?;
        let file = matches!(target, Target::File(_));
        if file && p.depth.is_some() {
            return Err(shape("`depth` walks a symbol's callers; a file target takes none"));
        }
        let wanted = sections(
            p.sections.as_deref(),
            if file { FILE_SECTIONS } else { SYMBOL_SECTIONS },
        )?;
        let want = |section: &str| wanted.contains(&section);
        let asked = Asked {
            limit: limit as usize,
            depth,
            code: !p.no_code,
            want: &want,
        };
        let mut briefing = match target {
            Target::File(path) => file::briefing(self, path, &asked)?,
            Target::At(path, line) => symbol::briefing(self, pick::symbol_at(self, path, line)?, &asked)?,
            Target::Name(name) => symbol::briefing(self, pick::symbol_named(self, &name)?, &asked)?,
        };
        let budget = budget(p.max_tokens).map_or(u64::MAX, u64::from);
        fit(&mut briefing, budget, p.sections.as_deref())?;
        let mut response = briefing.response;
        response.notice = self.lifecycle_notice();
        response.elapsed_us = elapsed_us(started);
        Ok(response)
    }
}

fn shape(problem: &str) -> EngineError {
    EngineError::invalid_argument(format!(
        "`code explain`: {problem}; pass a symbol name, an indexed file, or `path:line`, with optional \
         `sections`, `limit` (1 to {LIMIT_MAX}), `depth` (1 to {DEPTH_MAX}, symbols only) and `no_code`"
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
    // A file on disk the scan left out (`Makefile`, an ignored `a.rs`) is no symbol name.
    if path.contains('/') || ws.root.join(path).is_file() {
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

/// The calls and doc comments of `loaded`, extracted from its current bytes when it is stale. `None`
/// when the language has no call extraction.
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

/// Whether the call starting at `start` is a macro invocation (`name!`): a macro is no call of a
/// function, so it is no callee.
fn is_macro(source: &[u8], start: u32, callee: &str) -> bool {
    let start = start as usize;
    let end = start + callee.len();
    source.get(start..end) == Some(callee.as_bytes()) && source.get(end) == Some(&b'!')
}

/// `tool` with `args`, as a continuation names it.
fn call(tool: &str, args: Value) -> String {
    format!("{tool} {args}")
}

/// The `code explain` call that lists `section` of `target` alone, with the most rows.
fn explain_call(target: &str, section: &str) -> String {
    call(
        "code_explain",
        json!({"target": target, "sections": section, "limit": LIMIT_MAX}),
    )
}

/// Another operation's arguments, built from JSON so a field it gains later takes its default.
fn params<P: DeserializeOwned>(args: Value) -> Result<P, EngineError> {
    serde_json::from_value(args).map_err(|error| EngineError::invalid_argument(format!("`code explain`: {error}")))
}

/// Cut `rows` to `limit`, listing the section as cut when it held more.
fn cut<T>(briefing: &mut Briefing, section: &'static str, rows: &mut Vec<T>, limit: usize) {
    let had = rows.len();
    if had > limit {
        rows.truncate(limit);
        listed_cut(briefing, section, limit, Some(had), "limit");
    }
}

/// List `section` as cut: `shown` rows of `total` (unknown when `None`), stopped by `reason`.
fn listed_cut(
    briefing: &mut Briefing,
    section: &'static str,
    shown: usize,
    total: Option<usize>,
    reason: &'static str,
) {
    briefing.response.truncated_sections.push(TruncatedSection {
        section,
        shown,
        total,
        reason,
        next: briefing.next.get(section).cloned().unwrap_or_default(),
    });
}

/// Push each note of `more` that `notes` does not hold yet.
fn push_new(notes: &mut Vec<String>, more: Vec<String>) {
    for note in more {
        if !notes.contains(&note) {
            notes.push(note);
        }
    }
}

/// Estimated tokens of the answer as written.
fn tokens(response: &ExplainResponse) -> u64 {
    serde_json::to_vec(response).map_or(0, |bytes| bytes_to_tokens(bytes.len() as u64))
}

/// List sections the budget halves, longest first, and the scalar ones it drops after them.
const LISTS: &[&str] = &[
    "callers",
    "indirect_callers",
    "callees",
    "importers",
    "imports",
    "calls_in",
    "calls_out",
    "outline",
    "history",
    "owners",
    "also_defined",
];
const SCALARS: &[&str] = &["doc", "signature", "hotspot"];

fn list_len(response: &ExplainResponse, section: &str) -> usize {
    fn len<T>(rows: Option<&Vec<T>>) -> usize {
        rows.map_or(0, Vec::len)
    }
    match section {
        "callers" => len(response.callers.as_ref()),
        "indirect_callers" => len(response.indirect_callers.as_ref()),
        "callees" => len(response.callees.as_ref()),
        "importers" => len(response.importers.as_ref()),
        "imports" => len(response.imports.as_ref()),
        "calls_in" => len(response.calls_in.as_ref()),
        "calls_out" => len(response.calls_out.as_ref()),
        "outline" => len(response.outline.as_ref()),
        "history" => len(response.history.as_ref()),
        "owners" => len(response.owners.as_ref()),
        "also_defined" => response.also_defined.len(),
        "notes" => response.notes.len(),
        _ => 0,
    }
}

fn truncate_list(response: &mut ExplainResponse, section: &str, keep: usize) {
    fn keep_first<T>(rows: Option<&mut Vec<T>>, keep: usize) {
        if let Some(rows) = rows {
            rows.truncate(keep);
        }
    }
    match section {
        "callers" => keep_first(response.callers.as_mut(), keep),
        "indirect_callers" => keep_first(response.indirect_callers.as_mut(), keep),
        "callees" => keep_first(response.callees.as_mut(), keep),
        "importers" => keep_first(response.importers.as_mut(), keep),
        "imports" => keep_first(response.imports.as_mut(), keep),
        "calls_in" => keep_first(response.calls_in.as_mut(), keep),
        "calls_out" => keep_first(response.calls_out.as_mut(), keep),
        "outline" => keep_first(response.outline.as_mut(), keep),
        "history" => keep_first(response.history.as_mut(), keep),
        "owners" => keep_first(response.owners.as_mut(), keep),
        "also_defined" => response.also_defined.truncate(keep),
        "notes" => response.notes.truncate(keep),
        _ => {}
    }
}

/// Drop the scalar `section`; whether it was present.
fn drop_scalar(response: &mut ExplainResponse, section: &str) -> bool {
    match section {
        "hotspot" => response.hotspot.take().is_some(),
        "doc" => response
            .definition
            .as_mut()
            .is_some_and(|definition| definition.doc.take().is_some()),
        "signature" => response
            .definition
            .as_mut()
            .is_some_and(|definition| definition.signature.take().is_some()),
        _ => false,
    }
}

/// Cut the answer to `budget` tokens: the body first, then the longest list section by half (down to
/// no row), then the doc, signature and hotspot, then the notes. Each cut is listed in
/// `truncated_sections`. When even that is over the budget, every section is dropped and one row
/// names the `code explain` call with the budget the whole answer needs; when the bare answer is
/// still over it, the request is refused with the smallest budget that fits.
fn fit(briefing: &mut Briefing, budget: u64, sections: Option<&str>) -> Result<(), EngineError> {
    let whole = tokens(&briefing.response);
    if whole <= budget {
        return Ok(());
    }
    if briefing.response.body.take().is_some() {
        mark(briefing, "body", 0, None);
    }
    while tokens(&briefing.response) > budget {
        let longest = LISTS
            .iter()
            .map(|section| (*section, list_len(&briefing.response, section)))
            .filter(|(_, len)| *len > 0)
            .max_by_key(|(_, len)| *len);
        if let Some((section, len)) = longest {
            truncate_list(&mut briefing.response, section, len / 2);
            mark(briefing, section, len / 2, Some(len));
            continue;
        }
        if let Some(section) = SCALARS
            .iter()
            .find(|section| drop_scalar(&mut briefing.response, section))
        {
            mark(briefing, section, 0, None);
            continue;
        }
        let notes = list_len(&briefing.response, "notes");
        if notes > 0 {
            truncate_list(&mut briefing.response, "notes", notes / 2);
            mark(briefing, "notes", notes / 2, Some(notes));
            continue;
        }
        return collapse(briefing, budget, whole, sections);
    }
    Ok(())
}

/// The last resort of [`fit`]: the target alone and one row naming the call that answers in full.
fn collapse(briefing: &mut Briefing, budget: u64, whole: u64, sections: Option<&str>) -> Result<(), EngineError> {
    let response = &mut briefing.response;
    let target = match (&response.definition, &response.path) {
        (Some(definition), _) => format!("{}:{}", definition.path, definition.line),
        (None, Some(path)) => path.to_string(),
        (None, None) => String::new(),
    };
    let mut args = json!({"target": target, "max_tokens": whole + whole / 4});
    if let Some(sections) = sections {
        args["sections"] = json!(sections);
    }
    let kept = ExplainResponse {
        target: response.target,
        definition: response.definition.take(),
        path: response.path.take(),
        language: response.language.take(),
        truncated_sections: vec![TruncatedSection {
            section: "all",
            shown: 0,
            total: None,
            reason: "budget",
            next: call("code_explain", args),
        }],
        ..ExplainResponse::default()
    };
    *response = kept;
    let floor = tokens(response);
    if floor > budget {
        return Err(EngineError::invalid_argument(format!(
            "`code explain`: `max_tokens` {budget} is below the smallest answer for this target ({floor} tokens); \
             pass at least {floor}"
        )));
    }
    Ok(())
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
