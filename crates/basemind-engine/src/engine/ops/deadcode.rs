//! `basemind code deadcode` (#123): files no entry point reaches through the import graph,
//! definitions nothing else names, and imports that resolve to nothing; each finding with a
//! confidence tier and its reason, and a coverage note per language.
//!
//! - **Files** ([`super::deadcode_files`]): entry points are the build manifests' targets
//!   ([`crate::engine::manifests`]), the `<script>`s of a package's `index.html`, `[analysis]
//!   entry_points` and each language's conventions; reachability follows runtime imports and Rust
//!   `mod` declarations. Only languages with a resolver are judged.
//! - **Symbols** ([`super::deadcode_symbols`]): judged by name over one text pass of the indexed
//!   code, so "unreferenced" is a lower bound on use, and the tier says how much the index can see.
//! - **Imports**: the unresolved specifiers the file-edge rows keep (ADR-0047 decision 4).
//!
//! A report that could not judge a requested kind in full says why in `incomplete`, so a CI gate
//! never passes on a report that examined nothing.

use std::collections::BTreeMap;
use std::ops::Range;

use ahash::{AHashMap, AHashSet};

use super::deadcode_files::{
    FILE_LANGUAGES, FileFacts, Judged, PackageCaps, cap_by_package, conventional_entry, crate_modules, is_test_data,
    manifest_entries, manifest_prefixes, named_by_module_string, nearest, surface_files, unreachable,
};
use super::deadcode_symbols::{Candidate, Judge, Names, SYMBOL_LANGUAGES, ScanFile, candidates_of, judge, text_pass};
use super::graph_report::{coverage, notes};
use crate::engine::error::EngineError;
use crate::engine::files::{IndexedFiles, Sweep};
use crate::engine::import_graph::{ImportGraph, import_graph};
use crate::engine::l1_cache::FileMeta;
use crate::engine::manifests::{Ecosystem, Manifest, TargetKind, parent_dir};
use crate::engine::timing::elapsed_us;
use crate::engine::types::deadcode::{
    DeadFile, DeadImport, DeadSymbol, DeadcodeCoverage, DeadcodeParams, DeadcodeResponse, Tier, TierCounts,
};
use crate::engine::workspace::Workspace;
use crate::path::RelPath;

/// Findings listed per kind when the caller passes no `limit`.
const LIMIT_DEFAULT: u32 = 100;
/// Upper bound on `limit`.
const LIMIT_MAX: u32 = 10_000;
/// The kinds of finding, in output order.
const KINDS: &[&str] = &["files", "symbols", "imports"];
/// Python files a tool runs, whose names it may call by their spelling (`pytest_configure` in
/// `conftest.py`, `application` in `wsgi.py`).
const PYTHON_LOADER_FILES: &[&str] = &[
    "conftest.py",
    "conf.py",
    "noxfile.py",
    "fabfile.py",
    "setup.py",
    "manage.py",
    "wsgi.py",
    "asgi.py",
];
/// Bytes of an `index.html` read for its `<script>` entries.
const INDEX_HTML_MAX: u64 = 1024 * 1024;

/// A request's filters, checked.
struct Request {
    scope: Option<String>,
    kinds: Vec<&'static str>,
    tier: Tier,
    limit: usize,
}

impl Request {
    fn parse(p: DeadcodeParams) -> Result<Self, EngineError> {
        let kinds = match p.kind.as_deref() {
            None => KINDS.to_vec(),
            Some(kind) => vec![*KINDS.iter().find(|k| **k == kind).ok_or_else(|| {
                EngineError::invalid_argument(format!(
                    "`code deadcode`: unknown kind `{kind}`; pass `files`, `symbols` or `imports`"
                ))
            })?],
        };
        let tier = match p.tier.as_deref() {
            None => Tier::Low,
            Some(tier) => Tier::parse(tier).ok_or_else(|| {
                EngineError::invalid_argument(format!(
                    "`code deadcode`: unknown tier `{tier}`; pass `high`, `medium` or `low`"
                ))
            })?,
        };
        let limit = p.limit.unwrap_or(LIMIT_DEFAULT);
        if limit == 0 || limit > LIMIT_MAX {
            return Err(EngineError::invalid_argument(format!(
                "`code deadcode`: `limit` is 1 to {LIMIT_MAX}"
            )));
        }
        let scope = p
            .path
            .map(|path| path.trim_end_matches('/').to_string())
            .filter(|path| !path.is_empty() && path != ".");
        Ok(Self {
            scope,
            kinds,
            tier,
            limit: limit as usize,
        })
    }

