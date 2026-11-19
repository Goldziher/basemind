//! `basemind overview` (#131): the shape of a repository in one call, facts only — languages,
//! top-level layout, build manifests and the entry points they declare, the largest and the most
//! imported files, HTTP routes, naming conventions and the latest commits.
//!
//! Every section is an aggregate over what the scan already holds (the file map, the resident term
//! index, the import graph, git history) except the manifests, which are configuration the code map
//! does not keep and are read from disk beside the indexed files ([`crate::engine::manifests`]).

use std::collections::BTreeMap;

use ahash::AHashMap;

use crate::engine::error::EngineError;
use crate::engine::files::{IndexedFiles, Sweep};
use crate::engine::import_graph::{ImportGraph, Tiers};
use crate::engine::manifests::{self, FileSet, Manifest, TargetKind};
use crate::engine::timing::elapsed_us;
use crate::engine::types::code::kind_to_str;
use crate::engine::types::files::SizedFile;
use crate::engine::types::git::{GitLogParams, GitLogResult};
use crate::engine::types::overview::{
    CaseShare, DirectoryRow, EntryPoint, GraphCoverage, ImportedFile, LanguageRow, NamingRow, OverviewParams,
    OverviewResponse, RecentCommit, Routes,
};
use crate::engine::workspace::Workspace;
use crate::extract::SymbolKind;
use crate::path::RelPath;

/// Rows a list section holds when the caller passes no `limit`.
const LIMIT_DEFAULT: u32 = 10;
/// Upper bound on `limit`.
const LIMIT_MAX: u32 = 100;
/// Commits `recent` lists.
const RECENT_COMMITS: u32 = 5;
/// Symbol kinds `naming` reports per language, the most frequent first.
const NAMING_KINDS_MAX: usize = 8;
/// Names a kind (or a language's files) needs before its dominant case is reported.
const NAMING_MIN_NAMES: usize = 3;

/// The sections `overview` reports, in output order.
const SECTIONS: &[&str] = &[
    "languages",
    "layout",
    "manifests",
    "entry_points",
    "largest",
    "most_imported",
    "routes",
    "naming",
    "recent",
];

/// One indexed code file, as the sections read it. Data and configuration files (`lang::is_non_code`)
/// are left out: the code map holds them only when the document tier is off.
struct FileRow {
    path: RelPath,
    language: String,
    lines: u32,
    bytes: u64,
}

impl Workspace {
    /// `overview`: the repository's shape, each section on request (all by default).
    pub(crate) fn overview(&self, p: OverviewParams) -> Result<OverviewResponse, EngineError> {
        let started = std::time::Instant::now();
        let wanted = sections(p.sections.as_deref())?;
        let want = |name: &str| wanted.contains(&name);
        let limit = match p.limit {
            Some(0) => {
                return Err(EngineError::invalid_argument(
                    "`overview`: `limit` must be at least 1; omit it for 10 rows per section.",
                ));
            }
            Some(n) => n.min(LIMIT_MAX) as usize,
            None => LIMIT_DEFAULT as usize,
        };
        let rows: Vec<FileRow> = {
            let store = self.store_read();
            store
                .index
                .files
                .iter()
                .filter(|(path, entry)| !path.is_external() && !crate::lang::is_non_code(&entry.language))
                .map(|(path, entry)| FileRow {
                    path: path.clone(),
                    language: entry.language.clone(),
                    lines: entry.lines,
                    bytes: entry.size_bytes,
                })
                .collect()
        };
        let mut omitted = BTreeMap::new();
        let mut cut = |section: &'static str, total: usize| {
            if total > limit {
                omitted.insert(section, total - limit);
            }
        };

