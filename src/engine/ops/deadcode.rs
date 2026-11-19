//! `basemind code deadcode` (#123): files no entry point reaches through the import graph,
//! definitions nothing else names, and imports that resolve to nothing; each finding with a
//! confidence tier and its reason, and a coverage note per language.
//!
//! - **Files** ([`super::deadcode_files`]): entry points are the build manifests' targets
//!   ([`crate::engine::manifests`]) and each language's conventions; reachability follows runtime
//!   imports and Rust `mod` declarations. Only languages with a resolver are judged.
//! - **Symbols** ([`super::deadcode_symbols`]): judged by name over one text pass of the indexed
//!   code, so "unreferenced" is a lower bound on use, and the tier says how much the index can see.
//! - **Imports**: the unresolved specifiers the file-edge rows keep (ADR-0047 decision 4).

use std::collections::BTreeMap;

use ahash::{AHashMap, AHashSet};

use super::deadcode_files::{
    FILE_LANGUAGES, FileFacts, Judged, conventional_entry, is_test_data, manifest_entries, surface_files, unreachable,
};
use super::deadcode_symbols::{
    Candidate, Judge, SYMBOL_LANGUAGES, ScanFile, candidates_of, definitions, judge, text_pass,
};
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

impl Workspace {
    /// `basemind code deadcode`: unreachable files, unreferenced symbols and unresolved imports.
    pub(crate) fn code_deadcode(&self, p: DeadcodeParams) -> Result<DeadcodeResponse, EngineError> {
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
        let manifests = self.manifests_of(files.iter().filter_map(|(path, _)| path.as_str()));
        let entries = manifest_entries(&manifests, files.iter().map(|(path, _)| *path));
        let entry_set: AHashSet<&RelPath> = entries.iter().map(|(path, _)| path).collect();
        let configured = configured_entries(&self.config.analysis.entry_points, files.iter().map(|(path, _)| *path))?;
        let surfaces: Vec<RelPath> = entries
            .iter()
            .filter(|(_, surface)| *surface)
            .map(|(path, _)| path.clone())
            .collect();
        let surface = surface_files(&graph, &surfaces);
        let cargo_libs = cargo_library_dirs(&manifests);
        let in_surface = |path: &RelPath, language: &str| {
            surface.contains(path) || (language == "rust" && in_cargo_library(path, &cargo_libs))
        };
        let view = self.store_read().read_index_view();
        let mut notes = notes(&graph);

        let mut candidates: Vec<Candidate> = Vec::new();
        if request.wants("symbols") {
            cache.for_each(|path, l1| {
                let Some(language) = cache.language_of(path) else {
                    return;
                };
                if SYMBOL_LANGUAGES.contains(&language)
                    && request.in_scope(path)
                    && !path.is_external()
                    && !is_test_data(path)
                {
                    candidates_of(path, language, l1, &mut candidates);
                }
            });
        }
        let mut names: AHashMap<&str, u32> = AHashMap::new();
        for candidate in &candidates {
            let next = u32::try_from(names.len()).unwrap_or(u32::MAX);
            names.entry(candidate.name.as_str()).or_insert(next);
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
        let mut by_file: AHashMap<&RelPath, Vec<usize>> = AHashMap::new();
        for (at, candidate) in candidates.iter().enumerate() {
            by_file.entry(&candidate.path).or_default().push(at);
        }
        // Without candidate names, only the files whose entry conventions read their text, or whose
        // unresolved imports need a line, are read.
        let scan_files: Vec<ScanFile<'_>> = files
            .iter()
            .filter(|(path, meta)| {
                !names.is_empty()
                    || matches!(&*meta.language, "go" | "python" | "java")
                    || specifiers.contains_key(path)
            })
            .map(|&(path, meta)| ScanFile {
                path,
                meta,
                candidates: by_file.remove(path).unwrap_or_default(),
                specifiers: specifiers.get(path).copied().unwrap_or_default(),
            })
            .collect();
        let sweep = Sweep::new(IndexedFiles::of(self));
        let scan = text_pass(&sweep, &scan_files, &candidates, &names);
        drop(scan_files);

        let dead_files = request.wants("files").then(|| {
            let judged: Vec<Judged<'_>> = files
                .iter()
                .filter(|(path, meta)| judged_language(&graph, &meta.language, path, &manifests))
                .map(|&(path, meta)| {
                    let facts = scan.facts.get(path);
                    let entry = if entry_set.contains(path) {
                        Some("a manifest target")
                    } else if configured.contains(path) {
                        Some("listed in `[analysis] entry_points`")
                    } else {
                        conventional_entry(
                            &path.to_str_lossy(),
                            &meta.language,
                            facts.unwrap_or(&FileFacts::default()),
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
            cap_partial_graph(&mut found, graph.probes_complete());
            found
        });

        let dead_symbols = request.wants("symbols").then(|| {
            let read: AHashSet<&RelPath> = scan.facts.keys().collect();
            let defs = definitions(&cache, &read, &names);
            let others: Vec<[i64; 3]> = scan
                .occurrences
                .iter()
                .zip(&defs)
                .map(|(&[code, strings, comments], &defined)| {
                    [i64::from(code) - i64::from(defined), i64::from(strings), i64::from(comments)]
                })
                .collect();
            let judge_with = Judge {
                surface: &in_surface,
                others: &others,
            };
            let mut stale = 0usize;
            let found: Vec<DeadSymbol> = candidates
                .iter()
                .enumerate()
                .filter_map(|(at, candidate)| {
                    let Some(local) = scan.locals.get(&at) else {
                        stale += 1;
                        return None;
                    };
                    let facts = scan.facts.get(&candidate.path);
                    judge(candidate, local, facts, names[candidate.name.as_str()], &judge_with)
                })
                .collect();
            if stale > 0 {
                notes.push(format!(
                    "{stale} definition(s) in files changed or removed since the scan were not judged: run `basemind scan`"
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
                &mut notes,
            )
        });

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
        Ok(DeadcodeResponse {
            counts,
            files: files_out,
            symbols: symbols_out,
            imports: imports_out,
            omitted,
            entry_points: entry_set.len() + configured.iter().filter(|path| !entry_set.contains(*path)).count(),
            coverage: language_coverage(&graph, &files),
            graph: coverage(&graph),
            notes,
            notice: self.lifecycle_notice(),
            elapsed_us: elapsed_us(started),
        })
    }
}

/// The files `[analysis] entry_points` names. The globs were checked when the config loaded, so a
/// failure here is a config the loader did not validate.
fn configured_entries<'a>(
    globs: &[String],
    files: impl Iterator<Item = &'a RelPath>,
) -> Result<AHashSet<&'a RelPath>, EngineError> {
    if globs.is_empty() {
        return Ok(AHashSet::new());
    }
    let set = crate::config::rules::compile_patterns(globs.iter().map(String::as_str)).map_err(|error| {
        EngineError::invalid_argument(format!("invalid glob in `[analysis] entry_points`: {error}"))
    })?;
    Ok(files.filter(|path| set.is_match(&*path.to_str_lossy())).collect())
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
fn judged_language(graph: &ImportGraph, language: &str, path: &RelPath, manifests: &[Manifest]) -> bool {
    if !FILE_LANGUAGES.contains(&language)
        || !graph
            .coverage()
            .languages
            .iter()
            .any(|known| known.language == language && known.resolver)
    {
        return false;
    }
    let anchor = match language {
        "rust" => Ecosystem::Cargo,
        "go" => Ecosystem::Go,
        _ => return true,
    };
    let path = path.to_str_lossy();
    manifests.iter().any(|manifest| {
        manifest.ecosystem == anchor && {
            let dir = parent_dir(&manifest.path.to_str_lossy()).to_string();
            dir.is_empty() || path.starts_with(&format!("{dir}/"))
        }
    })
}

/// The directories of the Cargo packages with a library target.
fn cargo_library_dirs(manifests: &[Manifest]) -> Vec<String> {
    manifests
        .iter()
        .filter(|m| m.ecosystem == Ecosystem::Cargo && m.targets.iter().any(|t| t.kind == TargetKind::Lib))
        .map(|m| parent_dir(&m.path.to_str_lossy()).to_string())
        .collect()
}

/// Whether a Rust file is library code by Cargo's layout: under `src/` of a package with a library,
/// and neither its binary root nor under `src/bin/`. Stands in for the module tree when no Rust
/// resolver is compiled in.
fn in_cargo_library(path: &RelPath, dirs: &[String]) -> bool {
    let path = path.to_str_lossy();
    dirs.iter().any(|dir| {
        let rest = if dir.is_empty() {
            Some(path.as_ref())
        } else {
            path.strip_prefix(dir.as_str()).and_then(|rest| rest.strip_prefix('/'))
        };
        rest.and_then(|rest| rest.strip_prefix("src/"))
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
        for (at, specifier) in edges.unresolved_specifiers.iter().enumerate() {
            if names_unindexed_file(root, path, specifier) {
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
            "{unindexed} unresolved import(s) name a file the index does not hold (a stylesheet, an asset) and are not listed"
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

/// Whether a relative `specifier` of `importer` names a file or directory on disk that the index
/// does not hold (`./styles.module.css`): not a broken import.
fn names_unindexed_file(root: &std::path::Path, importer: &RelPath, specifier: &str) -> bool {
    if !(specifier.starts_with("./") || specifier.starts_with("../")) {
        return false;
    }
    let importer = importer.to_str_lossy();
    let mut parts: Vec<&str> = parent_dir(&importer).split('/').filter(|p| !p.is_empty()).collect();
    for part in specifier.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return false;
                }
            }
            part => parts.push(part),
        }
    }
    let target = root.join(parts.join("/"));
    target.starts_with(root) && std::fs::symlink_metadata(&target).is_ok()
}

/// How sure an unresolved import is to be broken, by what its language's resolver can see.
pub(super) fn import_tier(language: &str, specifier: &str) -> (Tier, &'static str) {
    match language {
        "javascript" | "typescript" | "tsx" if specifier.starts_with('.') || specifier.starts_with('/') => {
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
            "`include!` and macro-generated modules are not followed; a `pub` item of a library crate counts as exported"
        }
        "go" => "build tags are ignored; a capitalised name outside `package main` counts as exported",
        "python" => {
            "modules loaded by name are not seen; a public module-level name is at most `low` unless `__all__` leaves it out"
        }
        "javascript" | "typescript" | "tsx" => {
            "dynamic `import()` and bundler entries are not seen; an `export` reached from a package's `main` or `exports` counts as exported"
        }
        "java" => "classes of one package use each other without an import, so a file finding is at most `low`",
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
