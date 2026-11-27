//! The file briefing of `code explain`: outline, imports and importers, calls in and out grouped by
//! file, hotspot row, history and owners; and the import-graph and git helpers both briefings share.

use ahash::AHashMap;
use serde_json::{Value, json};

use crate::engine::error::EngineError;
use crate::engine::import_graph::{Direction, WalkLimits, import_graph};
use crate::engine::pick::is_nested;
use crate::engine::types::code::kind_to_str;
use crate::engine::types::explain::{
    ExplainCommit, ExplainFileCalls, ExplainOutlineRow, ExplainOwner, ExplainResponse,
};
use crate::engine::types::git::GitLogResult;
use crate::engine::types::refs::Resolution;
use crate::engine::workspace::Workspace;
use crate::path::RelPath;

use super::super::graph_report::{notes as graph_notes, tiers};
use super::pick::callable;
use super::symbol::callers_page;
use super::{
    Asked, Briefing, Loaded, call, calls_and_docs, cut, explain_call, is_macro, listed_cut, load, params, push_new,
};

/// Commits a file briefing's history lists.
const HISTORY_MAX: usize = 5;
/// Commits of a file whose authors are its owners.
const OWNER_COMMITS: u32 = 20;
/// Definitions of a file whose callers `calls_in` looks up; past it the counts are partial.
const CALLS_IN_DEFINITIONS_MAX: usize = 100;
/// Call sites one definition's `calls_in` lookup reads.
const CALLS_IN_SITES_MAX: usize = 1_000;
/// Definitions of a callee name read to find the one file defining it.
const DEFINERS_PROBED: usize = 16;

pub(super) fn briefing(ws: &Workspace, path: RelPath, asked: &Asked<'_>) -> Result<Briefing, EngineError> {
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
    let next = &mut briefing.next;
    next.insert("outline", call("code_outline", json!({"path": path_text})));
    next.insert(
        "importers",
        call("code_refs", json!({"relation": "importers", "path": path_text})),
    );
    for section in ["imports", "calls_in", "calls_out"] {
        next.insert(section, explain_call(&path_text, section));
    }
    next.insert("hotspot", call("git_hotspots", json!({"limit": 200})));
    next.insert("history", call("git_log", json!({"path": path_text})));
    next.insert(
        "owners",
        call("git_log", json!({"path": path_text, "limit": OWNER_COMMITS})),
    );
    next.insert(
        "notes",
        call(
            "code_explain",
            json!({"target": path_text, "max_tokens": 4 * crate::engine::budget::DEFAULT_MAX_TOKENS}),
        ),
    );

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
        cut(&mut briefing, "outline", &mut rows, limit);
        briefing.response.outline = Some(rows);
    }
    if want("imports") {
        match neighbours(ws, &path, &loaded.language, Direction::Dependencies) {
            Ok((mut files, notes)) => {
                cut(&mut briefing, "imports", &mut files, limit);
                briefing.response.imports = Some(files);
                push_new(&mut briefing.response.notes, notes);
            }
            Err(note) => briefing.response.notes.push(note),
        }
    }
    if want("importers") {
        importers(ws, &mut briefing, &path, &loaded.language, limit);
    }
    if want("calls_in") {
        calls_in(ws, &mut briefing, &path, &loaded, limit);
    }
    if want("calls_out") {
        calls_out(ws, &mut briefing, &path, &loaded, limit);
    }
    if want("hotspot") {
        hotspot(ws, &mut briefing, &path);
    }
    if want("history") || want("owners") {
        history_and_owners(ws, &mut briefing, &path_text, asked);
    }
    briefing.response.path = Some(path);
    Ok(briefing)
}

/// The file's last commits and the authors of up to [`OWNER_COMMITS`] of them.
fn history_and_owners(ws: &Workspace, briefing: &mut Briefing, path: &str, asked: &Asked<'_>) {
    let want = asked.want;
    match git_log(ws, json!({"path": path, "limit": OWNER_COMMITS})) {
        Ok(GitLogResult::Touching(answer)) => {
            let more = answer.next_cursor.is_some() || answer.truncated;
            if want("owners") {
                let response = &mut briefing.response;
                response.owners = Some(owners(answer.commits.iter().map(|c| c.author.as_str())));
                response.owners_commits = Some(answer.commits.len());
            }
            if want("history") {
                let seen = answer.commits.len();
                let commits: Vec<ExplainCommit> = answer
                    .commits
                    .into_iter()
                    .take(HISTORY_MAX)
                    .map(|commit| ExplainCommit {
                        sha: commit.sha,
                        author: commit.author,
                        time: commit.author_time_unix,
                        summary: commit.summary,
                    })
                    .collect();
                if more || seen > commits.len() {
                    let total = (!more).then_some(seen);
                    listed_cut(briefing, "history", commits.len(), total, "limit");
                }
                briefing.response.history = Some(commits);
            }
        }
        Ok(_) => {}
        Err(note) => briefing.response.notes.push(note),
    }
}