        let languages = want("languages").then(|| {
            let all = languages(&rows);
            cut("languages", all.len());
            all.into_iter().take(limit).collect()
        });
        let layout = want("layout").then(|| {
            let all = layout(&rows);
            cut("layout", all.len());
            all.into_iter().take(limit).collect()
        });
        let found = (want("manifests") || want("entry_points")).then(|| self.manifests(&rows));
        let (entry_points, entry_points_counted) = if want("entry_points") {
            let (points, counted, total) = entry_points(found.as_deref().unwrap_or_default(), limit);
            cut("entry_points", total);
            (Some(points), Some(counted))
        } else {
            (None, None)
        };
        let manifests = want("manifests").then(|| {
            let mut all = found.unwrap_or_default();
            cut("manifests", all.len());
            all.truncate(limit);
            for manifest in &mut all {
                manifest.targets.clear();
            }
            all
        });
        let largest = want("largest").then(|| {
            cut("largest", rows.len());
            largest(&rows, limit)
        });
        let (most_imported, import_graph) = if want("most_imported") {
            let graph = crate::engine::import_graph::import_graph(self)?;
            let (files, total) = most_imported(&graph, limit);
            cut("most_imported", total);
            let coverage = GraphCoverage {
                no_resolver: graph
                    .coverage()
                    .without_resolver()
                    .filter(|language| !crate::lang::is_non_code(language))
                    .map(str::to_owned)
                    .collect(),
                complete: graph.probes_complete(),
            };
            (Some(files), Some(coverage))
        } else {
            (None, None)
        };
        let (routes, naming) = if want("routes") || want("naming") {
            let (routes, naming) = self.symbol_sections(&rows, want("naming"));
            let naming = want("naming").then(|| {
                cut("naming", naming.len());
                naming.into_iter().take(limit).collect()
            });
            (want("routes").then_some(routes), naming)
        } else {
            (None, None)
        };
        let recent = if want("recent") { self.recent_commits() } else { None };

        Ok(OverviewResponse {
            files: rows.len(),
            lines: rows.iter().map(|r| u64::from(r.lines)).sum(),
            bytes: rows.iter().map(|r| r.bytes).sum(),
            languages,
            layout,
            manifests,
            entry_points,
            entry_points_counted,
            largest,
            most_imported,
            import_graph,
            routes,
            naming,
            recent,
            omitted,
            notice: self.lifecycle_notice(),
            elapsed_us: elapsed_us(started),
        })
    }

    /// Every build manifest beside the indexed files, parsed.
    fn manifests(&self, rows: &[FileRow]) -> Vec<Manifest> {
        let files = FileSet::new(rows.iter().filter_map(|r| r.path.as_str()));
        let sweep = Sweep::new(IndexedFiles::of(self));
        let exists = |rel: &str| sweep.path(&RelPath::from(rel)).is_some_and(|file| file.is_file());
        let read = |rel: &str, max: usize| {
            let file = sweep.path(&RelPath::from(rel))?;
            read_head(&file, max)
        };
        let cx = manifests::Context {
            files: &files,
            read: &read,
            go_modules: &[],
        };
        manifests::discover(&files.directories(), &exists, &cx)
    }

    /// The `routes` and `naming` sections, from one sweep of the resident term index.
    fn symbol_sections(&self, rows: &[FileRow], naming: bool) -> (Routes, Vec<NamingRow>) {
        self.wait_map_ready();
        let language_of: AHashMap<&RelPath, &str> = rows.iter().map(|r| (&r.path, r.language.as_str())).collect();
        let mut routes = Routes::default();
        let mut tallies: BTreeMap<&str, LanguageTally> = BTreeMap::new();
        if naming {
            for row in rows {
                let tally = tallies.entry(row.language.as_str()).or_default();
                tally.file_names += 1;
                if let Some(case) = file_stem(&row.path).and_then(case_of) {
                    *tally.files.entry(case).or_default() += 1;
                }
            }
        }
        let cache = self.cache.load_full();
        for (path, terms) in cache.terms().iter() {
            let mut file_routes = 0;
            let language = language_of.get(path).copied();
            for (_, name, kind) in terms.symbols() {
                if kind == SymbolKind::Route {
                    file_routes += 1;
                    let name = String::from_utf8_lossy(name);
                    if let Some((method, _)) = name.split_once(' ') {
                        *routes.methods.entry(method.to_owned()).or_default() += 1;
                    }
                    continue;
                }
                let (true, Some(language)) = (naming, language) else {
                    continue;
                };
                let Some(case) = std::str::from_utf8(name).ok().and_then(case_of) else {
                    continue;
                };
                let tally = tallies.entry(language).or_default();
                *tally
                    .symbols
                    .entry(kind_to_str(kind))
                    .or_default()
                    .entry(case)
                    .or_default() += 1;
            }
            routes.count += file_routes;
            routes.files += usize::from(file_routes > 0);
        }
        let mut naming_rows: Vec<(usize, NamingRow)> = tallies
            .into_iter()
            .map(|(language, tally)| (tally.file_names, tally.into_row(language)))
            .filter(|(_, row)| row.files.is_some() || !row.symbols.is_empty())
            .collect();
        naming_rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.language.cmp(&b.1.language)));
        (routes, naming_rows.into_iter().map(|(_, row)| row).collect())
    }

    /// The latest commits, or `None` outside a git repository or when the history cannot be read.
    fn recent_commits(&self) -> Option<Vec<RecentCommit>> {
        self.repo.as_ref()?;
        let params = GitLogParams {
            path: None,
            query: None,
            author: None,
            symbol: None,
            line: None,
            rev: None,
            limit: Some(RECENT_COMMITS),
            cursor: None,
            max_tokens: None,
        };
        let Ok(GitLogResult::Recent(log)) = self.git_log(params) else {
            return None;
        };
        Some(
            log.commits
                .into_iter()
                .map(|c| RecentCommit {
                    sha: c.sha.chars().take(12).collect(),
                    summary: c.summary,
                    author: c.author,
                    time_unix: c.author_time_unix,
                })
                .collect(),
        )
    }
}

