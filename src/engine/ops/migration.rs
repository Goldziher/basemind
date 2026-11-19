//! `basemind code migration check` (#130, ADR-0047 decision 1): evaluates the rules of
//! `.basemind/migrations/<name>.toml` against the index and records the counts in the migration's
//! history, so progress across checks is visible.
//!
//! The unit everywhere is a LINE of a file: a line holding two old forms, or one form two patterns
//! name (a `call` and a `text`), counts once. Each pattern reads what the index holds for it:
//!
//! - `call`: the file's call sites (`calls_by_callee` rows of its content);
//! - `import`: the file's L1 imports, placed on a line through the file's bytes;
//! - `identifier`: the file's L1 definitions and its call sites;
//! - `text`: the file's lines, matched as `code_grep` matches them.
//!
//! Only code files of the index are checked, and never the rule files under `.basemind/`.

use std::sync::Arc;
use std::time::Instant;

use rayon::prelude::*;

use crate::engine::error::EngineError;
use crate::engine::files::{IndexedFiles, Sweep};
use crate::engine::l1_cache::FileMeta;
use crate::engine::migration_history as history;
use crate::engine::migration_rules::{self, Migration, Pattern, Rule, Target};
use crate::engine::ops::code_grep::line_matches;
use crate::engine::ranges::{RangeOptions, format_spans, merge_hits};
use crate::engine::refs::calls_scan::{CallRef, calls_in_content_fjall};
use crate::engine::timing::elapsed_us;
use crate::engine::types::migration::{
    FILES_MAX, FileUses, LINES_MAX, MIXED_FILES_MAX, MigrationCheckParams, MigrationCheckResponse, MigrationReport,
    Progress, RECENT_RUNS, RuleReport, RunPoint,
};
use crate::engine::workspace::Workspace;
use crate::index::view::IndexView;
use crate::path::RelPath;

impl Workspace {
    /// `basemind code migration check [<name>]`: the remaining old forms of each rule, per file.
    pub(crate) fn code_migration_check(&self, p: MigrationCheckParams) -> Result<MigrationCheckResponse, EngineError> {
        let started = Instant::now();
        let migrations = migration_rules::load(&self.root, p.name.as_deref())?;
        self.wait_map_ready();
        let cache = self.cache.load_full();
        let (idx, cache_dir) = {
            let store = self.store_read();
            (store.read_index_view(), store.basemind_dir.clone())
        };
        let needs_calls = migrations
            .iter()
            .flat_map(|m| &m.rules)
            .any(|rule| rule.uses(Target::Call) || rule.uses(Target::Identifier));
        if needs_calls && idx.is_none() {
            return Err(EngineError::index_unavailable(
                "`code migration check`: a `call` or `identifier` rule reads the index, and this session holds \
                 none. Run `basemind scan`, or start the daemon with `basemind comms start`.",
            ));
        }
        let files: Vec<(&RelPath, &FileMeta)> = cache
            .file_metas()
            .filter(|(path, _)| !path.is_external() && !path.as_bytes().starts_with(b".basemind/"))
            .collect();
        let context = Context {
            cache: &cache,
            idx: idx.as_deref(),
            live: idx.as_ref().map(|idx| idx.live_keys()),
            sweep: Sweep::new(IndexedFiles::of(self)),
            ranges: p.ranges.unwrap_or(false),
        };
        let head = self
            .repo
            .as_ref()
            .and_then(|repo| repo.info().ok())
            .and_then(|info| info.head_short_sha);
        let reports = migrations
            .iter()
            .map(|migration| {
                let mut report = context.check(migration, &files)?;
                report.progress = progress(&cache_dir, &report, head.clone());
                Ok(report)
            })
            .collect::<Result<Vec<_>, EngineError>>()?;
        Ok(MigrationCheckResponse {
            migrations: reports,
            notice: self.lifecycle_notice(),
            elapsed_us: elapsed_us(started),
        })
    }
}

/// What one check reads from.
struct Context<'a> {
    cache: &'a Arc<crate::engine::MapCache>,
    idx: Option<&'a dyn IndexView>,
    /// The content key each live path's rows are under, read once for the whole check.
    live: Option<Arc<crate::index::live_keys::LiveKeys>>,
    sweep: Sweep<'a>,
    ranges: bool,
}

/// The lines of one file that one rule's patterns hit.
#[derive(Default)]
struct RuleHits {
    /// Lines holding an old form, ascending and distinct.
    old: Vec<u32>,
    /// Lines holding a new form, ascending and distinct.
    new: Vec<u32>,
    /// A `text` or `import` pattern could not read the file.
    unreadable: bool,
}

/// What the patterns of a file's rules need of it.
#[derive(Default)]
struct FileFacts {
    calls: Vec<CallRef>,
    outline: Option<Arc<crate::extract::FileMapL1>>,
    /// The file's text, when a pattern needs it and it is readable UTF-8.
    text: Option<String>,
    /// Byte offset of each line's start.
    line_starts: Vec<usize>,
}

