//! The symbol briefing of `code explain`: definition, body, callers (and their callers over resolved
//! edges), callees, the definition file's importers and the symbol's history.

use ahash::AHashSet;
use serde_json::{Value, json};

use crate::engine::error::EngineError;
use crate::engine::pick::line_span;
use crate::engine::types::code::kind_to_str;
use crate::engine::types::explain::{ExplainCommit, ExplainDefinition, ExplainIndirectCaller, ExplainResponse};
use crate::engine::types::git::GitLogResult;
use crate::engine::types::refs::{CodeRefsResult, FindCallersResponse, Resolution};
use crate::engine::workspace::Workspace;
use crate::path::RelPath;

use super::pick::{Chosen, callable, doc_above};
use super::{Asked, Briefing, call, calls_and_docs, cut, explain_call, is_macro, listed_cut, load, params, push_new};

/// A symbol's body is listed when it spans at most this many lines.
const BODY_LINES_MAX: u32 = 60;
/// Commits a symbol briefing's history lists.
const HISTORY_MAX: u32 = 5;
/// Rows `code_refs` returns at most, for a continuation call.
const REFS_LIMIT_MAX: usize = 1_000;
/// The resolutions that prove a call site binds to the definition: the edges `depth` walks.
const RESOLVED: &str = "same_file,imported";

pub(super) fn briefing(ws: &Workspace, chosen: Chosen, asked: &Asked<'_>) -> Result<Briefing, EngineError> {
    let Chosen {
        path,
        loaded,
        index,
        also_defined,
        looked_up,
        notes,
    } = chosen;
    let (limit, want) = (asked.limit, asked.want);
    let symbol = &loaded.l1.symbols[index];
    let (line, end_line) = line_span(symbol, Some(&loaded.bytes));
    let name = symbol.name.clone();
    let l2 = calls_and_docs(ws, &path, &loaded);
    let path_text = path.to_string();
    let target = format!("{path_text}:{line}");
    let doc = l2.as_ref().and_then(|l2| doc_above(&l2.docs, symbol, &loaded.bytes));
    let mut briefing = Briefing {
        response: ExplainResponse {
            target: "symbol",
            notes,
            ..ExplainResponse::default()
        },
        next: ahash::AHashMap::new(),
    };
    let next = &mut briefing.next;
    next.insert(
        "body",
        call(
            "code_read",
            json!({"path": path_text, "line": line, "end_line": end_line}),
        ),
    );
    next.insert(
        "signature",
        call("code_read", json!({"path": path_text, "line": line, "end_line": line})),
    );
    if let Some((_, doc_line)) = &doc {
        next.insert(
            "doc",
            call(
                "code_read",
                json!({"path": path_text, "line": doc_line, "end_line": line.saturating_sub(1).max(*doc_line)}),
            ),
        );
    }
    next.insert("callees", explain_call(&target, "callees"));
    next.insert(
        "callers",
        call(
            "code_refs",
            json!({"name": name, "path": path_text, "line": line, "limit": REFS_LIMIT_MAX}),
        ),
    );
    next.insert(
        "indirect_callers",
        call(
            "code_explain",
            json!({"target": target, "sections": "callers", "depth": asked.depth, "limit": super::LIMIT_MAX}),
        ),
    );
    next.insert(
        "importers",
        call("code_refs", json!({"relation": "importers", "path": path_text})),
    );
    next.insert(
        "history",
        call("git_log", json!({"path": path_text, "symbol": name, "line": line})),
    );
    next.insert(
        "also_defined",
        call("code_symbols", json!({"name": looked_up, "limit": REFS_LIMIT_MAX})),
    );
    next.insert(
        "notes",
        call(
            "code_explain",
            json!({"target": target, "max_tokens": 4 * crate::engine::budget::DEFAULT_MAX_TOKENS}),
        ),
    );

    let mut also_defined = also_defined;
    cut(&mut briefing, "also_defined", &mut also_defined, limit);
    briefing.response.also_defined = also_defined;

    if want("body") && asked.code {
        let lines = end_line - line + 1;
        if lines <= BODY_LINES_MAX {
            let start = (symbol.start_byte as usize).min(loaded.bytes.len());
            let end = (symbol.end_byte as usize).min(loaded.bytes.len()).max(start);
            briefing.response.body = Some(String::from_utf8_lossy(&loaded.bytes[start..end]).into_owned());
        } else {
            listed_cut(&mut briefing, "body", 0, Some(lines as usize), "limit");
        }
    }

    if want("callees") {
        match &l2 {
            Some(l2) => {
                let mut seen = AHashSet::new();
                let mut callees: Vec<String> = l2
                    .calls
                    .iter()
                    .filter(|c| symbol.start_byte <= c.start_byte && c.start_byte < symbol.end_byte && c.callee != name)
                    .filter(|c| !is_macro(&loaded.bytes, c.start_byte, &c.callee))
                    .filter(|c| seen.insert(c.callee.as_str()))
                    .map(|c| c.callee.clone())
                    .collect();
                cut(&mut briefing, "callees", &mut callees, limit);
                briefing.response.callees = Some(callees);
            }
            None => briefing.response.notes.push(format!(
                "callees: no call extraction for {}, so the names the body calls are unknown",
                loaded.language
            )),
        }
    }

    if want("callers") {
        callers(ws, &mut briefing, &path, &name, line, asked);
    }
    if want("importers") {
        super::file::importers(ws, &mut briefing, &path, &loaded.language, limit);
    }
    if want("history") {
        history(ws, &mut briefing, &path_text, &name, line);
    }

    briefing.response.definition = Some(ExplainDefinition {
        path,
        name,
        kind: kind_to_str(symbol.kind),
        line,
        end_line,
        signature: symbol.signature.clone(),
        doc: doc.map(|(text, _)| text),
        stale: loaded.stale,
    });
    Ok(briefing)
}

