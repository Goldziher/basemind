//! `basemind git hotspots` (#134, ADR-0047): the files where change frequency and size coincide.
//!
//! Six signals per file, each a raw number: commits and distinct authors in a window (the
//! git-history index, else a bounded live walk), lines (the file map), symbol count and the longest
//! function (the outline), and importers (the import graph's in-degree). The population ranked is
//! every indexed file in scope that a commit in the window touched; a file is reported when two or
//! more of its signals are in that signal's top slice. There is no composite score: rows sort by
//! how many signals coincide, then by commits. Branch or nesting counts are not measured.

use ahash::{AHashMap, AHashSet};
use rayon::prelude::*;

use super::common::{git_history_if_fresh, head_sha, require_git_repo};
use crate::engine::error::EngineError;
use crate::engine::files::indexed_file;
use crate::engine::freshness;
use crate::engine::import_graph::{Tiers, import_graph};
use crate::engine::timing::elapsed_us;
use crate::engine::types::git::{HotspotRow, HotspotThresholds, HotspotsParams, HotspotsResponse};
use crate::engine::workspace::Workspace;
use crate::extract::SymbolKind;
use crate::git::CommitInfo;
use crate::path::RelPath;

/// The window when the caller names none.
const SINCE_DEFAULT: &str = "90d";
/// Each signal's top slice, as a percentage of the files ranked.
const SLICE_PERCENT: u32 = 10;
/// Rows returned when the caller names no `limit`, and the most returned.
const LIMIT_DEFAULT: u32 = 20;
const LIMIT_MAX: u32 = 200;
/// Signals a file must have in their top slice to be reported.
const SIGNALS_MIN: usize = 2;
/// Commits the indexed walk inspects before it stops with `truncated_reason: "commit_cap"`.
const INDEXED_COMMIT_CAP: usize = 20_000;
/// Commits the live walk (no fresh git-history index) inspects: it diffs every commit it reads.
const LIVE_COMMIT_CAP: usize = 1_000;
/// Commits fetched from the index per round.
const CHUNK: usize = 500;
/// Consecutive commits authored before the window that end the walk. Author dates are not monotonic
/// in commit order (a rebase or cherry-pick keeps the original date), so one old commit does not.
const OUT_OF_WINDOW_RUN: usize = 50;
const SECS_PER_DAY: i64 = 86_400;

impl Workspace {
    /// `basemind git hotspots`: files where two or more change and size signals are in their top
    /// slice.
    pub(crate) fn git_hotspots(&self, p: HotspotsParams) -> Result<HotspotsResponse, EngineError> {
        run_hotspots(self, p, false)
    }

    /// `git hotspots` over the default window with every ranked row, not the first `limit`: a
    /// lookup of one file's row (`code explain`), which must not mistake a low rank for none.
    pub(crate) fn git_hotspots_all(&self) -> Result<HotspotsResponse, EngineError> {
        let p = HotspotsParams {
            since: None,
            path: None,
            limit: None,
        };
        run_hotspots(self, p, true)
    }
}

/// `<N>d`, `<N>w` or `<N>y` as seconds.
fn parse_since(since: &str) -> Result<i64, EngineError> {
    let refuse = || {
        EngineError::invalid_argument(format!(
            "`git_hotspots`: `since` must be a count of days, weeks or years such as `90d`, `12w` or `1y`, not `{since}`"
        ))
    };
    let unit = since.chars().last().ok_or_else(refuse)?;
    let days_per_unit = match unit {
        'd' => 1,
        'w' => 7,
        'y' => 365,
        _ => return Err(refuse()),
    };
    let count: i64 = since[..since.len() - 1].parse().map_err(|_| refuse())?;
    if count < 1 {
        return Err(refuse());
    }
    count.checked_mul(days_per_unit * SECS_PER_DAY).ok_or_else(refuse)
}

/// What the commits of the window say about each path they touched.
#[derive(Default)]
struct Window {
    start: i64,
    end: i64,
    commits: u32,
    /// Commits and the set of authors per path.
    per_path: AHashMap<RelPath, (u32, AHashSet<String>)>,
    truncated_reason: Option<&'static str>,
}