    fn wants(&self, kind: &str) -> bool {
        self.kinds.contains(&kind)
    }

    fn in_scope(&self, path: &RelPath) -> bool {
        self.scope.as_deref().is_none_or(|scope| under(path, scope))
    }
}

/// Whether `path` is `scope` or a file under the directory `scope`.
fn under(path: &RelPath, scope: &str) -> bool {
    let path = path.as_bytes();
    let scope = scope.as_bytes();
    path == scope || (path.starts_with(scope) && path.get(scope.len()) == Some(&b'/'))
}

/// The package directories of the manifests, as path prefixes, by what each rule reads.
struct Packages {
    crates: Vec<String>,
    go_modules: Vec<String>,
    npm: Vec<String>,
    python_named: Vec<String>,
    js_build_output: Vec<String>,
    cargo_libs: Vec<String>,
}

impl Packages {
    fn of(manifests: &[Manifest]) -> Self {
        let all = |_: &Manifest| true;
        Self {
            crates: manifest_prefixes(manifests, Ecosystem::Cargo, all),
            go_modules: manifest_prefixes(manifests, Ecosystem::Go, all),
            npm: manifest_prefixes(manifests, Ecosystem::Npm, all),
            python_named: manifest_prefixes(manifests, Ecosystem::Python, |m| m.name.is_some()),
            js_build_output: manifest_prefixes(manifests, Ecosystem::Npm, |m| {
                m.targets.iter().any(|t| {
                    matches!(t.kind, TargetKind::Main | TargetKind::Export)
                        && t.path.is_none()
                        && !matches!(t.name.as_deref(), Some("types" | "typings"))
                })
            }),
            cargo_libs: manifest_prefixes(manifests, Ecosystem::Cargo, |m| {
                m.targets.iter().any(|t| t.kind == TargetKind::Lib)
            }),
        }
    }
}