/// The last commits that changed the symbol's body, listed as cut when there are more.
fn history(ws: &Workspace, briefing: &mut Briefing, path: &str, name: &str, line: u32) {
    let args = json!({"path": path, "symbol": name, "line": line, "limit": HISTORY_MAX});
    match super::file::git_log(ws, args) {
        Ok(GitLogResult::Symbol(answer)) => {
            let more = answer.next_cursor.is_some() || answer.truncated;
            let commits: Vec<ExplainCommit> = answer
                .history
                .into_iter()
                .map(|entry| ExplainCommit {
                    sha: entry.sha,
                    author: entry.author,
                    time: entry.author_time_unix,
                    summary: entry.summary,
                })
                .collect();
            if more {
                listed_cut(briefing, "history", commits.len(), None, "limit");
            }
            briefing.response.history = Some(commits);
        }
        Ok(_) => {}
        Err(note) => briefing.response.notes.push(note),
    }
}

/// The pinned `code_refs` callers page of the definition `name` at `path:line`, keeping only rows of
/// `resolution` when given.
pub(in crate::engine::ops) fn callers_page(
    ws: &Workspace,
    path: &RelPath,
    name: &str,
    line: u32,
    limit: usize,
    resolution: Option<&str>,
) -> Result<FindCallersResponse, EngineError> {
    let mut args = json!({"name": name, "relation": "callers", "path": path.to_string(), "line": line, "limit": limit});
    if let Some(resolution) = resolution {
        args["resolution"] = json!(resolution);
    }
    match params(args).and_then(|p| ws.code_refs(p))? {
        CodeRefsResult::Callers(answer) => Ok(answer),
        _ => Err(EngineError::internal("`code_refs` callers answered another relation")),
    }
}

/// The call sites of the definition at `path:line`, as `code_refs` callers lists them, and with
/// `depth` above 1 the callers of the definitions holding them over resolved edges.
fn callers(ws: &Workspace, briefing: &mut Briefing, path: &RelPath, name: &str, line: u32, asked: &Asked<'_>) {
    let limit = asked.limit;
    let answer = match callers_page(ws, path, name, line, limit, None) {
        Ok(answer) => answer,
        Err(error) => {
            briefing.response.notes.push(format!("callers: {error}"));
            return;
        }
    };
    let default = answer.resolution_default.unwrap_or(Resolution::NameOnly);
    let frontier: Vec<(RelPath, u32)> = answer
        .hits
        .iter()
        .filter(|hit| matches!(hit.resolution, Resolution::SameFile | Resolution::Imported))
        .map(|hit| (hit.path.clone(), hit.line))
        .collect();
    let (total, truncated, truncated_reason, budgeted) = (
        answer.total as usize,
        answer.truncated,
        answer.truncated_reason,
        answer.budgeted,
    );
    let (resolved, note) = (answer.resolved_total, answer.resolution_note.clone());
    let value = serde_json::to_value(answer).unwrap_or(Value::Null);
    let rows: Vec<(String, Value)> = value
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
    if truncated {
        briefing.response.notes.push(format!(
            "callers: the scan stopped early ({}), so `callers_total` is a lower bound",
            truncated_reason.unwrap_or("cap")
        ));
    }
    if total > rows.len() || truncated {
        let reason = if budgeted { "budget" } else { "limit" };
        listed_cut(briefing, "callers", rows.len(), (!truncated).then_some(total), reason);
    }
    let response = &mut briefing.response;
    response.callers_total = Some(total);
    response.callers_resolved = resolved.map(u64::from);
    response.callers_resolution_default = Some(default.as_str());
    if let Some(note) = note {
        response.notes.push(format!("callers: {note}"));
    }
    response.callers = Some(rows);
    if asked.depth > 1 {
        indirect_callers(ws, briefing, (path.clone(), line), frontier, asked);
    }
}