/// The files importing `path`, one hop over runtime imports.
pub(super) fn importers(ws: &Workspace, briefing: &mut Briefing, path: &RelPath, language: &str, limit: usize) {
    match neighbours(ws, path, language, Direction::Importers) {
        Ok((mut files, notes)) => {
            cut(briefing, "importers", &mut files, limit);
            briefing.response.importers = Some(files);
            push_new(&mut briefing.response.notes, notes);
        }
        Err(note) => briefing.response.notes.push(note),
    }
}

/// `path`'s neighbours one hop away in `direction`, in path order, and the graph's notes. A file of
/// a language the graph resolves that holds no edge has none (`[]`); only a language with no
/// resolver is a note.
fn neighbours(
    ws: &Workspace,
    path: &RelPath,
    language: &str,
    direction: Direction,
) -> Result<(Vec<RelPath>, Vec<String>), String> {
    let graph = import_graph(ws).map_err(|error| format!("import graph: {}", error))?;
    let notes = graph_notes(&graph);
    let Some(node) = graph.node(&path.to_str_lossy()) else {
        let resolved = graph
            .coverage()
            .languages
            .iter()
            .any(|known| known.resolver && known.language == language);
        return if resolved {
            Ok((Vec::new(), notes))
        } else {
            Err(format!(
                "import graph: no import resolver reads {language} in this build, so the imports of `{path}` are unknown"
            ))
        };
    };
    let walk = graph.walk(&[node], direction, WalkLimits::new(Some(1)), tiers(false, false));
    let mut files: Vec<RelPath> = walk.reached.iter().map(|hit| graph.path(hit.node).clone()).collect();
    files.sort_unstable();
    Ok((files, notes))
}

/// `git_log` with `args`; a failure (no repository, an unreadable history) is a note.
pub(super) fn git_log(ws: &Workspace, args: Value) -> Result<GitLogResult, String> {
    params(args)
        .and_then(|p| ws.git_log(p))
        .map_err(|error| format!("history: {}", error))
}

/// Per other file: the call sites there of this file's callable definitions, over `code_refs`
/// callers pinned to each (rows proven to bind elsewhere are left out).
fn calls_in(ws: &Workspace, briefing: &mut Briefing, path: &RelPath, loaded: &Loaded, limit: usize) {
    let symbols = &loaded.l1.symbols;
    let definitions: Vec<_> = symbols
        .iter()
        .filter(|symbol| callable(symbol.kind) && !is_nested(symbols, symbol))
        .collect();
    let mut by_file: AHashMap<RelPath, ExplainFileCalls> = AHashMap::new();
    let mut partial = Vec::new();
    for symbol in definitions.iter().take(CALLS_IN_DEFINITIONS_MAX) {
        let line = symbol.start_row + 1;
        let page = match callers_page(ws, path, &symbol.name, line, CALLS_IN_SITES_MAX, None) {
            Ok(page) => page,
            Err(error) => {
                partial.push(format!("`{}`: {}", symbol.name, error));
                continue;
            }
        };
        if page.next_cursor.is_some() || page.truncated || page.budgeted {
            partial.push(format!("`{}` has more call sites than were read", symbol.name));
        }
        for hit in page.hits.iter().filter(|hit| hit.path != *path) {
            if hit.resolution == Resolution::Elsewhere {
                continue;
            }
            let row = by_file.entry(hit.path.clone()).or_insert_with(|| ExplainFileCalls {
                path: hit.path.clone(),
                ..ExplainFileCalls::default()
            });
            row.calls += 1;
            if matches!(hit.resolution, Resolution::SameFile | Resolution::Imported) {
                row.resolved += 1;
            }
            if !row.names.contains(&symbol.name) {
                row.names.push(symbol.name.clone());
            }
        }
    }
    if definitions.len() > CALLS_IN_DEFINITIONS_MAX {
        partial.push(format!(
            "only the first {CALLS_IN_DEFINITIONS_MAX} of {} definitions were looked up",
            definitions.len()
        ));
    }
    let mut rows = ranked(by_file);
    cut(briefing, "calls_in", &mut rows, limit);
    if !partial.is_empty() {
        briefing
            .response
            .notes
            .push(format!("calls_in: counts are lower bounds: {}", partial.join("; ")));
    }
    briefing.response.calls_in = Some(rows);
}