impl Workspace {
    /// `basemind code deadcode`: unreachable files, unreferenced symbols and unresolved imports.
    pub fn code_deadcode(&self, p: DeadcodeParams) -> Result<DeadcodeResponse, EngineError> {
        let started = std::time::Instant::now();
        let request = Request::parse(p)?;
        let graph = import_graph(self)?;
        self.wait_map_ready();
        let cache = self.cache.load_full();
        let files: Vec<(&RelPath, &FileMeta)> = cache
            .file_metas()
            .filter(|(path, meta)| {
                !path.is_external()
                    && !path.as_bytes().starts_with(b".basemind/")
                    && !crate::lang::is_non_code(&meta.language)
            })
            .collect();
        let index_of: AHashMap<&RelPath, u32> = files
            .iter()
            .enumerate()
            .map(|(at, (path, _))| (*path, u32::try_from(at).unwrap_or(u32::MAX)))
            .collect();
        let manifests = self.manifests_of(files.iter().filter_map(|(path, _)| path.as_str()));
        let packages = Packages::of(&manifests);
        let entries = manifest_entries(&manifests, files.iter().map(|(path, _)| *path));
        let entry_set: AHashSet<&RelPath> = entries.iter().map(|(path, _)| path).collect();
        let (configured, unmatched) =
            configured_entries(&self.config.analysis.entry_points, files.iter().map(|(path, _)| *path))?;
        let html = html_entries(&self.root, &packages.npm, &index_of);
        // The names a manifest target calls by spelling: `pkg.cli:run` names `run` in `pkg/cli.py`.
        let target_symbols: AHashSet<(&RelPath, &str)> = manifests
            .iter()
            .flat_map(|m| &m.targets)
            .filter_map(|t| Some((t.path.as_ref()?, t.symbol.as_deref()?)))
            .collect();
        let entry_files: AHashSet<&RelPath> = files
            .iter()
            .map(|(path, _)| *path)
            .filter(|path| {
                entry_set.contains(path)
                    || configured.contains(path)
                    || html.contains(path)
                    || path
                        .as_str()
                        .is_some_and(|p| PYTHON_LOADER_FILES.contains(&p.rsplit('/').next().unwrap_or(p)))
            })
            .collect();
        let surfaces: Vec<RelPath> = entries
            .iter()
            .filter(|(_, surface)| *surface)
            .map(|(path, _)| path.clone())
            .collect();
        let surface = surface_files(&graph, &surfaces);
        let in_surface = |path: &RelPath, language: &str| {
            surface.contains(path) || (language == "rust" && in_cargo_library(path, &packages.cargo_libs))
        };
        let view = self.store_read().read_index_view();
        let mut notes = notes(&graph);
        for pattern in unmatched {
            notes.push(format!(
                "`[analysis] entry_points` pattern `{pattern}` matched no indexed code file"
            ));
        }

        let mut names = Names::default();
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut ranges: AHashMap<u32, Range<usize>> = AHashMap::new();
        if request.wants("symbols") {
            cache.for_each(|path, l1| {
                let Some(&file) = index_of.get(path) else {
                    return;
                };
                let Some(language) = SYMBOL_LANGUAGES
                    .iter()
                    .find(|language| **language == &*files[file as usize].1.language)
                else {
                    return;
                };
                if request.in_scope(path) && !is_test_data(path) {
                    let start = candidates.len();
                    candidates_of(file, &path.to_str_lossy(), language, l1, &mut names, &mut candidates);
                    if candidates.len() > start {
                        ranges.insert(file, start..candidates.len());
                    }
                }
            });
        }
        let edge_rows = if request.wants("imports") {
            view.as_ref().map(|view| view.file_edges()).unwrap_or_default()
        } else {
            Vec::new()
        };
        let specifiers: AHashMap<&RelPath, &[String]> = edge_rows
            .iter()
            .filter(|(path, edges)| {
                !edges.unresolved_specifiers.is_empty() && request.in_scope(path) && !is_test_data(path)
            })
            .map(|(path, edges)| (path, edges.unresolved_specifiers.as_slice()))
            .collect();
        // Without candidate names, only the files whose entry conventions read their text, or whose
        // unresolved imports need a line, are read.
        let needs_facts = |language: &str| {
            matches!(language, "go" | "python" | "java") || (language == "rust" && request.wants("files"))
        };
        let scan_files: Vec<ScanFile<'_>> = files
            .iter()
            .enumerate()
            .filter(|(_, (path, meta))| {
                !names.is_empty() || needs_facts(&meta.language) || specifiers.contains_key(path)
            })
            .map(|(at, &(path, meta))| ScanFile {
                path,
                meta,
                candidates: u32::try_from(at).ok().and_then(|at| ranges.remove(&at)).unwrap_or(0..0),
                specifiers: specifiers.get(path).copied().unwrap_or_default(),
                entry_file: entry_files.contains(path),
            })
            .collect();
        let sweep = Sweep::new(IndexedFiles::of(self));
        let outline = |path: &RelPath| cache.get(path);
        let scan = text_pass(&sweep, &outline, &scan_files, &candidates, &names);
        drop(scan_files);
        let mut incomplete = Vec::new();

        let dead_files = request.wants("files").then(|| {
            let mut module_strings: AHashSet<&str> = AHashSet::new();
            for facts in scan.facts.values() {
                for string in &facts.module_strings {
                    module_strings.insert(string);
                    // `"app.middleware.Auth"` names a class of the module `app.middleware`.
                    if let Some((module, _)) = string.rsplit_once('.')
                        && module.contains('.')
                    {
                        module_strings.insert(module);
                    }
                }
            }
            let judged: Vec<Judged<'_>> = files
                .iter()
                .filter(|(path, meta)| judged_language(&graph, &meta.language, path, &packages))
                .map(|&(path, meta)| {
                    let path_str = path.to_str_lossy();
                    let entry = if entry_set.contains(path) {
                        Some("a manifest target")
                    } else if configured.contains(path) {
                        Some("listed in `[analysis] entry_points`")
                    } else if html.contains(path) {
                        Some("a `<script>` of its package's `index.html`")
                    } else {
                        let facts = scan.facts.get(path);
                        conventional_entry(&path_str, &meta.language, facts.unwrap_or(&FileFacts::default())).or_else(
                            || {
                                (&*meta.language == "python" && named_by_module_string(&path_str, &module_strings))
                                    .then_some(
                                        "its dotted module path is spelled in a string: a framework loads it by name",
                                    )
                            },
                        )
                    };
                    Judged {
                        path,
                        language: &meta.language,
                        entry,
                    }
                })
                .collect();
            let mut found = unreachable(&graph, &judged);
            found.retain(|file| request.in_scope(&file.path));
            let crate_modules = crate_modules(&packages.crates, scan.facts.iter());
            cap_by_package(
                &mut found,
                &PackageCaps {
                    crates: &packages.crates,
                    crate_modules: &crate_modules,
                    python_packages: &packages.python_named,
                    js_build_output: &packages.js_build_output,
                },
            );
            cap_partial_graph(&mut found, graph.probes_complete());
            found
        });

        let dead_symbols = request.wants("symbols").then(|| {
            let others: Vec<[i64; 3]> = scan
                .occurrences
                .iter()
                .zip(&scan.definitions)
                .map(|(&[code, strings, comments], &defined)| {
                    [i64::from(code) - i64::from(defined), i64::from(strings), i64::from(comments)]
                })
                .collect();
            let build_output = |path: &RelPath| nearest(&path.to_str_lossy(), &packages.js_build_output).is_some();
            let judge_with = Judge {
                surface: &in_surface,
                others: &others,
                surface_partial: !graph.probes_complete(),
                build_output: &build_output,
                unreadable: scan.unreadable,
            };
            let mut stale = 0usize;
            let found: Vec<DeadSymbol> = candidates
                .iter()
                .enumerate()
                .filter_map(|(at, candidate)| {
                    let Some(local) = scan.locals[at].as_ref() else {
                        stale += 1;
                        return None;
                    };
                    let path = files[candidate.file as usize].0;
                    let name = names.name(candidate.name);
                    if target_symbols.contains(&(path, name)) {
                        return None;
                    }
                    judge(candidate, path, name, local, scan.facts.get(path), &judge_with)
                })
                .collect();
            if stale > 0 {
                notes.push(format!(
                    "{stale} definition(s) in files changed or removed since the scan were not judged: run `basemind scan`"
                ));
            }
            if scan.edited > 0 {
                notes.push(format!(
                    "{} file(s) changed since the scan: every name they spell counts as a use, so a finding may be missing",
                    scan.edited
                ));
            }
            if scan.unreadable > 0 {
                notes.push(format!(
                    "{} indexed file(s) could not be read (deleted, or grown past `scan.max_file_bytes`, since the scan): symbol findings are at most `medium`",
                    scan.unreadable
                ));
            }
            if stale > 0 || scan.unreadable > 0 {
                incomplete.push(format!(
                    "symbols: {} file(s) changed, removed or unreadable since the scan; run `basemind scan`",
                    scan.edited + scan.unreadable
                ));
            }
            found
        });

        let dead_imports = request.wants("imports").then(|| {
            unresolved_imports(
                &self.root,
                &edge_rows,
                &files,
                &scan.specifier_lines,
                &request,
                &packages.npm,
                &mut notes,
            )
        });

        let coverage_rows = language_coverage(&graph, &files);
        let notice = self.lifecycle_notice();
        if let Some(notice) = &notice {
            incomplete.insert(
                0,
                format!("the index is not ready (`{}`): retry once it is", notice.state),
            );
        }
        let graph_kinds: Vec<&str> = ["files", "imports"]
            .into_iter()
            .filter(|kind| request.wants(kind))
            .collect();
        if !graph_kinds.is_empty() {
            let kinds = graph_kinds.join(" and ");
            if !coverage_rows.iter().any(|row| row.analysed.contains(&"files")) {
                incomplete.push(format!(
                    "{kinds}: no indexed language has an import resolver in this build, so none was analysed; pass `--kind symbols` to judge symbols alone"
                ));
            } else if !graph.probes_complete() {
                incomplete.push(format!(
                    "{kinds}: the resolve pass has not covered the whole index yet; wait for it to finish (`basemind status`)"
                ));
            }
        }

        let mut counts = BTreeMap::new();
        let mut omitted = BTreeMap::new();
        let files_out = dead_files.map(|found| {
            finish(
                "files",
                found,
                |f| (f.tier, &f.path, 0),
                &request,
                &mut counts,
                &mut omitted,
            )
        });
        let symbols_out = dead_symbols.map(|found| {
            finish(
                "symbols",
                found,
                |s| (s.tier, &s.path, s.line),
                &request,
                &mut counts,
                &mut omitted,
            )
        });
        let imports_out = dead_imports.map(|found| {
            finish(
                "imports",
                found,
                |i| (i.tier, &i.path, i.line.unwrap_or(0)),
                &request,
                &mut counts,
                &mut omitted,
            )
        });
        let declared: AHashSet<&RelPath> = entry_set
            .iter()
            .copied()
            .chain(configured.iter().copied())
            .chain(html.iter().copied())
            .collect();
        Ok(DeadcodeResponse {
            counts,
            files: files_out,
            symbols: symbols_out,
            imports: imports_out,
            omitted,
            entry_points: declared.len(),
            coverage: coverage_rows,
            graph: coverage(&graph),
            notes,
            incomplete,
            notice,
            elapsed_us: elapsed_us(started),
        })
    }
}