impl Window {
    fn add(&mut self, commit: CommitInfo) {
        self.commits += 1;
        let author = if commit.author_email.is_empty() {
            commit.author
        } else {
            commit.author_email.to_lowercase()
        };
        for (path, _) in commit.files {
            let (commits, authors) = self.per_path.entry(path).or_default();
            *commits += 1;
            if !authors.contains(&author) {
                authors.insert(author.clone());
            }
        }
    }
}

/// Walk `fetch(skip, take)` (newest first, with files) back to `span` seconds before the first
/// commit's author time, or to `cap` commits.
fn collect_window(mut fetch: impl FnMut(usize, usize) -> Vec<CommitInfo>, span: i64, cap: usize) -> Window {
    let mut window = Window::default();
    let mut anchored = false;
    let mut inspected = 0usize;
    let mut out_run = 0usize;
    loop {
        let take = CHUNK.min(cap - inspected);
        let chunk = fetch(inspected, take);
        let fetched = chunk.len();
        for commit in chunk {
            if !anchored {
                anchored = true;
                window.end = commit.author_time_unix;
                window.start = window.end.saturating_sub(span);
            }
            if commit.author_time_unix < window.start {
                out_run += 1;
                if out_run >= OUT_OF_WINDOW_RUN {
                    return window;
                }
                continue;
            }
            out_run = 0;
            window.add(commit);
        }
        inspected += fetched;
        if fetched < take {
            return window;
        }
        if inspected >= cap {
            window.truncated_reason = Some("commit_cap");
            return window;
        }
    }
}

/// The window's commits from the git-history index when it indexes HEAD, else a live walk of at
/// most [`LIVE_COMMIT_CAP`] commits.
fn window_of(ws: &Workspace, span: i64) -> Result<Window, EngineError> {
    let repo = require_git_repo(ws)?;
    let head = head_sha(repo)?;
    if let Some(index) = git_history_if_fresh(ws, &head) {
        return Ok(collect_window(
            |skip, take| index.recent_commits(skip, take, true),
            span,
            INDEXED_COMMIT_CAP,
        ));
    }
    let log = ws
        .git_cache
        .log(repo, &head, None, LIVE_COMMIT_CAP as u32, true)
        .map_err(|e| EngineError::internal(format!("log: {e}")))?;
    let mut window = collect_window(
        |skip, take| log.iter().skip(skip).take(take).cloned().collect(),
        span,
        LIVE_COMMIT_CAP,
    );
    if window.truncated_reason.is_some() {
        window.truncated_reason = Some("live_walk");
    }
    Ok(window)
}

/// The per-file numbers a hotspot row reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Signals {
    commits: u32,
    authors: u32,
    lines: u32,
    symbols: u32,
    longest_function: Option<u32>,
    longest_function_at: Option<String>,
    importers: Option<u32>,
}

/// The signal names, in the order rows list them.
const SIGNAL_NAMES: [&str; 6] = [
    "commits",
    "authors",
    "lines",
    "symbols",
    "longest_function",
    "importers",
];

impl Signals {
    fn values(&self) -> [Option<u32>; 6] {
        [
            Some(self.commits),
            Some(self.authors),
            Some(self.lines),
            Some(self.symbols),
            self.longest_function,
            self.importers,
        ]
    }
}

/// The smallest value of a signal's top slice: the values held by at most `percent` of the files
/// (at least one file). A tie that would overflow the slice is left out whole, so a value most files
/// share (a one-author repository's `authors`) marks none of them; `None` when no value qualifies
/// or the qualifying value is zero.
fn threshold(values: impl Iterator<Item = u32>, percent: u32) -> Option<u32> {
    let mut values: Vec<u32> = values.collect();
    values.sort_unstable_by(|a, b| b.cmp(a));
    let mut cut = (values.len() * percent as usize).div_ceil(100).max(1).min(values.len());
    while cut > 0 && cut < values.len() && values[cut - 1] == values[cut] {
        cut -= 1;
    }
    let at = *values.get(cut.checked_sub(1)?)?;
    (at > 0).then_some(at)
}