/// The requested sections, each a member of [`SECTIONS`]; all of them when none is named.
fn sections(list: Option<&str>) -> Result<Vec<&'static str>, EngineError> {
    let Some(list) = list.filter(|l| !l.trim().is_empty()) else {
        return Ok(SECTIONS.to_vec());
    };
    let mut wanted = Vec::new();
    for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let name = name.replace('-', "_");
        let Some(known) = SECTIONS.iter().find(|s| **s == name) else {
            return Err(EngineError::invalid_argument(format!(
                "`overview`: unknown section `{name}`; the sections are {}.",
                SECTIONS.join(", ")
            )));
        };
        wanted.push(*known);
    }
    Ok(wanted)
}

/// Up to `max` bytes of `file`, lossily decoded.
fn read_head(file: &std::path::Path, max: usize) -> Option<String> {
    use std::io::Read;

    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .ok()?
        .take(max as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn languages(rows: &[FileRow]) -> Vec<LanguageRow> {
    let mut by: BTreeMap<&str, LanguageRow> = BTreeMap::new();
    for row in rows {
        let entry = by.entry(row.language.as_str()).or_insert_with(|| LanguageRow {
            language: row.language.clone(),
            files: 0,
            lines: 0,
            bytes: 0,
        });
        entry.files += 1;
        entry.lines += u64::from(row.lines);
        entry.bytes += row.bytes;
    }
    let mut all: Vec<LanguageRow> = by.into_values().collect();
    all.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| b.files.cmp(&a.files)));
    all
}

fn layout(rows: &[FileRow]) -> Vec<DirectoryRow> {
    let mut by: BTreeMap<&str, (usize, u64, BTreeMap<&str, u64>)> = BTreeMap::new();
    for row in rows {
        let path = row.path.as_str().unwrap_or("");
        let top = path.split_once('/').map_or(".", |(top, _)| top);
        let entry = by.entry(top).or_default();
        entry.0 += 1;
        entry.1 += u64::from(row.lines);
        *entry.2.entry(row.language.as_str()).or_default() += u64::from(row.lines);
    }
    let mut all: Vec<DirectoryRow> = by
        .into_iter()
        .map(|(path, (files, lines, languages))| DirectoryRow {
            path: path.to_owned(),
            files,
            lines,
            language: dominant(&languages).unwrap_or_default().to_owned(),
        })
        .collect();
    all.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.path.cmp(&b.path)));
    all
}

/// The key with the largest count; ties go to the first key.
fn dominant<'k>(counts: &BTreeMap<&'k str, u64>) -> Option<&'k str> {
    counts
        .iter()
        .fold(None, |best: Option<(&str, u64)>, (k, n)| match best {
            Some((_, top)) if top >= *n => best,
            _ => Some((k, *n)),
        })
        .map(|(k, _)| k)
}

fn largest(rows: &[FileRow], limit: usize) -> Vec<SizedFile> {
    let mut sorted: Vec<&FileRow> = rows.iter().collect();
    sorted.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.path.cmp(&b.path)));
    sorted
        .into_iter()
        .take(limit)
        .map(|r| SizedFile {
            path: r.path.clone(),
            lines: r.lines,
            bytes: r.bytes,
        })
        .collect()
}