impl FileFacts {
    /// The 1-based line holding byte `offset`.
    fn line_of(&self, offset: usize) -> u32 {
        let line = self.line_starts.partition_point(|start| *start <= offset);
        u32::try_from(line.max(1)).unwrap_or(u32::MAX)
    }
}

impl Context<'_> {
    fn check(&self, migration: &Migration, files: &[(&RelPath, &FileMeta)]) -> Result<MigrationReport, EngineError> {
        let per_file: Vec<(&RelPath, Vec<Option<RuleHits>>)> = files
            .par_iter()
            .map(|(path, _)| self.file_hits(migration, path).map(|hits| (*path, hits)))
            .collect::<Result<_, EngineError>>()?;

        let rules: Vec<RuleReport> = migration
            .rules
            .iter()
            .enumerate()
            .map(|(at, rule)| self.rule_report(rule, per_file.iter().map(|(path, hits)| (*path, hits[at].as_ref()))))
            .collect();
        let remaining = rules.iter().fold(0u32, |sum, r| sum.saturating_add(r.remaining));
        let expected = rules
            .iter()
            .filter_map(|r| r.expected)
            .fold(0u32, |sum, n| sum.saturating_add(n));
        let failed_rules = u32::try_from(rules.iter().filter(|r| r.failed).count()).unwrap_or(u32::MAX);
        Ok(MigrationReport {
            name: migration.name.clone(),
            description: migration.description.clone(),
            remaining,
            expected,
            failed_rules,
            rules,
            progress: Progress {
                runs: 0,
                first: None,
                previous: None,
                recent: Vec::new(),
                history_file: None,
            },
        })
    }

    /// Per rule of `migration`, its hits in `path`; `None` for a rule that does not select it.
    fn file_hits(&self, migration: &Migration, path: &RelPath) -> Result<Vec<Option<RuleHits>>, EngineError> {
        let shown = path.to_str_lossy();
        let selecting: Vec<Option<&Rule>> = migration
            .rules
            .iter()
            .map(|rule| rule.selects(&shown).then_some(rule))
            .collect();
        if selecting.iter().all(Option::is_none) {
            return Ok(selecting.iter().map(|_| None).collect());
        }
        let uses = |target| selecting.iter().flatten().any(|rule| rule.uses(target));
        let mut facts = FileFacts::default();
        if (uses(Target::Call) || uses(Target::Identifier))
            && let (Some(idx), Some(live)) = (self.idx, &self.live)
        {
            facts.calls = calls_in_content_fjall(idx, live.key_of(path))?;
        }
        if uses(Target::Import) || uses(Target::Identifier) {
            facts.outline = self.cache.get(path);
        }
        if selecting.iter().flatten().any(|rule| rule.reads_text()) {
            facts.text = self.read_text(path);
            if let Some(text) = &facts.text {
                facts.line_starts = std::iter::once(0)
                    .chain(memchr::memchr_iter(b'\n', text.as_bytes()).map(|at| at + 1))
                    .collect();
            }
        }
        Ok(selecting
            .into_iter()
            .map(|rule| {
                rule.map(|rule| {
                    let needs_text = rule.reads_text();
                    RuleHits {
                        old: lines_of(&rule.matches, &facts),
                        new: lines_of(&rule.expect, &facts),
                        unreadable: needs_text && facts.text.is_none(),
                    }
                })
            })
            .collect())
    }

    /// The file's text, or `None` when it is gone, left the repository, grew past the scan's cap or
    /// is not UTF-8.
    fn read_text(&self, path: &RelPath) -> Option<String> {
        let file = self.sweep.path(path)?;
        let bytes = crate::engine::freshness::read_bounded(&file, self.sweep.max_file_bytes()).ok()??;
        String::from_utf8(bytes).ok()
    }

    fn rule_report<'p>(
        &self,
        rule: &Rule,
        hits: impl Iterator<Item = (&'p RelPath, Option<&'p RuleHits>)>,
    ) -> RuleReport {
        let mut files_checked = 0u32;
        let mut skipped = 0u32;
        let (mut remaining, mut expected, mut expected_files) = (0u32, 0u32, 0u32);
        let mut with_old: Vec<(&RelPath, &[u32])> = Vec::new();
        let mut mixed: Vec<RelPath> = Vec::new();
        for (path, hits) in hits {
            let Some(hits) = hits else { continue };
            files_checked += 1;
            skipped += u32::from(hits.unreadable);
            let (old, new) = (count(&hits.old), count(&hits.new));
            remaining = remaining.saturating_add(old);
            expected = expected.saturating_add(new);
            expected_files += u32::from(new > 0);
            if old > 0 {
                with_old.push((path, &hits.old));
                if new > 0 {
                    mixed.push(path.clone());
                }
            }
        }
        let remaining_files = u32::try_from(with_old.len()).unwrap_or(u32::MAX);
        with_old.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
        let omitted_files = u32::try_from(with_old.len().saturating_sub(FILES_MAX)).unwrap_or(u32::MAX);
        with_old.truncate(FILES_MAX);
        let ranges = self.ranges.then(|| {
            merge_hits(
                with_old
                    .iter()
                    .flat_map(|(path, lines)| lines.iter().map(|line| ((*path).clone(), *line))),
                |path| self.cache.meta(path).map(|meta| meta.lines).filter(|lines| *lines > 0),
                RangeOptions::default(),
            )
        });
        let files = with_old
            .iter()
            .map(|(path, lines)| FileUses {
                path: (*path).clone(),
                count: count(lines),
                lines: lines.iter().take(LINES_MAX).copied().collect(),
                ranges: ranges.as_ref().map(|merged| {
                    let spans: Vec<_> = merged
                        .files
                        .iter()
                        .find(|file| file.path == **path)
                        .map(|file| file.ranges.iter().map(|range| range.span).collect())
                        .unwrap_or_default();
                    format_spans(&spans)
                }),
            })
            .collect();
        mixed.sort_unstable();
        mixed.truncate(MIXED_FILES_MAX);
        let declares_expect = !rule.expect.is_empty();
        let total = u64::from(expected) + u64::from(remaining);
        RuleReport {
            name: rule.name.clone(),
            guidance: rule.guidance.clone(),
            files_checked,
            skipped_files: skipped,
            remaining,
            remaining_files,
            expected: declares_expect.then_some(expected),
            expected_files: declares_expect.then_some(expected_files),
            migrated_share: (declares_expect && total > 0)
                .then(|| (f64::from(expected) / total as f64 * 1000.0).round() / 1000.0),
            failed: declares_expect && expected == 0,
            mixed_files: mixed,
            files,
            omitted_files,
        }
    }
}