/// Rank `files`: each signal's threshold over the files that measured it, then the files with
/// [`SIGNALS_MIN`] or more signals at or above their threshold, sorted by how many coincide, then
/// commits, lines and path.
fn rank(files: Vec<(RelPath, Signals)>, percent: u32) -> (HotspotThresholds, Vec<HotspotRow>) {
    let mut cuts = [None; 6];
    for (i, cut) in cuts.iter_mut().enumerate() {
        *cut = threshold(files.iter().filter_map(|(_, s)| s.values()[i]), percent);
    }
    let mut rows: Vec<HotspotRow> = files
        .into_iter()
        .filter_map(|(path, s)| {
            let signals: Vec<&'static str> = s
                .values()
                .iter()
                .zip(cuts)
                .zip(SIGNAL_NAMES)
                .filter(|((value, cut), _)| matches!((value, cut), (Some(v), Some(c)) if v >= c))
                .map(|(_, name)| name)
                .collect();
            (signals.len() >= SIGNALS_MIN).then_some(HotspotRow {
                path,
                signals,
                commits: s.commits,
                authors: s.authors,
                lines: s.lines,
                symbols: s.symbols,
                longest_function: s.longest_function,
                longest_function_at: s.longest_function_at,
                importers: s.importers,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        b.signals
            .len()
            .cmp(&a.signals.len())
            .then(b.commits.cmp(&a.commits))
            .then(b.lines.cmp(&a.lines))
            .then_with(|| a.path.cmp(&b.path))
    });
    let [commits, authors, lines, symbols, longest_function, importers] = cuts;
    let thresholds = HotspotThresholds {
        commits,
        authors,
        lines,
        symbols,
        longest_function,
        importers,
    };
    (thresholds, rows)
}

/// Whether `path` is `scope` or lies under it.
fn in_scope(path: &RelPath, scope: Option<&[u8]>) -> bool {
    let Some(scope) = scope else { return true };
    let path = path.as_bytes();
    path == scope || (path.starts_with(scope) && path.get(scope.len()) == Some(&b'/'))
}

/// 1-based line of the byte at `offset`, from the sorted offsets of every newline in the file.
fn line_of(newlines: &[usize], offset: usize) -> u32 {
    newlines.partition_point(|newline| *newline < offset) as u32 + 1
}

/// The longest function or method of `symbols` over `source`: its line count and `name:line`.
fn longest_function(symbols: &[crate::extract::Symbol], source: &[u8]) -> Option<(u32, String)> {
    let newlines: Vec<usize> = memchr::memchr_iter(b'\n', source).collect();
    symbols
        .iter()
        .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
        .map(|s| {
            let first = line_of(&newlines, s.start_byte as usize);
            let last = line_of(&newlines, (s.end_byte as usize).saturating_sub(1)).max(first);
            (last - first + 1, s, first)
        })
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.2.cmp(&a.2)))
        .map(|(lines, s, first)| (lines, format!("{}:{first}", s.name)))
}

/// One candidate between the store read and the file read.
struct Candidate {
    path: RelPath,
    signals: Signals,
    outline: Option<(
        std::path::PathBuf,
        crate::store::FileEntry,
        std::sync::Arc<crate::extract::FileMapL1>,
    )>,
}