/// Callers at depth 2 and beyond: for each call site proven to bind (`same_file` / `imported`), the
/// callable definition holding it, and that definition's call sites proven to bind to it.
fn indirect_callers(
    ws: &Workspace,
    briefing: &mut Briefing,
    start: (RelPath, u32),
    mut frontier: Vec<(RelPath, u32)>,
    asked: &Asked<'_>,
) {
    let limit = asked.limit;
    let mut visited: AHashSet<(RelPath, u32)> = AHashSet::from_iter([start]);
    let mut rows: Vec<ExplainIndirectCaller> = Vec::new();
    let mut more = false;
    let mut notes = Vec::new();
    'walk: for depth in 2..=asked.depth {
        let mut next_frontier = Vec::new();
        for (site_path, site_line) in std::mem::take(&mut frontier) {
            let Some((holder, holder_line)) = holding_callable(ws, &site_path, site_line) else {
                continue;
            };
            if !visited.insert((site_path.clone(), holder_line)) {
                continue;
            }
            let page = match callers_page(ws, &site_path, &holder, holder_line, limit, Some(RESOLVED)) {
                Ok(page) => page,
                Err(error) => {
                    notes.push(format!("callers at depth {depth}: {error}"));
                    continue;
                }
            };
            more |= page.next_cursor.is_some() || page.truncated || page.budgeted;
            for hit in page.hits {
                if rows.len() >= limit {
                    more = true;
                    break 'walk;
                }
                next_frontier.push((hit.path.clone(), hit.line));
                rows.push(ExplainIndirectCaller {
                    depth,
                    path: hit.path,
                    line: hit.line,
                    callee: holder.clone(),
                    callee_at: format!("{site_path}:{holder_line}"),
                });
            }
        }
        frontier = next_frontier;
    }
    if more {
        listed_cut(briefing, "indirect_callers", rows.len(), None, "limit");
    }
    if rows.is_empty() && notes.is_empty() {
        notes.push(
            "callers beyond depth 1: none over resolved edges (a `name_only` caller is not followed)".to_string(),
        );
    }
    push_new(&mut briefing.response.notes, notes);
    briefing.response.indirect_callers = Some(rows);
}

/// The innermost callable definition of `path` holding 1-based `line`: its name and first line.
pub(in crate::engine::ops) fn holding_callable(ws: &Workspace, path: &RelPath, line: u32) -> Option<(String, u32)> {
    let loaded = load(ws, path).ok()?;
    loaded
        .l1
        .symbols
        .iter()
        .filter(|symbol| callable(symbol.kind))
        .filter(|symbol| {
            let (first, last) = line_span(symbol, Some(&loaded.bytes));
            (first..=last).contains(&line)
        })
        .min_by_key(|symbol| symbol.end_byte.saturating_sub(symbol.start_byte))
        .map(|symbol| (symbol.name.clone(), symbol.start_row + 1))
}

/// One call a definition's body makes, as `callees_of` lists it.
pub(in crate::engine::ops) struct Callee {
    pub(in crate::engine::ops) name: String,
    /// The file defining the callee, when the resolution facts bind the call.
    pub(in crate::engine::ops) definition: Option<RelPath>,
}

/// The distinct calls the definition holding 1-based `line` of `path` makes, each bound to its
/// definition by the resolution facts when it has one. A call to the definition itself is left out:
/// by binding when the facts bind it, by name when they do not. The reason, when the language has no
/// call extraction, the file is not indexed, the index is held by another process, or the file
/// changed since the scan and no longer holds `name` at `line`. A file that changed since the scan
/// gets no bindings.
pub(in crate::engine::ops) fn callees_of(
    ws: &Workspace,
    path: &RelPath,
    name: &str,
    line: u32,
) -> Result<Vec<Callee>, String> {
    let chosen = super::pick::symbol_named_at(ws, path.clone(), name, line).map_err(|error| error.to_string())?;
    let (loaded, symbol) = (&chosen.loaded, &chosen.loaded.l1.symbols[chosen.index]);
    if loaded.stale && (symbol.name != name || symbol.start_row + 1 != line) {
        return Err(format!(
            "`{path}` changed since the scan and no longer holds `{name}` at line {line}: rescan"
        ));
    }
    let l2 = calls_and_docs(ws, path, loaded).ok_or_else(|| format!("no call extraction for {}", loaded.language))?;
    let view = ws
        .store_read()
        .read_index_view()
        .ok_or_else(|| "the index is held by another basemind process: retry".to_string())?;
    let bindings = crate::intel::resolve::supports(&loaded.language) && !loaded.stale;
    let mut seen = AHashSet::new();
    let mut callees = Vec::new();
    for site in l2
        .calls
        .iter()
        .filter(|site| symbol.start_byte <= site.start_byte && site.start_byte < symbol.end_byte)
        .filter(|site| !is_macro(&loaded.bytes, site.start_byte, &site.callee))
    {
        let bound = bindings
            .then(|| view.definition_of(path, site.start_byte))
            .flatten()
            .filter(|_| crate::engine::refs::intel::identifier_at(&loaded.bytes, site.start_byte) == site.callee);
        if site.callee == symbol.name
            && bound
                .as_ref()
                .is_none_or(|(file, at)| file == path && (symbol.start_byte..symbol.end_byte).contains(at))
        {
            continue;
        }
        let definition = bound.map(|(definition, _)| definition);
        if seen.insert((site.callee.clone(), definition.clone())) {
            callees.push(Callee {
                name: site.callee.clone(),
                definition,
            });
        }
    }
    Ok(callees)
}