/// Per other file: the call sites of this file whose callee binds to a definition there, by the
/// resolution facts; a site with no binding counts toward the one file defining its callee's name
/// (unresolved, matched by name). The rest (a name defined in several files or none, such as a
/// method of an external type) is counted in `calls_out_unresolved`.
fn calls_out(ws: &Workspace, briefing: &mut Briefing, path: &RelPath, loaded: &Loaded, limit: usize) {
    let Some(l2) = calls_and_docs(ws, path, loaded) else {
        briefing.response.notes.push(format!(
            "calls_out: no call extraction for {}, so its calls are unknown",
            loaded.language
        ));
        return;
    };
    let view = {
        let store = ws.store_read();
        store.read_index_view()
    };
    let Some(view) = view else {
        briefing
            .response
            .notes
            .push("calls_out: the index is held by another basemind process: retry".to_string());
        return;
    };
    let facts = crate::intel::resolve::supports(&loaded.language);
    if !facts {
        briefing.response.notes.push(format!(
            "calls_out: no resolution facts for {} in this build, so each call is matched to the one file \
             defining its name (`resolved` 0)",
            loaded.language
        ));
    } else if loaded.stale {
        briefing.response.notes.push(
            "calls_out: the file changed since the scan, so its calls are matched by name only; rescan it".to_string(),
        );
    }
    let mut definers: AHashMap<&str, Option<RelPath>> = AHashMap::new();
    let mut by_file: AHashMap<RelPath, ExplainFileCalls> = AHashMap::new();
    let mut unresolved = 0;
    for site in l2
        .calls
        .iter()
        .filter(|site| !is_macro(&loaded.bytes, site.start_byte, &site.callee))
    {
        let bound = (facts && !loaded.stale)
            .then(|| view.definition_of(path, site.start_byte))
            .flatten()
            .filter(|_| crate::engine::refs::intel::identifier_at(&loaded.bytes, site.start_byte) == site.callee);
        let (target, resolved) = match bound {
            Some((target, _)) => (Some(target), true),
            None => {
                let definer = definers
                    .entry(site.callee.as_str())
                    .or_insert_with(|| only_definer(view.as_ref(), &site.callee));
                (definer.clone(), false)
            }
        };
        let Some(target) = target else {
            unresolved += 1;
            continue;
        };
        if target == *path {
            continue;
        }
        let row = by_file.entry(target.clone()).or_insert_with(|| ExplainFileCalls {
            path: target,
            ..ExplainFileCalls::default()
        });
        row.calls += 1;
        row.resolved += u32::from(resolved);
        if !row.names.contains(&site.callee) {
            row.names.push(site.callee.clone());
        }
    }
    let mut rows = ranked(by_file);
    cut(briefing, "calls_out", &mut rows, limit);
    briefing.response.calls_out = Some(rows);
    briefing.response.calls_out_unresolved = Some(unresolved);
}

/// The one file defining `name`, if exactly one does.
fn only_definer(view: &dyn crate::index::view::IndexView, name: &str) -> Option<RelPath> {
    let mut files = view
        .symbols_by_name_lookup(name, DEFINERS_PROBED)
        .into_iter()
        .map(|(_, _, path, _)| path);
    let first = files.next()?;
    files.all(|path| path == first).then_some(first)
}

/// Rows by calls, most first, then by path.
fn ranked(by_file: AHashMap<RelPath, ExplainFileCalls>) -> Vec<ExplainFileCalls> {
    let mut rows: Vec<ExplainFileCalls> = by_file.into_values().collect();
    rows.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.path.cmp(&b.path)));
    rows
}

/// Lines of `bytes`, a final unterminated line counted.
pub(super) fn line_count(bytes: &[u8]) -> u32 {
    let newlines = memchr::memchr_iter(b'\n', bytes).count();
    let open = usize::from(bytes.last().is_some_and(|b| *b != b'\n'));
    u32::try_from(newlines + open).unwrap_or(u32::MAX)
}

/// Authors by commits, most first, then by name.
pub(super) fn owners<'a>(authors: impl Iterator<Item = &'a str>) -> Vec<ExplainOwner> {
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

/// The file's row of `git hotspots` (without its path, with its `rank` among `total` hotspots),
/// searched among every ranked file; empty `signals` only when it is none of them.
fn hotspot(ws: &Workspace, briefing: &mut Briefing, path: &RelPath) {
    let answer = match ws.git_hotspots_all() {
        Ok(answer) => answer,
        Err(error) => {
            briefing.response.notes.push(format!("hotspot: {}", error));
            return;
        }
    };
    let mut notes: Vec<String> = answer.notes.iter().map(|note| format!("hotspot: {note}")).collect();
    if let Some(reason) = answer.truncated_reason {
        notes.push(format!(
            "hotspot: the commit walk stopped before the window's start ({reason}), so the signals are partial"
        ));
    }
    push_new(&mut briefing.response.notes, notes);
    let total = answer.total;
    let found = answer.files.iter().position(|row| row.path == *path);
    briefing.response.hotspot = Some(match found.map(|rank| (rank, &answer.files[rank])) {
        Some((rank, row)) => {
            let mut row = serde_json::to_value(row).unwrap_or(Value::Null);
            if let Value::Object(fields) = &mut row {
                fields.remove("path");
                fields.insert("rank".to_string(), json!(rank + 1));
                fields.insert("of".to_string(), json!(total));
            }
            row
        }
        None => json!({"signals": []}),
    });
}