fn run_hotspots(ws: &Workspace, p: HotspotsParams, every_row: bool) -> Result<HotspotsResponse, EngineError> {
    let started = std::time::Instant::now();
    let since = p.since.unwrap_or_else(|| SINCE_DEFAULT.to_string());
    let span = parse_since(&since)?;
    let limit = match p.limit {
        Some(0) => {
            return Err(EngineError::invalid_argument(
                "`git_hotspots`: `limit` must be at least 1; omit it for the default of 20 rows.",
            ));
        }
        Some(limit) => limit.min(LIMIT_MAX),
        None => LIMIT_DEFAULT,
    } as usize;
    let scope: Option<Vec<u8>> = p.path.as_ref().map(|path| {
        let mut bytes = path.as_bytes().to_vec();
        while bytes.last() == Some(&b'/') {
            bytes.pop();
        }
        bytes
    });
    let scope = scope.as_deref().filter(|scope| !scope.is_empty() && *scope != b".");

    let window = window_of(ws, span)?;
    let mut notes = Vec::new();
    // Built before the store read below: building takes that lock itself.
    let graph = match import_graph(ws) {
        Ok(graph) => Some(graph),
        Err(error) => {
            notes.push(format!("importers not measured: {error}"));
            None
        }
    };
    let resolvable: AHashSet<&str> = graph
        .iter()
        .flat_map(|graph| graph.coverage().languages.iter())
        .filter(|language| language.resolver)
        .map(|language| language.language.as_str())
        .collect();
    if graph
        .as_ref()
        .is_some_and(|graph| graph.coverage().any_resolvable() && !graph.probes_complete())
    {
        notes.push("importers are partial: the index has not probed every file's imports yet".to_string());
    }

    let cache = ws.cache.load_full();
    let mut no_resolver: AHashSet<String> = AHashSet::new();
    let candidates: Vec<Candidate> = {
        let store = ws.store_read();
        window
            .per_path
            .iter()
            .filter(|(path, _)| in_scope(path, scope))
            .filter_map(|(path, (commits, authors))| {
                let entry = store.lookup(path)?;
                let importers = match &graph {
                    Some(graph) if resolvable.contains(entry.language.as_str()) => Some(
                        path.as_str()
                            .and_then(|path| graph.node(path))
                            .map_or(0, |node| graph.in_degree(node, Tiers::RUNTIME) as u32),
                    ),
                    Some(_) => {
                        no_resolver.insert(entry.language.clone());
                        None
                    }
                    None => None,
                };
                let outline = freshness::indexed_outline(&store, &cache, path).and_then(|(entry, l1)| {
                    indexed_file(ws, &store, "git_hotspots", path)
                        .ok()
                        .map(|file| (file, entry, l1))
                });
                Some(Candidate {
                    path: path.clone(),
                    signals: Signals {
                        commits: *commits,
                        authors: authors.len() as u32,
                        lines: entry.lines,
                        symbols: outline.as_ref().map_or(0, |(_, _, l1)| l1.symbols.len() as u32),
                        importers,
                        ..Signals::default()
                    },
                    outline,
                })
            })
            .collect()
    };
    if !no_resolver.is_empty() {
        let mut languages: Vec<String> = no_resolver.into_iter().collect();
        languages.sort();
        notes.push(format!(
            "importers not measured for {}: no import resolver reads these languages",
            languages.join(", ")
        ));
    }

    let max_bytes = ws.config.scan.max_file_bytes;
    let files: Vec<(RelPath, Signals)> = candidates
        .into_par_iter()
        .map(|candidate| {
            let mut signals = candidate.signals;
            if let Some((file, entry, l1)) = candidate.outline
                && let Ok(live) = freshness::load(&file, (&entry).into(), max_bytes)
                && live.fresh
            {
                let (lines, at) = longest_function(&l1.symbols, &live.bytes).unzip();
                signals.longest_function = Some(lines.unwrap_or(0));
                signals.longest_function_at = at;
            }
            (candidate.path, signals)
        })
        .collect();

    let files_considered = files.len() as u32;
    let (thresholds, mut rows) = rank(files, SLICE_PERCENT);
    let total = rows.len() as u32;
    if !every_row {
        rows.truncate(limit);
    }
    Ok(HotspotsResponse {
        since,
        window_start: window.start,
        window_end: window.end,
        commits: window.commits,
        files_considered,
        slice_percent: SLICE_PERCENT,
        thresholds,
        total,
        files: rows,
        truncated: window.truncated_reason.is_some(),
        truncated_reason: window.truncated_reason,
        notes,
        elapsed_us: elapsed_us(started),
    })
}

#[cfg(test)]
#[path = "hotspots_tests.rs"]
mod tests;