/// The `limit` files with the most runtime importers (ties by path), and how many files have any.
pub(crate) fn most_imported(graph: &ImportGraph, limit: usize) -> (Vec<ImportedFile>, usize) {
    let mut ranked: Vec<(usize, u32)> = (0..graph.len() as u32)
        .map(|node| (graph.in_degree(node, Tiers::RUNTIME), node))
        .filter(|(importers, _)| *importers > 0)
        .collect();
    let total = ranked.len();
    // Node ids follow path order, so the id breaks ties by path.
    ranked.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let files = ranked
        .into_iter()
        .take(limit)
        .map(|(importers, node)| ImportedFile {
            path: graph.path(node).clone(),
            importers,
        })
        .collect();
    (files, total)
}

/// Targets of every manifest: tests, benchmarks and examples counted, the rest listed up to `limit`.
/// Returns the listed targets, the counts, and how many targets were listable.
fn entry_points(found: &[Manifest], limit: usize) -> (Vec<EntryPoint>, BTreeMap<&'static str, usize>, usize) {
    let mut points = Vec::new();
    let mut counts = BTreeMap::new();
    let mut total = 0;
    for manifest in found {
        for target in &manifest.targets {
            let counted = match target.kind {
                TargetKind::Test => Some("test"),
                TargetKind::Bench => Some("bench"),
                TargetKind::Example => Some("example"),
                _ => None,
            };
            if let Some(kind) = counted {
                *counts.entry(kind).or_default() += 1;
                continue;
            }
            total += 1;
            if points.len() < limit {
                points.push(EntryPoint {
                    manifest: manifest.path.clone(),
                    target: target.clone(),
                });
            }
        }
    }
    (points, counts, total)
}

/// Case counts of one language's names.
#[derive(Default)]
struct LanguageTally {
    file_names: usize,
    files: BTreeMap<&'static str, usize>,
    symbols: BTreeMap<&'static str, BTreeMap<&'static str, usize>>,
}

impl LanguageTally {
    fn into_row(self, language: &str) -> NamingRow {
        let mut kinds: Vec<(&'static str, CaseShare)> = self
            .symbols
            .into_iter()
            .filter_map(|(kind, cases)| Some((kind, share(&cases)?)))
            .collect();
        kinds.sort_by(|a, b| b.1.names.cmp(&a.1.names).then_with(|| a.0.cmp(b.0)));
        kinds.truncate(NAMING_KINDS_MAX);
        NamingRow {
            language: language.to_owned(),
            files: share(&self.files),
            symbols: kinds.into_iter().collect(),
        }
    }
}

/// The dominant case and its share, when at least [`NAMING_MIN_NAMES`] names have a case.
fn share(cases: &BTreeMap<&'static str, usize>) -> Option<CaseShare> {
    let names: usize = cases.values().sum();
    if names < NAMING_MIN_NAMES {
        return None;
    }
    let (case, top) = cases
        .iter()
        .fold(None, |best: Option<(&'static str, usize)>, (case, n)| match best {
            Some((_, top)) if top >= *n => best,
            _ => Some((case, *n)),
        })?;
    Some(CaseShare {
        case,
        percent: u8::try_from(top * 100 / names).unwrap_or(100),
        names,
    })
}

/// A file name without its extensions (`index.test.ts` is `index`).
fn file_stem(path: &RelPath) -> Option<&str> {
    let name = path.as_str()?.rsplit('/').next()?;
    name.split('.').next().filter(|s| !s.is_empty())
}

/// The case convention `name` follows, or `None` when it shows none: a single lower-case word, a
/// single upper-case word, or a name mixing conventions or holding other characters.
pub(crate) fn case_of(name: &str) -> Option<&'static str> {
    let name = name.trim_start_matches('_').trim_end_matches('_');
    let mut chars = name.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    let has_lower = name.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = name.chars().any(|c| c.is_ascii_uppercase());
    let underscore = name.contains('_');
    let hyphen = name.contains('-');
    match (underscore, hyphen) {
        (true, true) => None,
        (false, true) => (!has_upper).then_some("kebab-case"),
        (true, false) if !has_upper => Some("snake_case"),
        (true, false) if !has_lower => Some("SCREAMING_SNAKE_CASE"),
        (true, false) => None,
        (false, false) if !has_upper || !has_lower => None,
        (false, false) if first.is_ascii_uppercase() => Some("PascalCase"),
        (false, false) => Some("camelCase"),
    }
}

#[cfg(test)]
#[path = "overview_tests.rs"]
mod tests;