fn count(lines: &[u32]) -> u32 {
    u32::try_from(lines.len()).unwrap_or(u32::MAX)
}

/// The lines of the file's call sites `pattern` matches.
fn call_lines<'f>(pattern: &'f Pattern, facts: &'f FileFacts) -> impl Iterator<Item = u32> + 'f {
    facts
        .calls
        .iter()
        .filter(|call| {
            let source = facts
                .text
                .as_deref()
                .and_then(|text| text.get(call.start_byte as usize..));
            pattern.matches_call(&call.callee, source)
        })
        .map(|call| call.line)
}

/// The distinct lines of a file any of `patterns` hits, ascending.
fn lines_of(patterns: &[Pattern], facts: &FileFacts) -> Vec<u32> {
    let mut lines: Vec<u32> = Vec::new();
    for pattern in patterns {
        match pattern.target {
            Target::Call => lines.extend(call_lines(pattern, facts)),
            Target::Identifier => {
                lines.extend(call_lines(pattern, facts));
                if let Some(outline) = &facts.outline {
                    lines.extend(
                        outline
                            .symbols
                            .iter()
                            .filter(|s| pattern.matches_name(&s.name))
                            .map(|s| s.start_row.saturating_add(1)),
                    );
                }
            }
            Target::Import => {
                if let (Some(outline), Some(_)) = (&facts.outline, &facts.text) {
                    lines.extend(
                        outline
                            .imports
                            .iter()
                            .filter(|i| pattern.matches_import(i.module.as_deref(), &i.raw))
                            .map(|i| facts.line_of(i.start_byte as usize)),
                    );
                }
            }
            Target::Text => {
                if let (Some(text), Some(re)) = (&facts.text, pattern.line_regex()) {
                    lines.extend(line_matches(re, text).map(|found| facts.line_of(found.start())));
                }
            }
        }
    }
    lines.retain(|line| *line > 0);
    lines.sort_unstable();
    lines.dedup();
    lines
}

/// Records this run in the migration's history and reports it against the runs before it.
fn progress(cache_dir: &std::path::Path, report: &MigrationReport, head: Option<String>) -> Progress {
    let file = history::file(cache_dir, &report.name);
    let recorded = history::read(&file, RECENT_RUNS.saturating_sub(1));
    let now = RunPoint {
        at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
        head,
        remaining: report.remaining,
        expected: report.expected,
    };
    let written = match history::append(&file, &now) {
        Ok(()) => true,
        Err(error) => {
            tracing::debug!(path = %file.display(), %error, "migration history not written");
            false
        }
    };
    let previous = recorded.recent.last().cloned();
    let mut recent = recorded.recent;
    recent.push(now);
    Progress {
        runs: recorded.runs.saturating_add(1),
        first: recorded.first,
        previous,
        recent,
        history_file: written.then(|| file.display().to_string()),
    }
}

#[cfg(test)]
#[path = "migration_tests.rs"]
mod tests;