/// The files `[analysis] entry_points` names, and the patterns that matched no indexed code file.
/// The globs were checked when the config loaded, so a failure here is a config the loader did not
/// validate.
fn configured_entries<'a, 'g>(
    globs: &'g [String],
    files: impl Iterator<Item = &'a RelPath>,
) -> Result<(AHashSet<&'a RelPath>, Vec<&'g str>), EngineError> {
    if globs.is_empty() {
        return Ok((AHashSet::new(), Vec::new()));
    }
    let sets = globs
        .iter()
        .map(|glob| {
            crate::config::rules::compile_patterns([glob.as_str()]).map_err(|error| {
                EngineError::invalid_argument(format!("invalid glob in `[analysis] entry_points`: {error}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut matched = vec![false; sets.len()];
    let mut out = AHashSet::new();
    for path in files {
        let text = path.to_str_lossy();
        for (at, set) in sets.iter().enumerate() {
            if set.is_match(&*text) {
                matched[at] = true;
                out.insert(path);
            }
        }
    }
    let unmatched = globs
        .iter()
        .zip(matched)
        .filter(|(_, matched)| !matched)
        .map(|(glob, _)| glob.as_str())
        .collect();
    Ok((out, unmatched))
}

/// The indexed files the `<script src>` tags of each package's `index.html` name (a Vite app's
/// `/src/main.tsx`), keyed to themselves. A `/`-rooted source is relative to the package.
fn html_entries<'a>(
    root: &std::path::Path,
    packages: &[String],
    index_of: &AHashMap<&'a RelPath, u32>,
) -> AHashSet<&'a RelPath> {
    let mut out = AHashSet::new();
    for prefix in packages {
        let file = root.join(format!("{prefix}index.html"));
        let Ok(Some(bytes)) = crate::engine::freshness::read_bounded(&file, INDEX_HTML_MAX) else {
            continue;
        };
        let html = String::from_utf8_lossy(&bytes);
        for tag in html.split("<script").skip(1) {
            let tag = tag.split('>').next().unwrap_or_default();
            let Some(src) = ["src=\"", "src='"].iter().find_map(|open| {
                let at = tag.find(open)? + open.len();
                let quote = open.as_bytes()[open.len() - 1] as char;
                tag[at..].split(quote).next()
            }) else {
                continue;
            };
            let src = src.split(['?', '#']).next().unwrap_or_default();
            let rel = src.trim_start_matches('/').trim_start_matches("./");
            let path = RelPath::from(format!("{prefix}{rel}"));
            if let Some((key, _)) = index_of.get_key_value(&path) {
                out.insert(*key);
            }
        }
    }
    out
}

/// A missing edge reads as an unreachable file, so until the resolve pass covers the whole index a
/// file finding is at most `low`.
fn cap_partial_graph(found: &mut [DeadFile], complete: bool) {
    if complete {
        return;
    }
    for file in found {
        file.tier = Tier::Low;
        file.reason
            .push_str("; the resolve pass has not covered the whole index yet");
    }
}

/// Keep the findings at or above the requested tier, most sure first and then by place; count them
/// per tier and cut the list at `limit`.
fn finish<T>(
    kind: &'static str,
    mut found: Vec<T>,
    key: impl Fn(&T) -> (Tier, &RelPath, u32),
    request: &Request,
    counts: &mut BTreeMap<&'static str, TierCounts>,
    omitted: &mut BTreeMap<&'static str, usize>,
) -> Vec<T> {
    found.retain(|finding| key(finding).0 >= request.tier);
    found.sort_by(|a, b| {
        let (ta, pa, la) = key(a);
        let (tb, pb, lb) = key(b);
        tb.cmp(&ta).then_with(|| pa.cmp(pb)).then(la.cmp(&lb))
    });
    let mut tally = TierCounts::default();
    for finding in &found {
        tally.add(key(finding).0);
    }
    counts.insert(kind, tally);
    if found.len() > request.limit {
        omitted.insert(kind, found.len() - request.limit);
        found.truncate(request.limit);
    }
    found
}

/// Whether the reachability walk judges `path`: its language has a resolver in this build, and a
/// Rust or Go file sits in a crate or module the resolver can place it in.
fn judged_language(graph: &ImportGraph, language: &str, path: &RelPath, packages: &Packages) -> bool {
    if !FILE_LANGUAGES.contains(&language)
        || !graph
            .coverage()
            .languages
            .iter()
            .any(|known| known.language == language && known.resolver)
    {
        return false;
    }
    let anchors = match language {
        "rust" => &packages.crates,
        "go" => &packages.go_modules,
        _ => return true,
    };
    nearest(&path.to_str_lossy(), anchors).is_some()
}

/// Whether a Rust file is library code by Cargo's layout: under `src/` of a package with a library,
/// and neither its binary root nor under `src/bin/`. Stands in for the module tree when no Rust
/// resolver is compiled in.
fn in_cargo_library(path: &RelPath, prefixes: &[String]) -> bool {
    let path = path.to_str_lossy();
    prefixes.iter().any(|prefix| {
        path.strip_prefix(prefix.as_str())
            .and_then(|rest| rest.strip_prefix("src/"))
            .is_some_and(|rest| rest != "main.rs" && !rest.starts_with("bin/"))
    })
}

/// The unresolved specifiers the file-edge rows keep, each with its tier.
fn unresolved_imports(
    root: &std::path::Path,
    rows: &[(RelPath, crate::index::keys::FileEdges)],
    files: &[(&RelPath, &FileMeta)],
    lines: &AHashMap<RelPath, Vec<Option<u32>>>,
    request: &Request,
    npm_packages: &[String],
    notes: &mut Vec<String>,
) -> Vec<DeadImport> {
    let language_of: AHashMap<&RelPath, &str> = files.iter().map(|(path, meta)| (*path, &*meta.language)).collect();
    let mut out = Vec::new();
    let mut unlisted = 0u64;
    let mut unindexed = 0usize;
    for (path, edges) in rows {
        if edges.unresolved == 0 || !request.in_scope(path) || is_test_data(path) {
            continue;
        }
        unlisted += u64::from(edges.unresolved).saturating_sub(edges.unresolved_specifiers.len() as u64);
        let language = language_of.get(path).copied().unwrap_or("");
        let file_lines = lines.get(path);
        let package = nearest(&path.to_str_lossy(), npm_packages).map(str::to_string);
        for (at, specifier) in edges.unresolved_specifiers.iter().enumerate() {
            if names_unindexed_file(root, path, specifier, package.as_deref()) {
                unindexed += 1;
                continue;
            }
            let (tier, reason) = import_tier(language, specifier);
            out.push(DeadImport {
                path: path.clone(),
                line: file_lines.and_then(|lines| lines.get(at).copied().flatten()),
                specifier: specifier.clone(),
                language: language.to_string(),
                tier,
                reason: reason.to_string(),
            });
        }
    }
    if unindexed > 0 {
        notes.push(format!(
            "{unindexed} unresolved import(s) name a file on disk the index does not hold (a stylesheet, an asset, a `?raw` or `?worker` import) and are not listed"
        ));
    }
    if unlisted > 0 {
        notes.push(format!(
            "{unlisted} more unresolved import(s) are counted but not listed: the index keeps {} specifiers per file",
            crate::index::keys_probe::MAX_UNRESOLVED_SPECIFIERS
        ));
    }
    out
}

/// Index files a directory import resolves to.
const DIRECTORY_INDEX: &[&str] = &[
    "index.js",
    "index.mjs",
    "index.cjs",
    "index.jsx",
    "index.ts",
    "index.tsx",
    "index.css",
    "package.json",
];

/// Whether `specifier` of `importer` names a file on disk that the index does not hold
/// (`./styles.module.css`, `./icon.svg?react`, `/vite.svg` served from the package's `public/`):
/// not a broken import. A query (`?raw`) or fragment is dropped first; a directory counts only
/// when it holds an index file.
fn names_unindexed_file(root: &std::path::Path, importer: &RelPath, specifier: &str, package: Option<&str>) -> bool {
    let specifier = specifier.split(['?', '#']).next().unwrap_or_default();
    let exists = |rel: &str| {
        let mut parts: Vec<&str> = Vec::new();
        for part in rel.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop()?;
                }
                part => parts.push(part),
            }
        }
        let target = root.join(parts.join("/"));
        if !target.starts_with(root) {
            return None;
        }
        let meta = std::fs::symlink_metadata(&target).ok()?;
        (meta.is_file() || DIRECTORY_INDEX.iter().any(|index| target.join(index).is_file())).then_some(())
    };
    if let Some(rooted) = specifier.strip_prefix('/') {
        let package = package.unwrap_or_default();
        return [
            rooted.to_string(),
            format!("{package}{rooted}"),
            format!("{package}public/{rooted}"),
        ]
        .iter()
        .any(|rel| exists(rel).is_some());
    }
    if !(specifier.starts_with("./") || specifier.starts_with("../")) {
        return false;
    }
    let importer = importer.to_str_lossy();
    exists(&format!("{}/{specifier}", parent_dir(&importer))).is_some()
}

/// How sure an unresolved import is to be broken, by what its language's resolver can see.
pub(super) fn import_tier(language: &str, specifier: &str) -> (Tier, &'static str) {
    match language {
        "javascript" | "typescript" | "tsx" if specifier.starts_with('/') => (
            Tier::Medium,
            "root-relative: the bundler's root is not known, and no file of the repository or its `public/` matches it",
        ),
        "javascript" | "typescript" | "tsx" if specifier.starts_with('.') => {
            (Tier::High, "a relative import of a file that does not exist")
        }
        "javascript" | "typescript" | "tsx" => (Tier::Medium, "an alias or path the resolver does not know"),
        "python" => (
            Tier::Medium,
            "no module file matches it; a namespace package or a generated module would not be seen",
        ),
        "go" => (Tier::Medium, "no package directory of the module matches it"),
        "rust" => (
            Tier::Low,
            "no module of the crate matches it; `pub use` re-exports and macro-generated modules under-resolve",
        ),
        _ => (Tier::Low, "no file of the repository matches it"),
    }
}

/// Per indexed code language: its files, the kinds analysed and what the analysis cannot see.
fn language_coverage(graph: &ImportGraph, files: &[(&RelPath, &FileMeta)]) -> Vec<DeadcodeCoverage> {
    let mut by: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, meta) in files {
        *by.entry(&meta.language).or_default() += 1;
    }
    let resolver = |language: &str| {
        graph
            .coverage()
            .languages
            .iter()
            .any(|known| known.language == language && known.resolver)
    };
    by.into_iter()
        .map(|(language, count)| {
            let mut analysed = Vec::new();
            let resolved = resolver(language);
            if resolved && FILE_LANGUAGES.contains(&language) {
                analysed.push("files");
            }
            if SYMBOL_LANGUAGES.contains(&language) {
                analysed.push("symbols");
            }
            if resolved {
                analysed.push("imports");
            }
            let note = language_note(language, resolved, analysed.is_empty());
            DeadcodeCoverage {
                language: language.to_string(),
                files: count,
                analysed,
                note,
            }
        })
        .collect()
}

fn language_note(language: &str, resolved: bool, nothing: bool) -> Option<String> {
    if nothing {
        return Some("not analysed".to_string());
    }
    let what = match language {
        "rust" => {
            "`include!`, `#[cfg_attr(…, path)]` and modules declared inside a macro are not followed: a file its crate may \
             declare that way is at most `low`; a `pub` item of a library crate counts as exported"
        }
        "go" => "build tags are ignored; a capitalised name outside `package main` counts as exported",
        "python" => {
            "modules loaded by name are not seen unless a string spells their dotted path; a public module of a named \
             package and a public module-level name are at most `low` unless `__all__` leaves it out"
        }
        "javascript" | "typescript" | "tsx" => {
            "dynamic `import()` is not seen; an `export` reached from a package's `main`, `exports`, `source`, `module` or \
             `types` counts as exported, and a package whose entry is a build output with no indexed source is at most `low`"
        }
        "java" => {
            "classes of one package use each other without an import, so a file finding is at most `low`; a `public` or \
             `protected` member counts as exported, and JUnit tests are entry points"
        }
        _ => return None,
    };
    Some(if resolved || !FILE_LANGUAGES.contains(&language) {
        what.to_string()
    } else {
        format!("no import resolver in this build, so files and imports are not analysed; {what}")
    })
}

#[cfg(test)]
#[path = "deadcode_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "deadcode_lang_tests.rs"]
mod lang_tests;
